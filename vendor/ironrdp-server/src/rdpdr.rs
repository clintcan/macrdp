//! Server-side RDPDR (drive redirection) static channel — [MS-RDPEFS].
//!
//! Server-direction peer to `ironrdp-rdpdr`'s client `Rdpdr`: drives the init
//! handshake the client expects (Server Announce → capability exchange → Client
//! ID Confirm → User Logged On), surfaces the client's announced devices to a
//! [`RdpdrServerHandler`] backend, and issues device-I/O requests (open / read /
//! close) whose completions are matched back to the awaiting caller by an
//! [`IoRouter`]. The async [`RdpdrHandle`] is what the backend uses to read the
//! client's files.
//!
//! Lives here alongside the other server channel processors (`echo.rs`,
//! `gfx.rs`, `sound.rs`); reuses `ironrdp-rdpdr` purely for the MS-RDPEFS wire
//! types. Upstreamable as a `SvcServerProcessor` peer to the client `Rdpdr`.
//!
//! [MS-RDPEFS]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpefs/34d9de58-b2b5-40b6-b970-f82d4603bdb5

use core::fmt;
use std::collections::HashMap;
use std::io::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result, anyhow};
use ironrdp_core::{ReadCursor, impl_as_any};
use ironrdp_pdu::gcc::ChannelName;
use ironrdp_pdu::utils::CharacterSet;
use ironrdp_pdu::{PduResult, decode_err};
use ironrdp_rdpdr::pdu::efs::{
    Boolean, Capabilities, ClientDeviceListAnnounce, ClientNameRequest, CoreCapability, CoreCapabilityKind,
    CreateDisposition, CreateOptions, DesiredAccess, DeviceCloseRequest, DeviceCreateRequest, DeviceCreateResponse,
    DeviceIoRequest, DeviceIoResponse, DeviceReadRequest, DeviceReadResponse, DeviceType, DeviceWriteRequest,
    DeviceWriteResponse, FileAttributes, FileDirectoryInformation, FileDispositionInformation,
    FileEndOfFileInformation, FileInformationClass, FileInformationClassLevel, FileRenameInformation, MajorFunction,
    MinorFunction, NtStatus, ServerDeviceAnnounceResponse, ServerDriveIoRequest, ServerDriveQueryDirectoryRequest,
    ServerDriveSetInformationRequest, SharedAccess, VERSION_MAJOR, VERSION_MINOR_RDP51, VersionAndIdPdu,
    VersionAndIdPduKind,
};
use ironrdp_rdpdr::pdu::esc::{
    CardProtocol, CardStateFlags, ConnectCall, ConnectCommon, ConnectReturn, ContextCall, EstablishContextCall,
    EstablishContextReturn, GetStatusChangeCall, GetStatusChangeReturn, HCardAndDispositionCall, ListReadersCall,
    ListReadersReturn, LongReturn, ReaderState, ReaderStateCommonCall, ReturnCode, SCardIORequest, ScardCall,
    ScardContext, ScardHandle as ScardCardHandle, ScardIoCtlCode, Scope, StatusCall, StatusReturn, TransmitCall,
    TransmitReturn,
};
use ironrdp_rdpdr::pdu::{PacketId, RdpdrPdu, ScardControlRequest, SharedHeader};
use ironrdp_svc::{CompressionCondition, SvcMessage, SvcProcessor, SvcServerProcessor};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::{ServerEvent, ServerEventSender};

/// A device the client announced as redirected.
#[derive(Debug, Clone)]
pub struct AnnouncedDevice {
    pub device_id: u32,
    pub device_type: DeviceType,
    /// The 8-char PreferredDosName the client sent (e.g. a drive/share label).
    pub name: String,
}

/// Backend hooks for the macrdp side of RDPDR.
pub trait RdpdrServerHandler: Send + fmt::Debug {
    /// Hands the backend the async [`RdpdrHandle`] used to read the client's
    /// files. Called once, before the channel starts. Default no-op.
    fn set_handle(&mut self, _handle: RdpdrHandle) {}
    /// Called when the client announces (or re-announces) its device list.
    fn on_devices_announced(&mut self, devices: &[AnnouncedDevice]);
}

/// Builds the macrdp-side backend + supplies connection config.
pub trait RdpdrBackendFactory {
    fn build_backend(&self) -> Box<dyn RdpdrServerHandler>;
    /// Computer name shown to the client (Explorer renders shares as
    /// "`<directory>` on `<computer_name>`").
    fn computer_name(&self) -> String;
}

/// Factory for the RDPDR static channel, mirroring [`SoundServerFactory`] /
/// [`CliprdrServerFactory`].
///
/// [`SoundServerFactory`]: crate::SoundServerFactory
/// [`CliprdrServerFactory`]: crate::CliprdrServerFactory
pub trait RdpdrServerFactory: RdpdrBackendFactory + ServerEventSender {}

/// Outbound RDPDR messages the server pushes onto the connection event loop.
#[derive(Debug)]
pub enum RdpdrServerMessage {
    /// Pre-framed SVC messages to write on the rdpdr static channel.
    SendMessages(Vec<SvcMessage>),
}

/// One entry from a directory listing.
#[derive(Debug, Clone)]
pub struct DirEntry {
    /// File/directory name (no path).
    pub name: String,
    /// Size in bytes (0 for directories).
    pub size: u64,
    pub is_dir: bool,
}

/// Error carrying the [`NtStatus`] the client returned for a failed device-I/O
/// request. Callers can match on `status` to surface a precise error (e.g.
/// `ACCESS_DENIED` → "permission denied", `OBJECT_NAME_COLLISION` → "exists")
/// instead of a generic failure. Returned (boxed into `anyhow::Error`) by the
/// [`RdpdrHandle`] ops; recover it with `err.downcast_ref::<RdpdrStatus>()`.
#[derive(Debug, Clone)]
pub struct RdpdrStatus {
    /// The operation that failed, e.g. `create(\\foo.txt)` or `write`.
    pub op: String,
    pub status: NtStatus,
}

