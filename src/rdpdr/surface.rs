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

/// The `deadtimeout` mount option, in seconds (see [`run_mount`]).
const DEAD_TIMEOUT_SECS: u32 = 60;

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

    /// Move `id` to `to_path` (a child of `to_parent`), keeping its fileid.
    ///
    /// NFS keeps an object's identity across a rename, and macOS relies on it:
    /// whatever already holds the object (an open file, the folder a Finder
    /// window shows) keeps using the old fileid. Forgetting the id and
    /// interning the new path under a fresh one, as this used to, made every
    /// later `getattr` on the held id fail with NFS3ERR_NOENT — logged at ERROR
    /// by nfsserve every 5 s (the attribute-cache interval) for as long as the
    /// window stayed open. Reproduced live 2026-11-13 by creating a folder in
    /// Finder (it makes "untitled folder" and then renames it).
    ///
    /// A node already at `to_path` was overwritten by the rename and is
    /// dropped. For a folder, every cached path inside it moves too, or later
    /// operations on its contents would use the old location.
    fn rename(&mut self, id: fileid3, to_parent: fileid3, to_path: &str) {
        let Some(old_path) = self.id_to_node.get(&id).map(|n| n.remote_path.clone()) else {
            return;
        };
        if let Some(&replaced) = self.path_to_id.get(to_path) {
            if replaced != id {
                self.forget(replaced);
            }
        }
        self.path_to_id.remove(&old_path);
        if let Some(n) = self.id_to_node.get_mut(&id) {
            n.remote_path = to_path.to_owned();
            n.parent = to_parent;
        }
        self.path_to_id.insert(to_path.to_owned(), id);

        let old_prefix = format!("{old_path}\\");
        let moved: Vec<(fileid3, String)> = self
            .id_to_node
            .iter()
            .filter(|(_, n)| n.remote_path.starts_with(&old_prefix))
            .map(|(&cid, n)| (cid, n.remote_path.clone()))
            .collect();
        for (cid, path) in moved {
            let new_path = join_remote(to_path, &path[old_prefix.len()..]);
            self.path_to_id.remove(&path);
            if let Some(n) = self.id_to_node.get_mut(&cid) {
                n.remote_path = new_path.clone();
            }
            self.path_to_id.insert(new_path, cid);
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

    /// Attributes for a node. Files and folders are reported as owned by the
    /// user macrdp runs as (who mounted the volume). Left at the default (uid 0,
    /// gid 0), Finder showed them as owned by "system" with "everyone: Read
    /// only", so New Folder silently did nothing and a rename failed with
    /// "another operation is in progress" — Finder judges writability from
    /// ownership and mode, even though the server's ACCESS reply allows writes
    /// (shell commands worked). Found live 2026-11-13.
    fn attr(&self, id: fileid3, is_dir: bool, size: u64) -> fattr3 {
        let t = nfstime3 {
            seconds: self.created_secs,
            nseconds: 0,
        };
        // SAFETY: getuid/getgid cannot fail.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        if is_dir {
            fattr3 {
                ftype: ftype3::NF3DIR,
                mode: 0o755,
                nlink: 2,
                uid,
                gid,
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
                uid,
                gid,
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
        let (src_id, _, _) = src.ok_or(nfsstat3::NFS3ERR_NOENT)?;
        self.handle
            .rename(self.device_id, &from_path, &to_path, true)
            .await
            .map_err(|e| nfs_err("rename failed", &e))?;
        self.cache
            .lock()
            .unwrap()
            .rename(src_id, to_dirid, &to_path);
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

/// How long one `umount` may run before we stop waiting. A forced unmount of
/// a loopback NFS mount normally returns at once; the bound exists so that
/// neither the shutdown path (launchd kills the process if it lingers) nor the
/// startup reaper can be held up by one that doesn't.
const UMOUNT_TIMEOUT: Duration = Duration::from_secs(10);

/// Run `cmd` to completion, or kill it once `timeout` has passed. Returns its
/// output, or why there is none. Pure apart from the child process;
/// unit-tested.
fn run_with_timeout(
    mut cmd: std::process::Command,
    timeout: Duration,
) -> Result<std::process::Output, String> {
    let mut child = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not start: {e}"))?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().map_err(|e| e.to_string()),
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("timed out after {timeout:?}"));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// Every current mountpoint, from the kernel's mount table.
///
/// `MNT_NOWAIT` returns the cached table without asking any filesystem for
/// fresh statistics, so a mount whose NFS server has died can't make this
/// hang. That is the point: a `stat()` on such a mountpoint blocks forever (a
/// hard NFS mount waits for its server), which is what stalled the startup
/// reaper after a restart with a drive mounted (2026-11-10).
fn mount_points() -> Vec<PathBuf> {
    // SAFETY: a null buffer asks only for the number of mounts.
    let n = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    if n <= 0 {
        return Vec::new();
    }
    // Room for a few mounts appearing between the two calls.
    let cap = n as usize + 8;
    // SAFETY: `statfs` is plain old data; all-zero is a valid value.
    let mut buf: Vec<libc::statfs> = vec![unsafe { std::mem::zeroed() }; cap];
    let bytes = (cap * std::mem::size_of::<libc::statfs>()) as libc::c_int;
    // SAFETY: `buf` holds `cap` entries and `bytes` is exactly its size.
    let got = unsafe { libc::getfsstat(buf.as_mut_ptr(), bytes, libc::MNT_NOWAIT) };
    if got <= 0 {
        return Vec::new();
    }
    buf.truncate(got as usize);
    buf.iter()
        .map(|fs| {
            // SAFETY: the kernel NUL-terminates f_mntonname.
            let name = unsafe { std::ffi::CStr::from_ptr(fs.f_mntonname.as_ptr()) };
            PathBuf::from(
                <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(name.to_bytes()),
            )
        })
        .collect()
}

/// The mountpoints in `mounts` that are `dir` itself or inside it. Compares
/// path components, so `/t/macrdp-rdpdr-12` doesn't claim
/// `/t/macrdp-rdpdr-123/x`. Pure; unit-tested.
fn mounts_under(mounts: &[PathBuf], dir: &Path) -> Vec<PathBuf> {
    mounts
        .iter()
        .filter(|m| m.starts_with(dir))
        .cloned()
        .collect()
}

/// `p` as the mount table spells it, without touching `p` itself: only its
/// parent is resolved (`$TMPDIR` lives under the `/var` → `/private/var`
/// symlink). Resolving `p` would `stat` it, and if `p` is a dead mountpoint
/// that never returns.
fn mount_table_path(p: &Path) -> PathBuf {
    match (p.parent(), p.file_name()) {
        (Some(parent), Some(name)) => std::fs::canonicalize(parent)
            .map(|c| c.join(name))
            .unwrap_or_else(|_| p.to_path_buf()),
        _ => p.to_path_buf(),
    }
}

/// `umount -f` the mountpoint, then remove it and prune the empty per-pid
/// parent. Shared by `Surface::Drop` (disconnect), [`shutdown_cleanup`] (signal
/// exit) and [`reap_stale`] (startup). Returns whether it ended unmounted.
///
/// Checks the mount table first, so an already-unmounted path is just
/// cleaned up (no `umount` error for a double call). A failed or stuck
/// `umount` is logged at warn with its error output — it used to be debug
/// only, which hid why a restart left a mount behind — and the directories are
/// then left alone: removing them would touch a still-mounted, possibly dead,
/// mountpoint.
fn unmount_at(mp: &Path) -> bool {
    let table_path = mount_table_path(mp);
    if mounts_under(&mount_points(), &table_path).is_empty() {
        remove_mountpoint_dirs(mp);
        return true;
    }
    let mut cmd = std::process::Command::new("/sbin/umount");
    cmd.arg("-f").arg(mp);
    let unmounted = match run_with_timeout(cmd, UMOUNT_TIMEOUT) {
        Ok(out) if out.status.success() => true,
        Ok(out) => {
            warn!(
                mountpoint = ?mp,
                code = ?out.status.code(),
                stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                "rdpdr nfs: umount failed"
            );
            false
        }
        Err(e) => {
            warn!(mountpoint = ?mp, error = %e, "rdpdr nfs: umount did not complete");
            false
        }
    };
    // Trust the table, not the exit code: macOS can report a failure for a
    // forced unmount that did take effect.
    if unmounted || mounts_under(&mount_points(), &table_path).is_empty() {
        remove_mountpoint_dirs(mp);
        debug!(mountpoint = ?mp, "rdpdr nfs: cleaned up mount");
        true
    } else {
        false
    }
}

/// Remove an (unmounted) mountpoint and prune its per-pid parent, which only
/// succeeds once empty, so it's safe while other drives are mounted under it.
fn remove_mountpoint_dirs(mp: &Path) {
    let _ = std::fs::remove_dir(mp);
    if let Some(parent) = mp.parent() {
        let _ = std::fs::remove_dir(parent);
    }
}

/// Unmount every still-live RDPDR NFS volume. Called from the signal handler
/// just before `std::process::exit` (which bypasses `Surface::Drop`), so a
/// SIGINT/SIGTERM stop — Ctrl-C, `kill`, `launchctl bootout`/`kickstart -k` —
/// doesn't strand a mount pointing at the now-dead NFS server. Synchronous and
/// best-effort; each `umount` is bounded by [`UMOUNT_TIMEOUT`]. The NFS server
/// is still running here (the process exits after this), so a plain forced
/// unmount should succeed; anything that slips through is cleared by the
/// `deadtimeout` mount option or the next start's [`reap_stale`].
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
        if unmount_at(&mp) {
            info!(mountpoint = ?mp, "rdpdr nfs: unmounted on shutdown");
        }
    }
}

/// Reap mount leftovers from a PRIOR macrdp process that died uncleanly. A
/// SIGKILL/panic skips both `Surface::Drop` and [`shutdown_cleanup`], so its
/// `$TMPDIR/macrdp-rdpdr-<pid>/<label>` mountpoint(s) — and the stale NFS mounts
/// pointing at its now-dead server — survive. Only dirs tagged with a DEAD pid
/// (never our own) are touched, so this is safe while another macrdp instance
/// is live. Best-effort; called once at startup off the async runtime.
///
/// It never touches a mountpoint directly: the mounts are found in the kernel
/// mount table ([`mount_points`]) and unmounted with a bounded `umount`. A
/// `stat()` (`is_dir()`) on a dead NFS mountpoint never returns, which is how
/// this used to hang until the user clicked "Disconnect All" (2026-11-10). If
/// a mount still won't go, the dir is left for the `deadtimeout` mount option
/// to clear rather than recursed into. `/Volumes/<label>` mounts (only when
/// running as root) are not reaped — there's no pid to gate on.
pub fn reap_stale() {
    crate::reaper::for_each_stale(
        &std::env::temp_dir(),
        "macrdp-rdpdr-",
        true,
        std::process::id() as i32,
        &crate::reaper::process_is_alive,
        &mut |dir| {
            // The per-pid dir itself is never a mountpoint, so resolving it
            // is safe; its children may be dead mounts and are not touched.
            let table_dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
            for mp in mounts_under(&mount_points(), &table_dir) {
                unmount_at(&mp);
            }
            let left = mounts_under(&mount_points(), &table_dir);
            if left.is_empty() {
                let _ = std::fs::remove_dir_all(dir);
                info!(stale_dir = ?dir, "rdpdr nfs: reaped mount dir from a dead prior macrdp process");
            } else {
                warn!(
                    stale_dir = ?dir,
                    mounts = ?left,
                    "rdpdr nfs: a dead prior process's mount would not unmount — leaving it \
                     for the kernel's deadtimeout to clear"
                );
            }
        },
    );
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
///
/// **`deadtimeout`** (2026-11-13): if the mount stays unresponsive this long
/// after macOS first reports it unresponsive, the kernel marks it dead and
/// force-unmounts it. The server lives in this process, so it only goes away
/// when macrdp does: a restart whose unmount failed, a crash, SIGKILL, power
/// loss. Without it, such a mount stayed until the user clicked "Disconnect
/// All" in macOS's "Server connections interrupted" dialog, and any `stat()` on
/// it hung. A live session only trips it if the client stops answering drive
/// requests for over a minute, by which point the session is gone too.
fn run_mount(port: u16, mountpoint: &Path) -> bool {
    let opts = format!(
        "nolocks,vers=3,tcp,rsize=262144,wsize=262144,readahead=4,port={port},mountport={port},actimeo=5,deadtimeout={DEAD_TIMEOUT_SECS}"
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

#[cfg(test)]
mod tests {
    use super::*;

    fn path_of(c: &FsCache, id: fileid3) -> &str {
        &c.id_to_node[&id].remote_path
    }

    #[test]
    fn a_renamed_file_keeps_its_fileid() {
        // THE 2026-11-13 NOENT LOOP: Finder holds the object it renamed; a new
        // id for the new name left the held one unknown.
        let mut c = FsCache::new();
        let f = c.intern(ROOT_ID, "\\hello.txt", false, 41);
        c.rename(f, ROOT_ID, "\\renamed.txt");
        assert_eq!(path_of(&c, f), "\\renamed.txt");
        assert_eq!(c.path_to_id.get("\\renamed.txt"), Some(&f));
        assert!(!c.path_to_id.contains_key("\\hello.txt"));
        assert_eq!(c.id_to_node[&f].size, 41);
    }

    #[test]
    fn a_rename_over_an_existing_file_drops_the_replaced_one() {
        let mut c = FsCache::new();
        let tmp = c.intern(ROOT_ID, "\\doc.tmp", false, 10);
        let old = c.intern(ROOT_ID, "\\doc.txt", false, 5);
        c.rename(tmp, ROOT_ID, "\\doc.txt"); // an atomic save
        assert_eq!(c.path_to_id.get("\\doc.txt"), Some(&tmp));
        assert!(!c.id_to_node.contains_key(&old));
        assert!(!c.path_to_id.contains_key("\\doc.tmp"));
    }

    #[test]
    fn a_renamed_folder_moves_everything_inside_it() {
        let mut c = FsCache::new();
        let dir = c.intern(ROOT_ID, "\\untitled folder", true, 0);
        let sub = c.intern(dir, "\\untitled folder\\sub", true, 0);
        let file = c.intern(sub, "\\untitled folder\\sub\\a.txt", false, 3);
        // A sibling whose name merely starts the same way stays put.
        let sibling = c.intern(ROOT_ID, "\\untitled folder 2", true, 0);
        let other = c.intern(ROOT_ID, "\\other", true, 0);

        c.rename(dir, other, "\\other\\Projects");

        assert_eq!(path_of(&c, dir), "\\other\\Projects");
        assert_eq!(c.id_to_node[&dir].parent, other);
        assert_eq!(path_of(&c, sub), "\\other\\Projects\\sub");
        assert_eq!(path_of(&c, file), "\\other\\Projects\\sub\\a.txt");
        assert_eq!(
            c.path_to_id.get("\\other\\Projects\\sub\\a.txt"),
            Some(&file)
        );
        assert!(!c.path_to_id.contains_key("\\untitled folder\\sub\\a.txt"));
        assert_eq!(path_of(&c, sibling), "\\untitled folder 2");
        // Parents of the moved children are unchanged ids.
        assert_eq!(c.id_to_node[&file].parent, sub);
    }

    #[test]
    fn run_with_timeout_returns_output_or_kills_a_stuck_command() {
        let ok = run_with_timeout(
            std::process::Command::new("/bin/echo"),
            Duration::from_secs(5),
        )
        .expect("echo runs");
        assert!(ok.status.success());

        let failed = run_with_timeout(
            std::process::Command::new("/usr/bin/false"),
            Duration::from_secs(5),
        )
        .expect("false runs");
        assert!(!failed.status.success());

        // A command that would outlive the bound is killed, promptly.
        let mut sleep = std::process::Command::new("/bin/sleep");
        sleep.arg("30");
        let started = std::time::Instant::now();
        let stuck = run_with_timeout(sleep, Duration::from_millis(200));
        assert!(
            matches!(&stuck, Err(e) if e.contains("timed out")),
            "{stuck:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn mounts_under_matches_whole_path_components() {
        let mounts = [
            PathBuf::from("/"),
            PathBuf::from("/private/var/folders/x/T/macrdp-rdpdr-12/D"),
            PathBuf::from("/private/var/folders/x/T/macrdp-rdpdr-123/E"),
            PathBuf::from("/Volumes/USB"),
        ];
        let dir = Path::new("/private/var/folders/x/T/macrdp-rdpdr-12");
        assert_eq!(
            mounts_under(&mounts, dir),
            vec![PathBuf::from("/private/var/folders/x/T/macrdp-rdpdr-12/D")]
        );
        // The mountpoint itself counts as "under" itself.
        let mp = Path::new("/Volumes/USB");
        assert_eq!(
            mounts_under(&mounts, mp),
            vec![PathBuf::from("/Volumes/USB")]
        );
        assert!(mounts_under(&mounts, Path::new("/nowhere")).is_empty());
    }

    #[test]
    fn mount_points_reads_the_live_table() {
        // Read-only and never blocks (MNT_NOWAIT); the root is always mounted.
        assert!(mount_points().contains(&PathBuf::from("/")));
    }

    #[test]
    fn mount_table_path_resolves_only_the_parent() {
        // /tmp is a symlink to /private/tmp; the leaf needn't exist, which
        // shows it is never stat'ed.
        assert_eq!(
            mount_table_path(Path::new("/tmp/macrdp-no-such-mountpoint")),
            PathBuf::from("/private/tmp/macrdp-no-such-mountpoint")
        );
    }

    #[test]
    fn unmount_at_cleans_up_a_mountpoint_that_is_not_mounted() {
        // The double-call case (Drop then shutdown): nothing mounted, so no
        // umount runs and the dirs are just removed.
        let parent = std::env::temp_dir().join(format!(
            "macrdp-rdpdr-test-{}-{}",
            std::process::id(),
            line!()
        ));
        let mp = parent.join("D");
        std::fs::create_dir_all(&mp).unwrap();
        assert!(unmount_at(&mp));
        assert!(!mp.exists());
        assert!(!parent.exists(), "the emptied per-pid parent is pruned");
    }
}
