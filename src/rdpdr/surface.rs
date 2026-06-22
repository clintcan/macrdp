//! macOS NFS-mount surface for RDPDR drive redirection (Phase 2).
//!
//! Replaces the Phase-1c temp-folder/`NSFilePresenter` mirror with a **real
//! volume**: an in-process NFSv3 server ([`nfsserve`]) exposes the client's
//! redirected drive, and macOS's built-in NFS client mounts it at
//! `/Volumes/<label>`. The OS then routes every `stat`/`readdir`/`read` to our
//! [`RdpdrFs`], which translates them into RDPDR `list_dir` / `read_file` calls
//! over the [`RdpdrHandle`]. The win over the temp-folder surface: real
//! subdirectory navigation (the kernel drives lookups lazily as the user
//! browses), a proper Finder volume in the sidebar, and no per-file placeholder
//! bookkeeping — the OS's VFS layer is the cache.
//!
//! No kext / FUSE / app-extension: the mount uses only the built-in `mount_nfs`
//! against `localhost`, and (verified) needs **no root** for a loopback NFS
//! mount onto a user-created mountpoint. **Read-write:** writes map to RDPDR
//! `DeviceWrite` (`write`), set-information (`set_len`/`remove`/`rename`), and
//! create (`create`/`mkdir`).
//!
//! Cleaned up (unmount + remove mountpoint + stop the server) when the
//! connection's backend drops.