impl fmt::Display for RdpdrStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RDPDR {} failed: {:?}", self.op, self.status)
    }
}

impl std::error::Error for RdpdrStatus {}

/// Matches `DeviceIoCompletion` responses to the request awaiting them, keyed by
/// completion id (the RDPDR analogue of clipboard's `DownloadRouter`). The
/// delivered `Vec<u8>` is the response body (a `DeviceIoResponse` followed by
/// the per-request tail); the waiter decodes the specific response type.
#[derive(Clone, Default)]
pub struct IoRouter {
    inner: Arc<IoRouterInner>,
}

#[derive(Default)]
struct IoRouterInner {
    next_id: AtomicU32,
    pending: Mutex<HashMap<u32, oneshot::Sender<Vec<u8>>>>,
}

impl IoRouter {
    fn new() -> Self {
        Self::default()
    }

    /// Allocate a completion id and a receiver for its response body.
    fn register(&self) -> (u32, oneshot::Receiver<Vec<u8>>) {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().unwrap().insert(id, tx);
        (id, rx)
    }

    /// Deliver a response body to the matching waiter (dropped if none).
    fn deliver(&self, completion_id: u32, body: Vec<u8>) {
        if let Some(tx) = self.inner.pending.lock().unwrap().remove(&completion_id) {
            let _ = tx.send(body);
        } else {
            warn!(completion_id, "RDPDR: I/O completion with no waiter (dropped)");
        }
    }
}

/// Upper bound on simultaneously-cached open file handles per connection.
/// Sequential I/O on one file reuses a single handle; the LRU evicts the
/// least-recently-used once a workload touches more distinct files than this.
const MAX_OPEN_HANDLES: usize = 16;

/// Which access a cached handle was opened with — a read and a write of the same
/// file are cached separately (a read-only file can't be opened for write).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum AccessKind {
    Read,
    Write,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct HandleKey {
    device_id: u32,
    path: String,
    kind: AccessKind,
}

struct CachedHandle {
    file_id: u32,
    /// Monotonic LRU stamp drawn from [`HandleCache::tick`].
    last_used: u64,
}

/// LRU cache of open RDPDR file handles. Consecutive reads/writes on the same
/// file reuse one open→…→close instead of paying create+close per NFS chunk —
/// the lever that makes large sequential transfers one round-trip per chunk
/// instead of three.
#[derive(Default)]
struct HandleCache {
    map: Mutex<HashMap<HandleKey, CachedHandle>>,
    tick: AtomicU64,
}

/// Async handle for issuing device-I/O to the client over the rdpdr channel.
/// Cheap to clone (the file-handle cache is shared across clones); backends keep
/// one to read/write the client's files on demand.
#[derive(Clone)]
pub struct RdpdrHandle {
    sender: mpsc::UnboundedSender<ServerEvent>,
    router: IoRouter,
    cache: Arc<HandleCache>,
}

impl fmt::Debug for RdpdrHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RdpdrHandle").finish_non_exhaustive()
    }
}

impl RdpdrHandle {
    fn new(sender: mpsc::UnboundedSender<ServerEvent>, router: IoRouter) -> Self {
        Self {
            sender,
            router,
            cache: Arc::new(HandleCache::default()),
        }
    }

    /// Read up to `length` bytes from `offset` of `path` on `device_id`, reusing a
    /// cached open handle (opening one on a miss) so consecutive chunk reads don't
    /// re-open. Path uses Windows backslash separators relative to the redirected
    /// drive root (e.g. `\\readme.txt`).
    pub async fn read_file(&self, device_id: u32, path: &str, offset: u64, length: u32) -> Result<Vec<u8>> {
        let file_id = self.acquire(device_id, path, AccessKind::Read).await?;
        match self.read(device_id, file_id, offset, length).await {
            Ok(data) => Ok(data),
            Err(e) => {
                // The cached handle may be stale; drop it so the next read re-opens.
                self.invalidate(device_id, path, AccessKind::Read);
                Err(e)
            }
        }
    }

    /// Open `path`, stream it to `dst` on disk in chunks (open once, read until
    /// EOF, close), and return the number of bytes written. Used by the Finder
    /// surface to materialize a file on demand.
    pub async fn read_to_file(&self, device_id: u32, path: &str, dst: &Path) -> Result<u64> {
        const CHUNK: u32 = 1024 * 1024; // 1 MiB reads
        let file_id = self
            .create_with(
                device_id,
                path,
                Self::file_read_access(),
                CreateOptions::FILE_SYNCHRONOUS_IO_NONALERT | CreateOptions::FILE_NON_DIRECTORY_FILE,
            )
            .await?;
        let result = async {
            let mut file = std::fs::File::create(dst).map_err(|e| anyhow!("create {dst:?}: {e}"))?;
            let mut offset = 0u64;
            loop {
                let data = self.read(device_id, file_id, offset, CHUNK).await?;
                if data.is_empty() {
                    break;
                }
                file.write_all(&data).map_err(|e| anyhow!("write {dst:?}: {e}"))?;
                offset += data.len() as u64;
                if (data.len() as u32) < CHUNK {
                    break; // short read = EOF
                }
            }
            Ok(offset)
        }
        .await;
        let _ = self.close(device_id, file_id).await;
        result
    }

    /// List the entries of directory `dir_path` (Windows backslash path relative
    /// to the drive root; `\\` is the root). Returns the entries excluding the
    /// `.`/`..` pseudo-entries.
    pub async fn list_dir(&self, device_id: u32, dir_path: &str) -> Result<Vec<DirEntry>> {
        let file_id = self
            .create_with(
                device_id,
                dir_path,
                DesiredAccess::FILE_READ_DATA_OR_FILE_LIST_DIRECTORY | DesiredAccess::FILE_READ_ATTRIBUTES,
                CreateOptions::FILE_DIRECTORY_FILE,
            )
            .await?;
        let pattern = query_pattern(dir_path);
        let mut entries = Vec::new();
        let mut initial = true;
        loop {
            let (completion_id, rx) = self.router.register();
            let mut header = self.io_header(device_id, completion_id, MajorFunction::DirectoryControl);
            header.file_id = file_id;
            header.minor_function = MinorFunction::IRP_MN_QUERY_DIRECTORY;
            self.send(ServerDriveIoRequest::ServerDriveQueryDirectoryRequest(
                ServerDriveQueryDirectoryRequest {
                    device_io_request: header,
                    file_info_class_lvl: FileInformationClassLevel::FILE_DIRECTORY_INFORMATION,
                    initial_query: u8::from(initial),
                    path: if initial { pattern.clone() } else { String::new() },
                },
            ))?;
            initial = false;

            let body = rx.await.context("RDPDR query-dir: connection closed")?;
            let mut src = ReadCursor::new(&body);
            let io = DeviceIoResponse::decode(&mut src).map_err(|e| anyhow!("{e}"))?;
            // The client signals end-of-enumeration with NO_MORE_FILES; some
            // return another error code there instead — either way we're done.
            if io.io_status != NtStatus::SUCCESS {
                break;
            }
            if src.len() < 4 {
                break;
            }
            let _length = src.read_u32();
            let entry = match FileDirectoryInformation::decode(&mut src) {
                Ok(e) => e,
                Err(e) => {
                    warn!(error = ?e, "RDPDR: undecodable directory entry");
                    break;
                }
            };
            if entry.file_name == "." || entry.file_name == ".." {
                continue;
            }
            let is_dir = entry.file_attributes.contains(FileAttributes::FILE_ATTRIBUTE_DIRECTORY);
            entries.push(DirEntry {
                name: entry.file_name,
                size: u64::try_from(entry.end_of_file).unwrap_or(0),
                is_dir,
            });
        }
        let _ = self.close(device_id, file_id).await;
        Ok(entries)
    }

    /// Write `data` at `offset` of `path`, reusing a cached open write handle
    /// (opening one on a miss) so consecutive chunk writes don't re-open. The file
    /// must already exist (NFS creates it via [`Self::create_file`] before writing).
    pub async fn write_file(&self, device_id: u32, path: &str, offset: u64, data: &[u8]) -> Result<u64> {
        let file_id = self.acquire(device_id, path, AccessKind::Write).await?;
        match self.write(device_id, file_id, offset, data).await {
            Ok(n) => Ok(u64::from(n)),
            Err(e) => {
                self.invalidate(device_id, path, AccessKind::Write);
                Err(e)
            }
        }
    }

    /// Create a regular file at `path`. With `exclusive`, fails if it already
    /// exists (FILE_CREATE); otherwise opens-or-creates (FILE_OPEN_IF).
    pub async fn create_file(&self, device_id: u32, path: &str, exclusive: bool) -> Result<()> {
        let disposition = if exclusive {
            CreateDisposition::FILE_CREATE
        } else {
            CreateDisposition::FILE_OPEN_IF
        };
        let file_id = self
            .open_with(
                device_id,
                path,
                Self::file_write_access(),
                disposition,
                CreateOptions::FILE_SYNCHRONOUS_IO_NONALERT | CreateOptions::FILE_NON_DIRECTORY_FILE,
            )
            .await?;
        self.close(device_id, file_id).await
    }

    /// Create a directory at `path` (fails if it already exists).
    pub async fn create_dir(&self, device_id: u32, path: &str) -> Result<()> {
        let file_id = self
            .open_with(
                device_id,
                path,
                Self::file_write_access(),
                CreateDisposition::FILE_CREATE,
                CreateOptions::FILE_DIRECTORY_FILE,
            )
            .await?;
        self.close(device_id, file_id).await
    }

    /// Delete the file/directory at `path` (open with DELETE access, set the
    /// disposition's delete-pending flag, close to commit). A directory must be
    /// empty.
    pub async fn remove(&self, device_id: u32, path: &str, is_dir: bool) -> Result<()> {
        // Release any cached handle first — a still-open handle would defer the
        // delete-on-close (the file would linger until that handle dropped).
        self.evict_path(device_id, path).await;
        let options = if is_dir {
            CreateOptions::FILE_DIRECTORY_FILE
        } else {
            CreateOptions::FILE_SYNCHRONOUS_IO_NONALERT | CreateOptions::FILE_NON_DIRECTORY_FILE
        };
        let file_id = self
            .open_with(
                device_id,
                path,
                DesiredAccess::DELETE | DesiredAccess::SYNCHRONIZE,
                CreateDisposition::FILE_OPEN,
                options,
            )
            .await?;
        let result = self
            .set_information(
                device_id,
                file_id,
                FileInformationClass::Disposition(FileDispositionInformation { delete_pending: 1 }),
            )
            .await;
        let _ = self.close(device_id, file_id).await;
        result
    }

    /// Rename / move the file/dir at `from` to `to` (both Windows backslash paths
    /// relative to the drive root).
    pub async fn rename(&self, device_id: u32, from: &str, to: &str, replace: bool) -> Result<()> {
        // Release any cached handles on both paths before moving (an open handle
        // on the source can block the rename; a stale one on the dest is wrong).
        self.evict_path(device_id, from).await;
        self.evict_path(device_id, to).await;
        let file_id = self
            .open_with(
                device_id,
                from,
                DesiredAccess::DELETE | DesiredAccess::SYNCHRONIZE,
                CreateDisposition::FILE_OPEN,
                CreateOptions::FILE_SYNCHRONOUS_IO_NONALERT,
            )
            .await?;
        let result = self
            .set_information(
                device_id,
                file_id,
                FileInformationClass::Rename(FileRenameInformation {
                    replace_if_exists: if replace { Boolean::True } else { Boolean::False },
                    file_name: to.to_owned(),
                }),
            )
            .await;
        let _ = self.close(device_id, file_id).await;
        result
    }