#![cfg(target_os = "macos")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use ironrdp_server::{DirEntry as RdpEntry, RdpdrHandle, RdpdrStatus};
use nfsserve::nfs::{
    fattr3, fileid3, filename3, ftype3, nfspath3, nfsstat3, nfsstring, nfstime3, sattr3, set_size3,
};
use nfsserve::tcp::{NFSTcp, NFSTcpListener};
use nfsserve::vfs::{DirEntry as NfsDirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Path/fileid cache
// ---------------------------------------------------------------------------

/// The root directory's NFS fileid (1 by NFS convention).
const ROOT_ID: fileid3 = 1;

/// Delay the `mount_nfs` + Finder open after a device is announced, to keep them
/// off the connect-time critical path. With `--virtual-display` +
/// `--detach-primary`/`--capture-primary`, mounting a volume and opening Finder
/// the instant the client connects collides with WindowServer reflowing the
/// desktop onto the virtual display — the desktop renders half-blank and app
/// windows never migrate. Deferring past that window (plus opening Finder in the
/// background, `open -g`, so it can't steal focus mid-reflow) avoids it; the cost
/// is only that the redirected drive appears a few seconds after connect.
const MOUNT_DEFER: Duration = Duration::from_secs(4);

/// One known node in the redirected tree.
#[derive(Clone)]
struct Node {
    /// Windows backslash path relative to the drive root (`\` is the root).
    remote_path: String,
    is_dir: bool,
    size: u64,
    /// Parent's fileid, for `..` resolution (root is its own parent).
    parent: fileid3,
}

/// Bidirectional path↔fileid map. NFS addresses everything by an opaque u64
/// fileid; RDPDR addresses by path — this interns each path we encounter to a
/// stable id and remembers its metadata so `getattr` needn't re-stat.
struct FsCache {
    next_id: fileid3,
    id_to_node: HashMap<fileid3, Node>,
    path_to_id: HashMap<String, fileid3>,
}

impl FsCache {
    fn new() -> Self {
        let mut id_to_node = HashMap::new();
        let mut path_to_id = HashMap::new();
        id_to_node.insert(
            ROOT_ID,
            Node {
                remote_path: "\\".to_owned(),
                is_dir: true,
                size: 0,
                parent: ROOT_ID,
            },
        );
        path_to_id.insert("\\".to_owned(), ROOT_ID);
        Self {
            next_id: ROOT_ID + 1,
            id_to_node,
            path_to_id,
        }
    }

    /// Intern `path` (a child of `parent`), returning its stable fileid. Refreshes
    /// the cached size/kind if the path is already known.
    fn intern(&mut self, parent: fileid3, path: &str, is_dir: bool, size: u64) -> fileid3 {
        if let Some(&id) = self.path_to_id.get(path) {
            if let Some(n) = self.id_to_node.get_mut(&id) {
                n.is_dir = is_dir;
                n.size = size;
            }
            return id;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.id_to_node.insert(
            id,
            Node {
                remote_path: path.to_owned(),
                is_dir,
                size,
                parent,
            },
        );
        self.path_to_id.insert(path.to_owned(), id);
        id
    }

    /// Drop a node from the cache (used after a successful delete/rename of the
    /// old path).
    fn forget(&mut self, id: fileid3) {
        if let Some(n) = self.id_to_node.remove(&id) {
            self.path_to_id.remove(&n.remote_path);
        }
    }

    /// Update a node's cached size (after a write/truncate).
    fn set_size(&mut self, id: fileid3, size: u64) {
        if let Some(n) = self.id_to_node.get_mut(&id) {
            n.size = size;
        }
    }
}

/// Join a child `name` onto a Windows backslash `dir` path.
fn join_remote(dir: &str, name: &str) -> String {
    if dir == "\\" {
        format!("\\{name}")
    } else {
        format!("{dir}\\{name}")
    }
}

/// Map an `RdpdrHandle` error to the closest NFS status, logging the cause. When
/// the client returned a concrete [`NtStatus`] (e.g. `ACCESS_DENIED` for a write
/// to a protected location like `C:\`), translate it so Finder reports the right
/// thing ("you don't have permission") instead of a generic I/O error. These are
/// routine client-side outcomes, so they log at debug, not warn.
fn nfs_err(context: &str, e: &anyhow::Error) -> nfsstat3 {
    let status = e.downcast_ref::<RdpdrStatus>().map(|s| s.status);
    let mapped = super::ntstatus_to_nfsstat3(status);
    debug!(error = %e, "rdpdr nfs: {context}");
    mapped
}

// ---------------------------------------------------------------------------
// RdpdrFs — the NFS filesystem backed by the client's redirected drive
// ---------------------------------------------------------------------------

/// Read-write NFS filesystem over a redirected RDPDR drive. Every NFS op the
/// kernel issues is satisfied from the cache or via an RDPDR round-trip.
pub struct RdpdrFs {
    handle: RdpdrHandle,
    device_id: u32,
    /// A fixed timestamp (server start) reported for every node; the surface
    /// doesn't model per-file times, so it only feeds the client's attr cache.
    created_secs: u32,
    cache: Mutex<FsCache>,
}

impl RdpdrFs {
    fn new(handle: RdpdrHandle, device_id: u32) -> Self {
        let created_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as u32)
            .unwrap_or(0);
        Self {
            handle,
            device_id,
            created_secs,
            cache: Mutex::new(FsCache::new()),
        }
    }

    fn attr(&self, id: fileid3, is_dir: bool, size: u64) -> fattr3 {
        let t = nfstime3 {
            seconds: self.created_secs,
            nseconds: 0,
        };
        if is_dir {
            fattr3 {
                ftype: ftype3::NF3DIR,
                mode: 0o755,
                nlink: 2,
                size: 4096,
                used: 4096,
                fileid: id,
                atime: t,
                mtime: t,
                ctime: t,
                ..Default::default()
            }
        } else {
            fattr3 {
                ftype: ftype3::NF3REG,
                mode: 0o644,
                nlink: 1,
                size,
                used: size,
                fileid: id,
                atime: t,
                mtime: t,
                ctime: t,
                ..Default::default()
            }
        }
    }

    /// List `dirid`'s children via RDPDR and intern each into the cache. Returns
    /// `(fileid, entry)` pairs. The cache lock is never held across the await.
    async fn list_and_cache(&self, dirid: fileid3) -> Result<Vec<(fileid3, RdpEntry)>, nfsstat3> {
        let dir_path = {
            let c = self.cache.lock().unwrap();
            let n = c.id_to_node.get(&dirid).ok_or(nfsstat3::NFS3ERR_NOENT)?;
            if !n.is_dir {
                return Err(nfsstat3::NFS3ERR_NOTDIR);
            }
            n.remote_path.clone()
        };
        let entries = self
            .handle
            .list_dir(self.device_id, &dir_path)
            .await
            .map_err(|e| nfs_err("list_dir failed", &e))?;
        let mut out = Vec::with_capacity(entries.len());
        let mut c = self.cache.lock().unwrap();
        for e in entries {
            let child = join_remote(&dir_path, &e.name);
            let id = c.intern(dirid, &child, e.is_dir, e.size);
            out.push((id, e));
        }
        Ok(out)
    }

    /// Resolve a directory fileid to its remote path (errors if unknown or not a
    /// directory).
    fn dir_path_of(&self, dirid: fileid3) -> Result<String, nfsstat3> {
        let c = self.cache.lock().unwrap();
        let n = c.id_to_node.get(&dirid).ok_or(nfsstat3::NFS3ERR_NOENT)?;
        if !n.is_dir {
            return Err(nfsstat3::NFS3ERR_NOTDIR);
        }
        Ok(n.remote_path.clone())
    }
}

#[async_trait]
impl NFSFileSystem for RdpdrFs {
    fn capabilities(&self) -> VFSCapabilities {
        VFSCapabilities::ReadWrite
    }

    fn root_dir(&self) -> fileid3 {
        ROOT_ID
    }

    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        let name = String::from_utf8_lossy(&filename.0).into_owned();
        if name == "." {
            return Ok(dirid);
        }
        if name == ".." {
            let c = self.cache.lock().unwrap();
            return c
                .id_to_node
                .get(&dirid)
                .map(|n| n.parent)
                .ok_or(nfsstat3::NFS3ERR_NOENT);
        }

        // Fast path: already interned (e.g. from a prior readdir).
        let (dir_path, cached) = {
            let c = self.cache.lock().unwrap();
            let n = c.id_to_node.get(&dirid).ok_or(nfsstat3::NFS3ERR_NOENT)?;
            if !n.is_dir {
                return Err(nfsstat3::NFS3ERR_NOTDIR);
            }
            let child = join_remote(&n.remote_path, &name);
            (child.clone(), c.path_to_id.get(&child).copied())
        };
        if let Some(id) = cached {
            return Ok(id);
        }

        // Slow path: enumerate the parent to discover the child.
        self.list_and_cache(dirid).await?;
        self.cache
            .lock()
            .unwrap()
            .path_to_id
            .get(&dir_path)
            .copied()
            .ok_or(nfsstat3::NFS3ERR_NOENT)
    }

    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        let (is_dir, size) = {
            let c = self.cache.lock().unwrap();
            let n = c.id_to_node.get(&id).ok_or(nfsstat3::NFS3ERR_NOENT)?;
            (n.is_dir, n.size)
        };
        Ok(self.attr(id, is_dir, size))
    }

    async fn read(
        &self,
        id: fileid3,
        offset: u64,
        count: u32,
    ) -> Result<(Vec<u8>, bool), nfsstat3> {
        let (remote_path, is_dir, size) = {
            let c = self.cache.lock().unwrap();
            let n = c.id_to_node.get(&id).ok_or(nfsstat3::NFS3ERR_NOENT)?;
            (n.remote_path.clone(), n.is_dir, n.size)
        };
        if is_dir {
            return Err(nfsstat3::NFS3ERR_ISDIR);
        }
        if offset >= size {
            return Ok((Vec::new(), true));
        }
        let data = self
            .handle
            .read_file(self.device_id, &remote_path, offset, count)
            .await
            .map_err(|e| nfs_err("read_file failed", &e))?;
        let eof = offset + data.len() as u64 >= size;
        Ok((data, eof))
    }

    async fn readdir(
        &self,
        dirid: fileid3,
        start_after: fileid3,
        max_entries: usize,
    ) -> Result<ReadDirResult, nfsstat3> {
        let mut kids = self.list_and_cache(dirid).await?;
        // Stable canonical order so paginated calls (start_after = last fileid)
        // stay consistent even if the client re-orders successive listings.
        kids.sort_by_key(|(id, _)| *id);

        let start_idx = if start_after == 0 {
            0
        } else {
            kids.iter()
                .position(|(id, _)| *id == start_after)
                .map(|p| p + 1)
                .unwrap_or(0)
        };

        let slice = &kids[start_idx.min(kids.len())..];
        let mut entries = Vec::new();
        for (id, e) in slice.iter().take(max_entries) {
            entries.push(NfsDirEntry {
                fileid: *id,
                name: nfsstring(e.name.clone().into_bytes()),
                attr: self.attr(*id, e.is_dir, e.size),
            });
        }
        let end = start_idx + entries.len() >= kids.len();
        Ok(ReadDirResult { entries, end })
    }

    async fn setattr(&self, id: fileid3, setattr: sattr3) -> Result<fattr3, nfsstat3> {
        let (remote_path, is_dir) = {
            let c = self.cache.lock().unwrap();
            let n = c.id_to_node.get(&id).ok_or(nfsstat3::NFS3ERR_NOENT)?;
            (n.remote_path.clone(), n.is_dir)
        };
        // Only a size change (truncate/extend) maps to RDPDR; mode/uid/gid/times
        // are accepted as no-ops since we don't model them.
        if let set_size3::size(new_size) = setattr.size {
            if !is_dir {
                self.handle
                    .set_len(self.device_id, &remote_path, new_size)
                    .await
                    .map_err(|e| nfs_err("set_len failed", &e))?;
                self.cache.lock().unwrap().set_size(id, new_size);
            }
        }
        let size = {
            let c = self.cache.lock().unwrap();
            c.id_to_node.get(&id).map(|n| n.size).unwrap_or(0)
        };
        Ok(self.attr(id, is_dir, size))
    }

    async fn write(&self, id: fileid3, offset: u64, data: &[u8]) -> Result<fattr3, nfsstat3> {
        let (remote_path, is_dir) = {
            let c = self.cache.lock().unwrap();
            let n = c.id_to_node.get(&id).ok_or(nfsstat3::NFS3ERR_NOENT)?;
            (n.remote_path.clone(), n.is_dir)
        };
        if is_dir {
            return Err(nfsstat3::NFS3ERR_ISDIR);
        }
        self.handle
            .write_file(self.device_id, &remote_path, offset, data)
            .await
            .map_err(|e| nfs_err("write_file failed", &e))?;
        let new_size = {
            let mut c = self.cache.lock().unwrap();
            let cur = c.id_to_node.get(&id).map(|n| n.size).unwrap_or(0);
            let ns = cur.max(offset + data.len() as u64);
            c.set_size(id, ns);
            ns
        };
        Ok(self.attr(id, false, new_size))
    }

    async fn create(
        &self,
        dirid: fileid3,
        filename: &filename3,
        _attr: sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let dir_path = self.dir_path_of(dirid)?;
        let name = String::from_utf8_lossy(&filename.0).into_owned();
        let child = join_remote(&dir_path, &name);
        self.handle
            .create_file(self.device_id, &child, false)
            .await
            .map_err(|e| nfs_err("create_file failed", &e))?;
        let id = self.cache.lock().unwrap().intern(dirid, &child, false, 0);
        Ok((id, self.attr(id, false, 0)))
    }

    async fn create_exclusive(
        &self,
        dirid: fileid3,
        filename: &filename3,
    ) -> Result<fileid3, nfsstat3> {
        let dir_path = self.dir_path_of(dirid)?;
        let name = String::from_utf8_lossy(&filename.0).into_owned();
        let child = join_remote(&dir_path, &name);
        self.handle
            .create_file(self.device_id, &child, true)
            .await
            .map_err(|e| nfs_err("create_exclusive failed", &e))?;
        Ok(self.cache.lock().unwrap().intern(dirid, &child, false, 0))
    }

    async fn mkdir(
        &self,
        dirid: fileid3,
        dirname: &filename3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let dir_path = self.dir_path_of(dirid)?;
        let name = String::from_utf8_lossy(&dirname.0).into_owned();
        let child = join_remote(&dir_path, &name);
        self.handle
            .create_dir(self.device_id, &child)
            .await
            .map_err(|e| nfs_err("create_dir failed", &e))?;
        let id = self.cache.lock().unwrap().intern(dirid, &child, true, 0);
        Ok((id, self.attr(id, true, 0)))
    }

    async fn remove(&self, dirid: fileid3, filename: &filename3) -> Result<(), nfsstat3> {
        let dir_path = self.dir_path_of(dirid)?;
        let name = String::from_utf8_lossy(&filename.0).into_owned();
        let child = join_remote(&dir_path, &name);
        // Resolve the target's id + kind; enumerate the parent if not yet cached.
        let mut resolved = {
            let c = self.cache.lock().unwrap();
            c.path_to_id
                .get(&child)
                .copied()
                .and_then(|id| c.id_to_node.get(&id).map(|n| (id, n.is_dir)))
        };
        if resolved.is_none() {
            self.list_and_cache(dirid).await?;
            resolved = {
                let c = self.cache.lock().unwrap();
                c.path_to_id
                    .get(&child)
                    .copied()
                    .and_then(|id| c.id_to_node.get(&id).map(|n| (id, n.is_dir)))
            };
        }
        let (id, is_dir) = resolved.ok_or(nfsstat3::NFS3ERR_NOENT)?;
        self.handle
            .remove(self.device_id, &child, is_dir)
            .await
            .map_err(|e| nfs_err("remove failed", &e))?;
        self.cache.lock().unwrap().forget(id);
        Ok(())
    }

    async fn rename(
        &self,
        from_dirid: fileid3,
        from_filename: &filename3,
        to_dirid: fileid3,
        to_filename: &filename3,
    ) -> Result<(), nfsstat3> {
        let from_dir = self.dir_path_of(from_dirid)?;
        let to_dir = self.dir_path_of(to_dirid)?;
        let from_name = String::from_utf8_lossy(&from_filename.0).into_owned();
        let to_name = String::from_utf8_lossy(&to_filename.0).into_owned();
        let from_path = join_remote(&from_dir, &from_name);
        let to_path = join_remote(&to_dir, &to_name);

        let mut src = {
            let c = self.cache.lock().unwrap();
            c.path_to_id
                .get(&from_path)
                .copied()
                .and_then(|id| c.id_to_node.get(&id).map(|n| (id, n.is_dir, n.size)))
        };
        if src.is_none() {
            self.list_and_cache(from_dirid).await?;
            src = {
                let c = self.cache.lock().unwrap();
                c.path_to_id
                    .get(&from_path)
                    .copied()
                    .and_then(|id| c.id_to_node.get(&id).map(|n| (id, n.is_dir, n.size)))
            };
        }
        let (src_id, is_dir, size) = src.ok_or(nfsstat3::NFS3ERR_NOENT)?;
        self.handle
            .rename(self.device_id, &from_path, &to_path, true)
            .await
            .map_err(|e| nfs_err("rename failed", &e))?;
        let mut c = self.cache.lock().unwrap();
        c.forget(src_id);
        c.intern(to_dirid, &to_path, is_dir, size);
        Ok(())
    }

    async fn symlink(
        &self,
        _dirid: fileid3,
        _linkname: &filename3,
        _symlink: &nfspath3,
        _attr: &sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }
    async fn readlink(&self, _id: fileid3) -> Result<nfspath3, nfsstat3> {
        Err(nfsstat3::NFS3ERR_NOTSUPP)
    }
}