    /// Truncate/extend the file at `path` to `size` bytes (FileEndOfFileInformation).
    pub async fn set_len(&self, device_id: u32, path: &str, size: u64) -> Result<()> {
        let file_id = self
            .open_with(
                device_id,
                path,
                Self::file_write_access(),
                CreateDisposition::FILE_OPEN,
                CreateOptions::FILE_SYNCHRONOUS_IO_NONALERT | CreateOptions::FILE_NON_DIRECTORY_FILE,
            )
            .await?;
        let end_of_file = i64::try_from(size).unwrap_or(i64::MAX);
        let result = self
            .set_information(
                device_id,
                file_id,
                FileInformationClass::EndOfFile(FileEndOfFileInformation { end_of_file }),
            )
            .await;
        let _ = self.close(device_id, file_id).await;
        result
    }

    /// The `FILE_GENERIC_WRITE`-ish access set for opening a file to write. Like
    /// [`Self::file_read_access`] it requests SYNCHRONIZE (synchronous I/O) and
    /// keeps read rights so an open handle can be both read and written.
    fn file_write_access() -> DesiredAccess {
        DesiredAccess::FILE_READ_DATA_OR_FILE_LIST_DIRECTORY
            | DesiredAccess::FILE_WRITE_DATA_OR_FILE_ADD_FILE
            | DesiredAccess::FILE_APPEND_DATA_OR_FILE_ADD_SUBDIRECTORY
            | DesiredAccess::FILE_READ_ATTRIBUTES
            | DesiredAccess::FILE_WRITE_ATTRIBUTES
            | DesiredAccess::FILE_READ_EA
            | DesiredAccess::FILE_WRITE_EA
            | DesiredAccess::READ_CONTROL
            | DesiredAccess::SYNCHRONIZE
    }

    fn send(&self, req: ServerDriveIoRequest) -> Result<()> {
        let msg = SvcMessage::from(req);
        self.sender
            .send(ServerEvent::Rdpdr(RdpdrServerMessage::SendMessages(vec![msg])))
            .map_err(|_| anyhow!("RDPDR event channel closed"))
    }

    fn io_header(&self, device_id: u32, completion_id: u32, major: MajorFunction) -> DeviceIoRequest {
        DeviceIoRequest {
            device_id,
            file_id: 0,
            completion_id,
            major_function: major,
            minor_function: MinorFunction::from(0),
        }
    }

    /// The `FILE_GENERIC_READ` access set for opening a file to read. Real
    /// Windows/mstsc honors the requested rights strictly — a bare
    /// `GENERIC_READ` (which FreeRDP tolerated) leaves the handle without the
    /// SYNCHRONIZE right a synchronous `IRP_MJ_READ` needs, yielding
    /// STATUS_ACCESS_DENIED. This mirrors what a normal `CreateFile(GENERIC_READ)`
    /// expands to.
    fn file_read_access() -> DesiredAccess {
        DesiredAccess::FILE_READ_DATA_OR_FILE_LIST_DIRECTORY
            | DesiredAccess::FILE_READ_ATTRIBUTES
            | DesiredAccess::FILE_READ_EA
            | DesiredAccess::READ_CONTROL
            | DesiredAccess::SYNCHRONIZE
    }

    /// Open `path` with `CreateDisposition::FILE_OPEN` (the file/dir must exist).
    async fn create_with(
        &self,
        device_id: u32,
        path: &str,
        desired_access: DesiredAccess,
        create_options: CreateOptions,
    ) -> Result<u32> {
        self.open_with(
            device_id,
            path,
            desired_access,
            CreateDisposition::FILE_OPEN,
            create_options,
        )
        .await
    }

    /// General IRP_MJ_CREATE: open `path` with an explicit `create_disposition`
    /// (FILE_OPEN to require existence, FILE_CREATE to create-or-fail,
    /// FILE_OPEN_IF to open-or-create, …).
    async fn open_with(
        &self,
        device_id: u32,
        path: &str,
        desired_access: DesiredAccess,
        create_disposition: CreateDisposition,
        create_options: CreateOptions,
    ) -> Result<u32> {
        let (completion_id, rx) = self.router.register();
        self.send(ServerDriveIoRequest::ServerCreateDriveRequest(DeviceCreateRequest {
            device_io_request: self.io_header(device_id, completion_id, MajorFunction::Create),
            desired_access,
            allocation_size: 0,
            file_attributes: FileAttributes::empty(),
            // Broad share access so we can open files the OS (or another
            // process) already holds open for reading/writing — common for
            // C:\ system files; a narrow FILE_SHARE_READ would SHARING_VIOLATION.
            shared_access: SharedAccess::FILE_SHARE_READ
                | SharedAccess::FILE_SHARE_WRITE
                | SharedAccess::FILE_SHARE_DELETE,
            create_disposition,
            create_options,
            path: path.to_owned(),
        }))?;
        let body = rx.await.context("RDPDR create: connection closed")?;
        let resp = DeviceCreateResponse::decode(&mut ReadCursor::new(&body)).map_err(|e| anyhow!("{e}"))?;
        if resp.device_io_reply.io_status != NtStatus::SUCCESS {
            return Err(RdpdrStatus {
                op: format!("create({path})"),
                status: resp.device_io_reply.io_status,
            }
            .into());
        }
        Ok(resp.file_id)
    }

    async fn read(&self, device_id: u32, file_id: u32, offset: u64, length: u32) -> Result<Vec<u8>> {
        let (completion_id, rx) = self.router.register();
        let mut header = self.io_header(device_id, completion_id, MajorFunction::Read);
        header.file_id = file_id;
        self.send(ServerDriveIoRequest::DeviceReadRequest(DeviceReadRequest {
            device_io_request: header,
            length,
            offset,
        }))?;
        let body = rx.await.context("RDPDR read: connection closed")?;
        let resp = DeviceReadResponse::decode(&mut ReadCursor::new(&body)).map_err(|e| anyhow!("{e}"))?;
        if resp.device_io_reply.io_status != NtStatus::SUCCESS {
            return Err(RdpdrStatus {
                op: "read".to_owned(),
                status: resp.device_io_reply.io_status,
            }
            .into());
        }
        Ok(resp.read_data)
    }

    async fn close(&self, device_id: u32, file_id: u32) -> Result<()> {
        let (completion_id, rx) = self.router.register();
        let mut header = self.io_header(device_id, completion_id, MajorFunction::Close);
        header.file_id = file_id;
        self.send(ServerDriveIoRequest::DeviceCloseRequest(DeviceCloseRequest {
            device_io_request: header,
        }))?;
        let _ = rx.await; // ignore close status
        Ok(())
    }

    async fn write(&self, device_id: u32, file_id: u32, offset: u64, data: &[u8]) -> Result<u32> {
        let (completion_id, rx) = self.router.register();
        let mut header = self.io_header(device_id, completion_id, MajorFunction::Write);
        header.file_id = file_id;
        self.send(ServerDriveIoRequest::DeviceWriteRequest(DeviceWriteRequest {
            device_io_request: header,
            offset,
            write_data: data.to_vec(),
        }))?;
        let body = rx.await.context("RDPDR write: connection closed")?;
        let resp = DeviceWriteResponse::decode(&mut ReadCursor::new(&body)).map_err(|e| anyhow!("{e}"))?;
        if resp.device_io_reply.io_status != NtStatus::SUCCESS {
            return Err(RdpdrStatus {
                op: "write".to_owned(),
                status: resp.device_io_reply.io_status,
            }
            .into());
        }
        Ok(resp.length)
    }

    async fn set_information(&self, device_id: u32, file_id: u32, info: FileInformationClass) -> Result<()> {
        let (completion_id, rx) = self.router.register();
        let mut header = self.io_header(device_id, completion_id, MajorFunction::SetInformation);
        header.file_id = file_id;
        self.send(ServerDriveIoRequest::ServerDriveSetInformationRequest(
            ServerDriveSetInformationRequest {
                device_io_request: header,
                set_buffer: info,
            },
        ))?;
        let body = rx.await.context("RDPDR set-info: connection closed")?;
        // Only the DeviceIoResponse status matters (the response's Length echoes
        // the request); decode the header and check it succeeded.
        let io = DeviceIoResponse::decode(&mut ReadCursor::new(&body)).map_err(|e| anyhow!("{e}"))?;
        if io.io_status != NtStatus::SUCCESS {
            return Err(RdpdrStatus {
                op: "set-info".to_owned(),
                status: io.io_status,
            }
            .into());
        }
        Ok(())
    }

    // ---- open-handle cache ----

    /// Return a cached open handle for `(device_id, path, kind)`, opening and
    /// caching one on a miss. Bumps the entry's LRU stamp.
    async fn acquire(&self, device_id: u32, path: &str, kind: AccessKind) -> Result<u32> {
        let key = HandleKey {
            device_id,
            path: path.to_owned(),
            kind,
        };
        // Cache hit.
        {
            let mut map = self.cache.map.lock().unwrap();
            if let Some(h) = map.get_mut(&key) {
                h.last_used = self.cache.tick.fetch_add(1, Ordering::Relaxed);
                return Ok(h.file_id);
            }
        }
        // Miss: open (no lock held across the await).
        let (access, options) = match kind {
            AccessKind::Read => (
                Self::file_read_access(),
                CreateOptions::FILE_SYNCHRONOUS_IO_NONALERT | CreateOptions::FILE_NON_DIRECTORY_FILE,
            ),
            AccessKind::Write => (
                Self::file_write_access(),
                CreateOptions::FILE_SYNCHRONOUS_IO_NONALERT | CreateOptions::FILE_NON_DIRECTORY_FILE,
            ),
        };
        let file_id = self
            .open_with(device_id, path, access, CreateDisposition::FILE_OPEN, options)
            .await?;

        // Install it; reconcile with a concurrent acquire and enforce the cap.
        let mut to_close: Vec<(u32, u32)> = Vec::new();
        let resolved = {
            let mut map = self.cache.map.lock().unwrap();
            if let Some(h) = map.get_mut(&key) {
                // Another task opened the same handle first; use theirs, close ours.
                h.last_used = self.cache.tick.fetch_add(1, Ordering::Relaxed);
                to_close.push((device_id, file_id));
                h.file_id
            } else {
                map.insert(
                    key,
                    CachedHandle {
                        file_id,
                        last_used: self.cache.tick.fetch_add(1, Ordering::Relaxed),
                    },
                );
                if map.len() > MAX_OPEN_HANDLES
                    && let Some(evk) = map.iter().min_by_key(|(_, h)| h.last_used).map(|(k, _)| k.clone())
                    && let Some(ev) = map.remove(&evk)
                {
                    to_close.push((evk.device_id, ev.file_id));
                }
                file_id
            }
        };
        for (dev, fid) in to_close {
            self.spawn_close(dev, fid);
        }
        Ok(resolved)
    }

    /// Drop a cached handle (after a failed op on it) and close it off-path.
    fn invalidate(&self, device_id: u32, path: &str, kind: AccessKind) {
        let key = HandleKey {
            device_id,
            path: path.to_owned(),
            kind,
        };
        let fid = self.cache.map.lock().unwrap().remove(&key).map(|h| h.file_id);
        if let Some(fid) = fid {
            self.spawn_close(device_id, fid);
        }
    }

    /// Close + remove every cached handle (any access kind) for `path`, awaiting
    /// the closes so the client has released the handles before a delete/rename.
    async fn evict_path(&self, device_id: u32, path: &str) {
        let fids: Vec<u32> = {
            let mut map = self.cache.map.lock().unwrap();
            let keys: Vec<HandleKey> = map
                .keys()
                .filter(|k| k.device_id == device_id && k.path == path)
                .cloned()
                .collect();
            keys.iter().filter_map(|k| map.remove(k)).map(|h| h.file_id).collect()
        };
        for fid in fids {
            let _ = self.close(device_id, fid).await;
        }
    }