// ---------------------------------------------------------------------------
// Surface — the mount lifecycle
// ---------------------------------------------------------------------------

/// A live NFS mount of one redirected drive. Dropping it unmounts the volume,
/// stops the in-process NFS server, and removes the mountpoint.
#[derive(Debug)]
pub struct Surface {
    mountpoint: Option<PathBuf>,
    /// The task running the NFS accept loop; aborting it stops the server.
    serve: Option<JoinHandle<()>>,
}

impl Surface {
    /// Stand up an NFS server for `device_id` (a redirected filesystem labelled
    /// `drive_label`) and mount it (see [`prepare_mountpoint`] for where).
    /// Returns immediately; the bind + mount run on the current tokio runtime.
    /// Must be called from an async (runtime) context.
    pub fn start(handle: RdpdrHandle, device_id: u32, drive_label: &str) -> Self {
        let label = sanitize_label(drive_label);
        let mountpoint = match prepare_mountpoint(&label) {
            Ok(p) => p,
            Err(e) => {
                warn!(label, error = %e, "rdpdr nfs: could not create mountpoint");
                return Self {
                    mountpoint: None,
                    serve: None,
                };
            }
        };

        let mp = mountpoint.clone();
        let serve = tokio::spawn(async move {
            let fs = RdpdrFs::new(handle, device_id);
            let listener = match NFSTcpListener::bind("127.0.0.1:0", fs).await {
                Ok(l) => l,
                Err(e) => {
                    warn!(error = %e, "rdpdr nfs: failed to bind NFS server");
                    return;
                }
            };
            let port = listener.get_listen_port();
            info!(port, mountpoint = ?mp, "rdpdr nfs: server listening");

            // Defer the macOS-side mount + Finder open off the connect-time
            // critical path (see MOUNT_DEFER) so they don't collide with the
            // desktop reflowing onto the virtual display. The sleep is inside
            // this task, so a disconnect during the wait aborts it via
            // Surface::Drop (no orphaned mount). The bind already happened, so
            // mount_nfs's connection queues in the backlog until handle_forever
            // (below) accepts it.
            tokio::time::sleep(MOUNT_DEFER).await;
            let mount_mp = mp.clone();
            tokio::task::spawn_blocking(move || {
                if run_mount(port, &mount_mp) {
                    // Track it so a signal-exit (process::exit skips Drop) can
                    // still unmount it — see shutdown_cleanup().
                    register_mount(&mount_mp);
                    info!(mountpoint = ?mount_mp, "rdpdr nfs: drive mounted — opening in Finder (background)");
                    // `-g`: open the window WITHOUT bringing Finder to the
                    // foreground — foregrounding it mid-reflow steals focus and
                    // breaks the desktop's migration to the virtual display.
                    let _ = std::process::Command::new("/usr/bin/open")
                        .arg("-g")
                        .arg(&mount_mp)
                        .spawn();
                } else {
                    warn!(mountpoint = ?mount_mp, "rdpdr nfs: mount_nfs failed");
                }
            });

            if let Err(e) = listener.handle_forever().await {
                warn!(error = %e, "rdpdr nfs: server loop ended");
            }
        });

        Self {
            mountpoint: Some(mountpoint),
            serve: Some(serve),
        }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        if let Some(h) = self.serve.take() {
            h.abort();
        }
        if let Some(mp) = self.mountpoint.take() {
            // Unmount + remove the mountpoint off the async runtime (umount can
            // block briefly). Detached: cleanup is best-effort on disconnect.
            std::thread::spawn(move || {
                unmount_at(&mp);
                // Already handled here, so shutdown_cleanup needn't touch it.
                unregister_mount(&mp);
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Mount registry — for unmounting on signal-exit, which skips Surface::Drop
// ---------------------------------------------------------------------------

/// Mountpoints currently mounted by live `Surface`s. A `Surface` registers its
/// mountpoint on a successful mount and removes it in `Drop`; [`shutdown_cleanup`]
/// unmounts whatever remains when the process is killed by a signal (where
/// `std::process::exit` skips `Drop`).
static LIVE_MOUNTS: OnceLock<Mutex<Vec<PathBuf>>> = OnceLock::new();

fn live_mounts() -> &'static Mutex<Vec<PathBuf>> {
    LIVE_MOUNTS.get_or_init(|| Mutex::new(Vec::new()))
}

fn register_mount(mp: &Path) {
    live_mounts().lock().unwrap().push(mp.to_path_buf());
}

fn unregister_mount(mp: &Path) {
    live_mounts().lock().unwrap().retain(|p| p != mp);
}

/// `umount -f` the mountpoint, remove it, and prune the empty per-pid parent.
/// Shared by `Surface::Drop` (disconnect) and [`shutdown_cleanup`] (signal exit).
/// `umount` of an already-unmounted path is harmless, so double-calls are safe.
fn unmount_at(mp: &Path) {
    let status = std::process::Command::new("/sbin/umount")
        .arg("-f")
        .arg(mp)
        .status();
    if !matches!(status, Ok(s) if s.success()) {
        debug!(mountpoint = ?mp, ?status, "rdpdr nfs: umount returned nonzero");
    }
    let _ = std::fs::remove_dir(mp);
    // Prune the per-pid parent (only succeeds once empty, so it's safe while
    // other drives are still mounted under it).
    if let Some(parent) = mp.parent() {
        let _ = std::fs::remove_dir(parent);
    }
    debug!(mountpoint = ?mp, "rdpdr nfs: cleaned up mount");
}

/// Unmount every still-live RDPDR NFS volume. Called from the signal handler
/// just before `std::process::exit` (which bypasses `Surface::Drop`), so a
/// SIGINT/SIGTERM stop — Ctrl-C, `kill`, `launchctl bootout`/`kickstart -k` —
/// doesn't strand a mount pointing at the now-dead NFS server. Synchronous and
/// best-effort; runs to completion before exit.
pub fn shutdown_cleanup() {
    let mounts: Vec<PathBuf> = std::mem::take(&mut *live_mounts().lock().unwrap());
    if mounts.is_empty() {
        return;
    }
    info!(
        count = mounts.len(),
        "rdpdr nfs: unmounting volumes on shutdown"
    );
    for mp in mounts {
        unmount_at(&mp);
    }
}

/// Mount the loopback NFS export at `mountpoint`. Read-write, NFSv3 over TCP.
/// Verified to need no root for a `localhost` mount onto a user-owned dir.
///
/// **`rsize`/`wsize` = 256 KiB and `readahead` = 4 are deliberately modest**, not
/// the NFS maximum. RDPDR shares the one RDP TCP connection — and the server's
/// single socket-writer mutex + single-threaded client loop — with the EGFX
/// video and RDPSND audio streams. A 1 MiB transfer unit (the old value) makes
/// each RDPDR data PDU monopolize that writer long enough to stall audio (whose
/// lag model then *drops* stale waves → choppy) and back up video; a deep
/// readahead fires a big burst of concurrent reads that saturates the shared
/// connection. Smaller PDUs + a shallow readahead keep the per-PDU critical
/// section short so A/V interleave cleanly. The cost is lower peak drive
/// throughput (more round-trips) — the right trade for an interactive remote
/// desktop, where smooth audio/video beats fast bulk file transfer. (Each NFS op
/// is still one RDPDR open→act→close, mitigated by the kept-open handle cache.)
fn run_mount(port: u16, mountpoint: &Path) -> bool {
    let opts = format!(
        "nolocks,vers=3,tcp,rsize=262144,wsize=262144,readahead=4,port={port},mountport={port},actimeo=5"
    );
    match std::process::Command::new("/sbin/mount_nfs")
        .arg("-o")
        .arg(&opts)
        .arg("localhost:/")
        .arg(mountpoint)
        .status()
    {
        Ok(s) if s.success() => true,
        Ok(s) => {
            warn!(code = ?s.code(), "rdpdr nfs: mount_nfs exited nonzero");
            false
        }
        Err(e) => {
            warn!(error = %e, "rdpdr nfs: could not spawn mount_nfs");
            false
        }
    }
}

/// Pick + create an empty mountpoint for `label`, preferring `/Volumes/<label>`
/// (so it shows as a real volume in Finder's sidebar). Appends a numeric suffix
/// on collision, and falls back to a temp dir if `/Volumes` isn't writable.
fn prepare_mountpoint(label: &str) -> std::io::Result<PathBuf> {
    let base = Path::new("/Volumes");
    let mut candidate = base.join(label);
    let mut n = 1;
    while candidate.exists() && n < 50 {
        candidate = base.join(format!("{label}-{n}"));
        n += 1;
    }
    match std::fs::create_dir(&candidate) {
        Ok(()) => Ok(candidate),
        Err(_) => {
            let tmp = std::env::temp_dir()
                .join(format!("macrdp-rdpdr-{}", std::process::id()))
                .join(label);
            std::fs::create_dir_all(&tmp)?;
            Ok(tmp)
        }
    }
}

/// Make a drive label safe for a single path component (no `/`, `:` etc.).
fn sanitize_label(label: &str) -> String {
    let trimmed = label.trim().trim_end_matches(':');
    let cleaned: String = trimmed
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c == ':' || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    if cleaned.is_empty() {
        "drive".to_owned()
    } else {
        cleaned
    }
}