    /// Close a handle in a detached task (used for LRU eviction / invalidation,
    /// where the caller shouldn't block on the round-trip).
    fn spawn_close(&self, device_id: u32, file_id: u32) {
        let this = self.clone();
        tokio::spawn(async move {
            let _ = this.close(device_id, file_id).await;
        });
    }
}

/// SCARD share modes (`dwShareMode`) for [`RdpdrHandle::scard_connect`].
pub const SCARD_SHARE_EXCLUSIVE: u32 = 1;
pub const SCARD_SHARE_SHARED: u32 = 2;
pub const SCARD_SHARE_DIRECT: u32 = 3;
/// SCARD dispositions (`dwDisposition`) for [`RdpdrHandle::scard_disconnect`].
pub const SCARD_LEAVE_CARD: u32 = 0;
pub const SCARD_RESET_CARD: u32 = 1;
pub const SCARD_UNPOWER_CARD: u32 = 2;
pub const SCARD_EJECT_CARD: u32 = 3;

/// Smart-card redirection (MS-RDPESC over the RDPDR `Smartcard` device). These
/// drive the client's reader/card by issuing `SCARD_IOCTL_*` device-control
/// IOCTLs (RPCE-marshaled `ScardCall`s) and decoding the `*Return`s the client
/// sends back. `device_id` is the announced Smartcard device. All use the W
/// (Unicode) IOCTL variants. The PC/SC `ReturnCode` inside each return (distinct
/// from the transport `NtStatus`) is surfaced as an error unless `Success`.
impl RdpdrHandle {
    /// `SCardEstablishContext(SCARD_SCOPE_SYSTEM)` — a resource-manager context.
    pub async fn scard_establish_context(&self, device_id: u32) -> Result<ScardContext> {
        let call = ScardCall::EstablishContextCall(EstablishContextCall { scope: Scope::System });
        let buf = self
            .scard_call(device_id, ScardIoCtlCode::EstablishContext, call, 256)
            .await?;
        let ret = EstablishContextReturn::decode(&mut ReadCursor::new(&buf)).map_err(|e| anyhow!("{e}"))?;
        scard_ok(ret.return_code, "establish_context")?;
        Ok(ret.context)
    }

    /// `SCardReleaseContext`.
    pub async fn scard_release_context(&self, device_id: u32, context: ScardContext) -> Result<()> {
        let call = ScardCall::ContextCall(ContextCall { context });
        let buf = self
            .scard_call(device_id, ScardIoCtlCode::ReleaseContext, call, 256)
            .await?;
        let ret = LongReturn::decode(&mut ReadCursor::new(&buf)).map_err(|e| anyhow!("{e}"))?;
        scard_ok(ret.return_code, "release_context")
    }

    /// `SCardListReaders(mszGroups = NULL)` — every reader the client redirected.
    pub async fn scard_list_readers(&self, device_id: u32, context: ScardContext) -> Result<Vec<String>> {
        let call = ScardCall::ListReadersCall(ListReadersCall {
            context,
            groups_ptr_length: 0,
            groups_length: 0,
            groups_ptr: 0,
            groups: Vec::new(),
            readers_is_null: false,
            readers_size: 0xFFFF_FFFF, // SCARD_AUTOALLOCATE
        });
        let buf = self
            .scard_call(device_id, ScardIoCtlCode::ListReadersW, call, 4096)
            .await?;
        let ret = ListReadersReturn::decode(&mut ReadCursor::new(&buf)).map_err(|e| anyhow!("{e}"))?;
        scard_ok(ret.return_code, "list_readers")?;
        Ok(ret.readers)
    }

    /// `SCardGetStatusChange` over `readers` — used to read presence + ATR. Each
    /// returned [`ReaderStateCommonCall`] carries the reader's current state flags
    /// (e.g. `SCARD_STATE_PRESENT`) and, when a card is present, its ATR.
    pub async fn scard_get_status_change(
        &self,
        device_id: u32,
        context: ScardContext,
        readers: &[String],
        timeout_ms: u32,
    ) -> Result<Vec<ReaderStateCommonCall>> {
        let states: Vec<ReaderState> = readers
            .iter()
            .map(|r| ReaderState {
                reader: r.clone(),
                common: ReaderStateCommonCall {
                    current_state: CardStateFlags::SCARD_STATE_UNAWARE,
                    event_state: CardStateFlags::empty(),
                    atr_length: 0,
                    atr: [0u8; 36],
                },
            })
            .collect();
        let states_len = u32::try_from(states.len()).unwrap_or(u32::MAX);
        let call = ScardCall::GetStatusChangeCall(GetStatusChangeCall {
            context,
            timeout: timeout_ms,
            states_ptr_length: states_len,
            states_ptr: 0,
            states_length: states_len,
            states,
        });
        let buf = self
            .scard_call(device_id, ScardIoCtlCode::GetStatusChangeW, call, 4096)
            .await?;
        let ret = GetStatusChangeReturn::decode(&mut ReadCursor::new(&buf)).map_err(|e| anyhow!("{e}"))?;
        scard_ok(ret.return_code, "get_status_change")?;
        Ok(ret.reader_states)
    }

    /// `SCardConnect` — returns the card handle and the negotiated protocol.
    pub async fn scard_connect(
        &self,
        device_id: u32,
        context: ScardContext,
        reader: &str,
        share_mode: u32,
        preferred_protocols: CardProtocol,
    ) -> Result<(ScardCardHandle, CardProtocol)> {
        let call = ScardCall::ConnectCall(ConnectCall {
            reader: reader.to_owned(),
            common: ConnectCommon {
                context,
                share_mode,
                preferred_protocols,
            },
        });
        let buf = self.scard_call(device_id, ScardIoCtlCode::ConnectW, call, 2048).await?;
        let ret = ConnectReturn::decode(&mut ReadCursor::new(&buf)).map_err(|e| anyhow!("{e}"))?;
        scard_ok(ret.return_code, "connect")?;
        Ok((ret.handle, ret.active_protocol))
    }

    /// `SCardStatus` — reader name(s), card state, active protocol, and ATR.
    pub async fn scard_status(&self, device_id: u32, handle: ScardCardHandle) -> Result<StatusReturn> {
        let call = ScardCall::StatusCall(StatusCall {
            handle,
            reader_names_is_null: false,
            reader_length: 0xFFFF_FFFF, // SCARD_AUTOALLOCATE
            atr_length: 32,
        });
        let buf = self.scard_call(device_id, ScardIoCtlCode::StatusW, call, 4096).await?;
        let ret =
            StatusReturn::decode(&mut ReadCursor::new(&buf), CharacterSet::Unicode).map_err(|e| anyhow!("{e}"))?;
        scard_ok(ret.return_code, "status")?;
        Ok(ret)
    }

    /// `SCardTransmit` — send one command APDU and return the card's response.
    pub async fn scard_transmit(
        &self,
        device_id: u32,
        handle: ScardCardHandle,
        send_protocol: CardProtocol,
        apdu: &[u8],
        recv_len: u32,
    ) -> Result<Vec<u8>> {
        let send_length = u32::try_from(apdu.len()).context("APDU too large")?;
        // The recv buffer the client should allocate for the response APDU — the
        // caller's actual `*RxLength`, so extended-length responses aren't capped.
        // Clamp to the MS-RDPESC `cbRecvLength` range [1, 0x10000] (0 or >64 KiB
        // are rejected); a sub-256 request is bumped to a sane floor.
        let recv_length = recv_len.clamp(256, 0x1_0000);
        let call = ScardCall::TransmitCall(TransmitCall {
            handle,
            send_pci: SCardIORequest {
                protocol: send_protocol,
                extra_bytes_length: 0,
                extra_bytes: Vec::new(),
            },
            send_length,
            send_buffer: apdu.to_vec(),
            recv_pci: None,
            recv_buffer_is_null: false,
            recv_length,
        });
        let output_buffer_length = recv_length.saturating_add(1024);
        let buf = self
            .scard_call(device_id, ScardIoCtlCode::Transmit, call, output_buffer_length)
            .await?;
        let ret = TransmitReturn::decode(&mut ReadCursor::new(&buf)).map_err(|e| anyhow!("{e}"))?;
        scard_ok(ret.return_code, "transmit")?;
        Ok(ret.recv_buffer)
    }

    /// `SCardDisconnect` with one of the `SCARD_*_CARD` dispositions.
    pub async fn scard_disconnect(&self, device_id: u32, handle: ScardCardHandle, disposition: u32) -> Result<()> {
        let call = ScardCall::HCardAndDispositionCall(HCardAndDispositionCall { handle, disposition });
        let buf = self
            .scard_call(device_id, ScardIoCtlCode::Disconnect, call, 256)
            .await?;
        let ret = LongReturn::decode(&mut ReadCursor::new(&buf)).map_err(|e| anyhow!("{e}"))?;
        scard_ok(ret.return_code, "disconnect")
    }

    fn send_scard(&self, req: ScardControlRequest) -> Result<()> {
        let msg = SvcMessage::from(req);
        self.sender
            .send(ServerEvent::Rdpdr(RdpdrServerMessage::SendMessages(vec![msg])))
            .map_err(|_| anyhow!("RDPDR event channel closed"))
    }

    /// Ship a DR_CONTROL_REQ carrying `call`, await the matching completion, check
    /// the transport `NtStatus`, skip the `OutputBufferLength`, and return the
    /// RPCE return buffer for the caller to decode.
    async fn scard_call(
        &self,
        device_id: u32,
        io_ctl_code: ScardIoCtlCode,
        call: ScardCall,
        output_buffer_length: u32,
    ) -> Result<Vec<u8>> {
        let (completion_id, rx) = self.router.register();
        let req = ScardControlRequest::new(device_id, completion_id, io_ctl_code, call, output_buffer_length);
        self.send_scard(req)?;
        let body = rx.await.context("RDPDR scard: connection closed")?;
        let mut src = ReadCursor::new(&body);
        let resp = DeviceIoResponse::decode(&mut src).map_err(|e| anyhow!("{e}"))?;
        if resp.io_status != NtStatus::SUCCESS {
            return Err(RdpdrStatus {
                op: format!("scard {io_ctl_code:?}"),
                status: resp.io_status,
            }
            .into());
        }
        if src.remaining().len() < size_of::<u32>() {
            return Err(anyhow!("scard {io_ctl_code:?}: response missing OutputBufferLength"));
        }
        let _output_buffer_length = src.read_u32();
        Ok(src.remaining().to_vec())
    }
}

/// Map a PC/SC `ReturnCode` to a `Result`, surfacing any non-`Success` as an error.
fn scard_ok(return_code: ReturnCode, op: &str) -> Result<()> {
    if return_code == ReturnCode::Success {
        Ok(())
    } else {
        Err(anyhow!("smart card {op}: SCARD error {return_code:?}"))
    }
}

/// Server-side RDPDR channel processor.
pub struct RdpdrServer {
    backend: Box<dyn RdpdrServerHandler>,
    /// Shown to the client; consumed by the `Debug` impl and later phases.
    computer_name: String,
    /// Server-chosen client id, echoed through the announce handshake.
    client_id: u32,
    /// Shared with [`RdpdrHandle`] — inbound I/O completions are delivered here.
    router: IoRouter,
}

impl fmt::Debug for RdpdrServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RdpdrServer")
            .field("computer_name", &self.computer_name)
            .finish()
    }
}

impl_as_any!(RdpdrServer);

impl RdpdrServer {
    pub fn new(backend: Box<dyn RdpdrServerHandler>, computer_name: String, router: IoRouter) -> Self {
        Self {
            backend,
            computer_name,
            client_id: 0x0000_0001,
            router,
        }
    }

    /// The capabilities the server advertises. The client keeps only the
    /// capabilities the server also advertises, so the Drive capability must be
    /// present for drive redirection to survive.
    fn server_capabilities() -> CoreCapability {
        let mut caps = Capabilities::new(); // GENERAL
        caps.add_drive();
        CoreCapability {
            capabilities: caps.clone_inner(),
            kind: CoreCapabilityKind::ServerCoreCapabilityRequest,
        }
    }

    fn version_and_id(&self, kind: VersionAndIdPduKind) -> RdpdrPdu {
        RdpdrPdu::VersionAndIdPdu(VersionAndIdPdu {
            version_major: VERSION_MAJOR,
            version_minor: VERSION_MINOR_RDP51,
            client_id: self.client_id,
            kind,
        })
    }
}

impl SvcProcessor for RdpdrServer {
    fn channel_name(&self) -> ChannelName {
        ChannelName::from_static(b"rdpdr\0\0\0")
    }

    fn compression_condition(&self) -> CompressionCondition {
        CompressionCondition::WhenRdpDataIsCompressed
    }

    fn start(&mut self) -> PduResult<Vec<SvcMessage>> {
        let announce = self.version_and_id(VersionAndIdPduKind::ServerAnnounceRequest);
        debug!("RDPDR: sending Server Announce Request");
        Ok(vec![SvcMessage::from(announce)])
    }

    fn process(&mut self, payload: &[u8]) -> PduResult<Vec<SvcMessage>> {
        let mut src = ReadCursor::new(payload);
        let header = SharedHeader::decode(&mut src).map_err(|e| decode_err!(e))?;

        match header.packet_id {
            PacketId::CoreClientidConfirm => {
                // Client Announce Reply: record the echoed id, then send BOTH the
                // Server Core Capability Request AND the Server Client ID Confirm.
                // Per MS-RDPEFS 3.3.5.1 the server sends both before the client
                // replies — mstsc waits for the Client ID Confirm and won't send
                // its capability response until it arrives (FreeRDP is lenient and
                // replies after just the capability request, which masked this).
                let reply = VersionAndIdPdu::decode(header, &mut src).map_err(|e| decode_err!(e))?;
                debug!(client_id = reply.client_id, "RDPDR: Client Announce Reply");
                self.client_id = reply.client_id;
                Ok(vec![
                    SvcMessage::from(RdpdrPdu::CoreCapability(Self::server_capabilities())),
                    SvcMessage::from(self.version_and_id(VersionAndIdPduKind::ServerClientIdConfirm)),
                ])
            }
            PacketId::CoreClientName => {
                let name = ClientNameRequest::decode(&mut src).map_err(|e| decode_err!(e))?;
                debug!(?name, "RDPDR: Client Name");
                Ok(Vec::new())
            }
            PacketId::CoreClientCapability => {
                let _resp = CoreCapability::decode(header, &mut src).map_err(|e| decode_err!(e))?;
                debug!("RDPDR: Client Capability Response — signaling user logged on");
                // Client ID Confirm was already sent; User Logged On prompts the
                // client to announce its (post-logon) devices.
                Ok(vec![SvcMessage::from(RdpdrPdu::UserLoggedon)])
            }
            PacketId::CoreDevicelistAnnounce => {
                let announce = ClientDeviceListAnnounce::decode(&mut src).map_err(|e| decode_err!(e))?;
                let devices: Vec<AnnouncedDevice> = announce
                    .device_list
                    .iter()
                    .map(|d| AnnouncedDevice {
                        device_id: d.device_id(),
                        device_type: d.device_type(),
                        name: d.preferred_dos_name().to_owned(),
                    })
                    .collect();
                for d in &devices {
                    info!(device_id = d.device_id, device_type = ?d.device_type, name = %d.name, "RDPDR: client announced device");
                }
                self.backend.on_devices_announced(&devices);
                // Acknowledge every announced device (read-only MVP).
                Ok(announce
                    .device_list
                    .iter()
                    .map(|d| {
                        SvcMessage::from(RdpdrPdu::ServerDeviceAnnounceResponse(ServerDeviceAnnounceResponse {
                            device_id: d.device_id(),
                            result_code: NtStatus::SUCCESS,
                        }))
                    })
                    .collect())
            }
            PacketId::CoreDeviceIoCompletion => {
                // Decode the DeviceIoResponse header to route by completion id;
                // deliver the whole body to the waiter, which decodes the
                // specific response type it asked for.
                let body = src.remaining();
                match DeviceIoResponse::decode(&mut ReadCursor::new(body)) {
                    Ok(resp) => {
                        debug!(completion_id = resp.completion_id, io_status = ?resp.io_status, "RDPDR: device I/O completion");
                        self.router.deliver(resp.completion_id, body.to_vec());
                    }
                    Err(e) => warn!(error = ?e, "RDPDR: undecodable device I/O completion"),
                }
                Ok(Vec::new())
            }
            other => {
                warn!(packet_id = ?other, "RDPDR: ignoring unhandled packet");
                Ok(Vec::new())
            }
        }
    }
}

impl SvcServerProcessor for RdpdrServer {}

/// The search pattern for an initial query-directory request: the directory
/// path with a trailing `\*` wildcard (relative to the drive root).
fn query_pattern(dir: &str) -> String {
    if dir.is_empty() || dir == "\\" {
        "\\*".to_owned()
    } else if dir.ends_with('\\') {
        format!("{dir}*")
    } else {
        format!("{dir}\\*")
    }
}

/// Build the channel processor + wire the backend's [`RdpdrHandle`]. Called from
/// `RdpServer::attach_channels` with the connection's `ServerEvent` sender.
pub(crate) fn build_rdpdr(factory: &dyn RdpdrServerFactory, sender: mpsc::UnboundedSender<ServerEvent>) -> RdpdrServer {
    let mut backend = factory.build_backend();
    let router = IoRouter::new();
    backend.set_handle(RdpdrHandle::new(sender, router.clone()));
    RdpdrServer::new(backend, factory.computer_name(), router)
}
