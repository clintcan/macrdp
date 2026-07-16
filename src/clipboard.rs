//! Bidirectional clipboard sync between the Mac and the RDP client.
//!
//! Text (CF_UNICODETEXT ↔ NSPasteboardTypeString) and images
//! (CF_DIB ↔ PNG/TIFF) flow both directions. File copy is **Mac → Windows
//! only**: copying a file in Finder advertises FileGroupDescriptorW with
//! the file names + sizes, and Windows can fetch the actual bytes via
//! FileContentsRequest (SIZE for the per-file size query, RANGE for the
//! body chunks). The backend snapshots the absolute paths at format-
//! data-request time so subsequent content requests resolve to the same
//! files even if the pasteboard changes underneath us.
//!
//! The factory owns the event sender and spawns a poller that detects
//! Mac-side clipboard changes via `NSPasteboard.changeCount` and signals
//! the protocol layer.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use std::io::{Cursor, Read, Seek, SeekFrom};

use image::{ImageEncoder, ImageReader};
use ironrdp_cliprdr::backend::{ClipboardMessage, CliprdrBackend, CliprdrBackendFactory};
use ironrdp_cliprdr::pdu::{
    ClipboardFileAttributes, ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags,
    FileContentsFlags, FileContentsRequest, FileContentsResponse, FileDescriptor,
    FormatDataRequest, FormatDataResponse, LockDataId, OwnedFormatDataResponse,
};
use ironrdp_server::{CliprdrServerFactory, ServerEvent, ServerEventSender};
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// Hard ceiling on the number of bytes we'll return for a single RANGE
/// request. mstsc and Microsoft Remote Desktop both chunk at <= 1 MiB in
/// practice; this cap keeps a malicious or buggy peer from getting us to
/// allocate gigabytes per request. We return a short response instead of
/// erroring — the client will just re-request from the next offset.
const MAX_FILE_RANGE_BYTES: u32 = 4 * 1024 * 1024;

type Sender = Arc<Mutex<Option<mpsc::UnboundedSender<ServerEvent>>>>;
type Paths = Arc<Mutex<Vec<PathBuf>>>;

/// Maximum FormatDataResponse payload we'll accept from the client. An
/// authenticated peer that paste-pumped a multi-gig DIB at us could
/// otherwise exhaust memory before any other check kicks in.
const MAX_INCOMING_PAYLOAD: usize = 50 * 1024 * 1024;

/// Convert PNG/TIFF bytes from NSPasteboard into a CF_DIB payload: a
/// `BITMAPINFOHEADER` (40 bytes) followed by 32bpp BGRA pixels in
/// top-down order (negative `biHeight`). 32bpp is the most widely
/// supported variant; we deliberately do not output BITMAPV5HEADER
/// since it complicates color-space negotiation with older clients.
fn png_or_tiff_to_dib(bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    let img = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()?
        .decode()?
        .to_rgba8();
    let (w, h) = (img.width(), img.height());
    let row_bytes = (w as usize) * 4;
    let pixel_bytes = row_bytes * (h as usize);

    let mut out = Vec::with_capacity(40 + pixel_bytes);
    // BITMAPINFOHEADER
    out.extend_from_slice(&40u32.to_le_bytes()); // biSize
    out.extend_from_slice(&(w as i32).to_le_bytes()); // biWidth
    out.extend_from_slice(&(-(h as i32)).to_le_bytes()); // biHeight (negative = top-down)
    out.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    out.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    out.extend_from_slice(&0u32.to_le_bytes()); // biCompression = BI_RGB
    out.extend_from_slice(&(pixel_bytes as u32).to_le_bytes()); // biSizeImage
    out.extend_from_slice(&0u32.to_le_bytes()); // biXPelsPerMeter
    out.extend_from_slice(&0u32.to_le_bytes()); // biYPelsPerMeter
    out.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
    out.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant

    // RGBA → BGRA, row order already top-down.
    for px in img.pixels() {
        let [r, g, b, a] = px.0;
        out.extend_from_slice(&[b, g, r, a]);
    }
    Ok(out)
}

/// Parse a CF_DIB / CF_DIBV5 payload into PNG bytes. We accept any
/// header size ≥ 40 (BITMAPINFOHEADER), 24bpp or 32bpp uncompressed
/// pixels (BI_RGB), top-down or bottom-up. Anything else is rejected
/// with an error.
fn dib_to_png(dib: &[u8]) -> anyhow::Result<Vec<u8>> {
    use anyhow::{anyhow, bail};
    if dib.len() < 40 {
        bail!("DIB shorter than BITMAPINFOHEADER");
    }
    let bi_size = u32::from_le_bytes(dib[0..4].try_into().unwrap()) as usize;
    if bi_size < 40 || bi_size > dib.len() {
        bail!("bogus biSize {bi_size}");
    }
    let width = i32::from_le_bytes(dib[4..8].try_into().unwrap());
    let height_signed = i32::from_le_bytes(dib[8..12].try_into().unwrap());
    let bit_count = u16::from_le_bytes(dib[14..16].try_into().unwrap());
    let compression = u32::from_le_bytes(dib[16..20].try_into().unwrap());

    if width <= 0 {
        bail!("invalid width {width}");
    }
    if height_signed == 0 {
        bail!("invalid height 0");
    }
    // BI_RGB (0) we treat as canonical layout. BI_BITFIELDS (3) we accept
    // for 32bpp under the assumption of standard ARGB masks
    //   (R=0x00FF0000, G=0x0000FF00, B=0x000000FF, A=0xFF000000)
    // — which is the only layout modern Windows actually emits. The masks
    // are stored differently per header version:
    //   BITMAPINFOHEADER (40):       12 bytes of RGB masks AFTER the header
    //   BITMAPV4HEADER  (108):       masks are INSIDE the header
    //   BITMAPV5HEADER  (124):       masks are INSIDE the header
    let bitfields = compression == 3 || compression == 6; // BI_BITFIELDS / BI_ALPHABITFIELDS
    if compression != 0 && !bitfields {
        bail!("unsupported BI_COMPRESSION {compression}");
    }
    if bit_count != 24 && bit_count != 32 {
        bail!("unsupported biBitCount {bit_count}");
    }
    if bitfields && bit_count != 32 {
        bail!("BI_BITFIELDS with biBitCount={bit_count} not supported");
    }

    let w = width as u32;
    let h = height_signed.unsigned_abs();
    let top_down = height_signed < 0;
    let bpp = (bit_count / 8) as usize;
    // BMP rows are padded to a 4-byte multiple.
    let stride = (w as usize * bpp + 3) & !3;
    // Pixel data starts after the header AND any out-of-band masks
    // (BITMAPINFOHEADER + BI_BITFIELDS = masks follow header).
    let mask_bytes = if bitfields && bi_size == 40 {
        if compression == 6 {
            16 // RGBA masks
        } else {
            12 // RGB masks
        }
    } else {
        0
    };
    let pixel_start = bi_size + mask_bytes;
    let need = pixel_start
        .checked_add(
            stride
                .checked_mul(h as usize)
                .ok_or_else(|| anyhow!("overflow"))?,
        )
        .ok_or_else(|| anyhow!("overflow"))?;
    if dib.len() < need {
        bail!("DIB payload truncated: have {}, need {need}", dib.len());
    }

    // Capacity arithmetic must match the byte-bounds checked_mul above —
    // otherwise an attacker could craft a DIB whose dimensions overflow u32
    // and silently allocate a too-small buffer. Vec would still grow on
    // push, so no UB, but be consistent.
    let cap = (w as usize)
        .checked_mul(h as usize)
        .and_then(|n| n.checked_mul(4))
        .ok_or_else(|| anyhow!("RGBA buffer size overflow"))?;
    let mut rgba: Vec<u8> = Vec::with_capacity(cap);
    for row in 0..h {
        let src_row = if top_down { row } else { h - 1 - row };
        let row_off = pixel_start + (src_row as usize) * stride;
        let row_bytes = &dib[row_off..row_off + w as usize * bpp];
        for chunk in row_bytes.chunks_exact(bpp) {
            // BMP pixels are BGR(A); convert to RGBA.
            let (b, g, r, a) = if bpp == 4 {
                (chunk[0], chunk[1], chunk[2], chunk[3])
            } else {
                (chunk[0], chunk[1], chunk[2], 0xFF)
            };
            rgba.extend_from_slice(&[r, g, b, a]);
        }
    }

    let mut png = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut png);
    encoder.write_image(&rgba, w, h, image::ExtendedColorType::Rgba8)?;
    Ok(png)
}

/// Shared state coordinating the Mac-side advertise poller with the cliprdr
/// backend's `on_format_list_response` hook. Lets us retry on Fail while
/// guaranteeing we STOP re-advertising the instant the remote accepts an
/// advertise — a later rejected re-advertise would wipe `local_file_list`
/// inside cliprdr and silently break a paste that was about to work.
#[derive(Debug, Default)]
struct AdvertiseState {
    /// Bumped each time the Mac pasteboard changes; identifies the current
    /// wave of (possibly retried) advertises.
    generation: std::sync::atomic::AtomicU64,
    /// When the remote responds with Ok to one of our format lists, the hook
    /// stores the current `generation` here. The retry loop compares against
    /// its own `my_gen` and stops the moment they match — meaning "this
    /// wave's advertise was accepted, don't send another one."
    locked_gen: std::sync::atomic::AtomicU64,
}

#[derive(Debug)]
pub struct MacCliprdr {
    sender: Sender,
    /// Absolute paths corresponding to the FILEGROUPDESCRIPTORW most recently
    /// pushed to the cliprdr server. Shared with every backend instance so
    /// that `on_file_contents_request` (which runs on the backend) can map
    /// `request.index` back to a real path even when the advertise was sent
    /// from the poller in the factory.
    file_paths: Paths,
    /// Windows→Mac file paste routing. Backend's
    /// `on_file_contents_response` dispatches incoming bytes through this;
    /// each in-flight download task holds the matching receiver. See
    /// `src/file_promise.rs`.
    #[cfg(target_os = "macos")]
    download_router: crate::file_promise::DownloadRouter,
    /// Most recently-allocated temp directory holding downloaded remote
    /// files. The download task swaps it on each new remote copy and
    /// removes the previous tree to keep /tmp tidy.
    #[cfg(target_os = "macos")]
    paste_temp_dir: Arc<Mutex<Option<std::path::PathBuf>>>,
    /// The `NSPasteboard.changeCount` value we set the last time we
    /// published remote files. The poller compares this against the
    /// current changeCount and skips its tick if equal, so we don't see
    /// our own write and bounce it back to Windows.
    #[cfg(target_os = "macos")]
    self_change_count: crate::file_promise::SelfChangeCount,
    /// Coordinates the advertise retry loop with the cliprdr backend's
    /// `on_format_list_response` hook. See [`AdvertiseState`].
    advertise_state: Arc<AdvertiseState>,
    /// Number of live cliprdr backends — i.e. connected clients with an
    /// active clipboard channel. Incremented in `build_cliprdr_backend`
    /// (per connection), decremented in the backend's `Drop` (disconnect).
    /// The pasteboard poller parks while this is 0, so an idle macrdp
    /// doesn't do an NSPasteboard round-trip 4×/s forever from process
    /// start — that poller was the sole reason a zero-client server wasn't
    /// ~0% idle.
    active_backends: Arc<std::sync::atomic::AtomicUsize>,
    /// When true (default), on_remote_file_list dispatches to
    /// `file_promise_lazy::spawn_lazy_paste`. Set false via
    /// `--no-lazy-paste` to use the eager path instead. Single-file and
    /// folder copies both work in lazy; entries without a size hint
    /// fall back to eager automatically.
    #[cfg(target_os = "macos")]
    lazy_paste: bool,
}

#[cfg(target_os = "macos")]
impl MacCliprdr {
    pub fn new(lazy_paste: bool) -> Self {
        let paste_temp_dir = Arc::new(Mutex::new(None));
        let self_change_count = Arc::new(std::sync::atomic::AtomicI64::new(-1));
        // Publish to the process-global so the signal-exit watcher in
        // main.rs can call cleanup_on_disconnect before
        // std::process::exit(0) (which bypasses Drop).
        crate::file_promise_lazy::register_shutdown_cleanup(
            paste_temp_dir.clone(),
            self_change_count.clone(),
        );
        Self {
            sender: Arc::new(Mutex::new(None)),
            file_paths: Arc::new(Mutex::new(Vec::new())),
            download_router: crate::file_promise::DownloadRouter::default(),
            paste_temp_dir,
            self_change_count,
            advertise_state: Arc::new(AdvertiseState::default()),
            active_backends: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            lazy_paste,
        }
    }
}

#[cfg(not(target_os = "macos"))]
impl MacCliprdr {
    pub fn new() -> Self {
        Self {
            sender: Arc::new(Mutex::new(None)),
            file_paths: Arc::new(Mutex::new(Vec::new())),
            advertise_state: Arc::new(AdvertiseState::default()),
            active_backends: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }
}

impl ServerEventSender for MacCliprdr {
    fn set_sender(&mut self, sender: mpsc::UnboundedSender<ServerEvent>) {
        *self.sender.lock().unwrap() = Some(sender);

        // Spawn a poller that notices Mac-side copies and tells the RDP
        // server to advertise the new content to the remote.
        let sender_arc = self.sender.clone();
        let paths_arc = self.file_paths.clone();
        #[cfg(target_os = "macos")]
        let self_cc = self.self_change_count.clone();
        let advertise_state = self.advertise_state.clone();
        let active_backends = self.active_backends.clone();
        tokio::spawn(async move {
            // NSPasteboard.changeCount is monotonic; record the starting
            // value so we don't fire an event for whatever was already on
            // the clipboard when macrdp launched.
            let mut last_seen = pb::change_count();
            loop {
                // Park while no client has a clipboard channel: there is
                // nobody to advertise to, so the NSPasteboard IPC 4×/s is
                // pure idle wakeups (this poller starts at process launch —
                // the server constructor calls set_sender — and used to run
                // forever). Idle at 1 Hz on a cheap atomic instead; on
                // connect it resumes within a second, and a copy made while
                // parked is advertised THEN (the fresh client learns the
                // current Mac clipboard, which is also the better behavior).
                if active_backends.load(std::sync::atomic::Ordering::Relaxed) == 0 {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    continue;
                }
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                let current = pb::change_count();
                if current == last_seen {
                    continue;
                }
                last_seen = current;
                // If the latest bump is from OUR remote-paste publish,
                // skip — otherwise we'd advertise the just-pasted Windows
                // files back to Windows as a fresh Mac→Windows copy.
                #[cfg(target_os = "macos")]
                if current == self_cc.load(std::sync::atomic::Ordering::Relaxed) {
                    debug!(current, "skipping pasteboard tick (self-write)");
                    continue;
                }
                // Start a new advertise wave. `my_gen` identifies it for the
                // retry loop and for the cliprdr backend's
                // `on_format_list_response` hook, which stamps `locked_gen` with
                // the current generation on `Ok` so we can stop re-advertising.
                use std::sync::atomic::Ordering;
                let my_gen = advertise_state.generation.fetch_add(1, Ordering::Relaxed) + 1;

                if !advertise_pasteboard(&sender_arc, &paths_arc) {
                    break;
                }

                // Retry on Fail, STOP on Ok. mstsc commonly rejects the first
                // advertise right after an in-session Cmd-C (it's still
                // processing the input) and accepts one ~0.5–1 s later. We
                // MUST stop re-advertising the instant the remote accepts —
                // otherwise a later re-advertise that gets rejected wipes
                // `local_file_list` inside cliprdr and silently breaks an
                // otherwise-working paste. The delays are sized to give each
                // response time to arrive before the next retry decision.
                // Supersede check handles a newer copy starting mid-wave.
                for delay in [
                    std::time::Duration::from_millis(1000),
                    std::time::Duration::from_millis(2500),
                    std::time::Duration::from_millis(5000),
                ] {
                    tokio::time::sleep(delay).await;
                    if advertise_state.generation.load(Ordering::Relaxed) != my_gen {
                        // A newer Mac-side copy started a new wave; let the
                        // main loop handle it on its next tick.
                        break;
                    }
                    if advertise_state.locked_gen.load(Ordering::Relaxed) == my_gen {
                        // Remote acknowledged our advertise. Do NOT send
                        // another format list — a rejection of that one would
                        // wipe the accepted state.
                        debug!(my_gen, "format list accepted; retry loop done");
                        break;
                    }
                    if !advertise_pasteboard(&sender_arc, &paths_arc) {
                        return;
                    }
                }
            }
        });
    }
}

impl CliprdrBackendFactory for MacCliprdr {
    fn build_cliprdr_backend(&self) -> Box<dyn CliprdrBackend> {
        // Un-park the pasteboard poller: a client just brought up its
        // clipboard channel. Balanced by the decrement in the backend's Drop.
        self.active_backends
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Box::new(MacCliprdrBackend {
            sender: self.sender.clone(),
            last_requested: None,
            active_backends: self.active_backends.clone(),
            file_paths: self.file_paths.clone(),
            #[cfg(target_os = "macos")]
            download_router: self.download_router.clone(),
            #[cfg(target_os = "macos")]
            paste_temp_dir: self.paste_temp_dir.clone(),
            #[cfg(target_os = "macos")]
            self_change_count: self.self_change_count.clone(),
            advertise_state: self.advertise_state.clone(),
            #[cfg(target_os = "macos")]
            lazy_paste: self.lazy_paste,
        })
    }
}

impl CliprdrServerFactory for MacCliprdr {}

#[derive(Debug)]
struct MacCliprdrBackend {
    sender: Sender,
    // Format we last asked the remote to send us. on_format_data_response
    // doesn't include the format ID, so we keep it here to know whether to
    // decode the payload as UTF-16 text or as a DIB.
    last_requested: Option<ClipboardFormatId>,
    // Live-backend counter shared with `MacCliprdr` — incremented when this
    // backend was built, decremented in Drop. Parks the pasteboard poller
    // while no client has a clipboard channel. See MacCliprdr::active_backends.
    active_backends: Arc<std::sync::atomic::AtomicUsize>,
    // Shared with `MacCliprdr` so the poller and the backend agree on which
    // paths back the currently-advertised FILEGROUPDESCRIPTORW.
    file_paths: Paths,
    // Windows→Mac side: route FileContentsResponses to whichever download
    // task is awaiting the matching stream_id.
    #[cfg(target_os = "macos")]
    download_router: crate::file_promise::DownloadRouter,
    // Latest paste temp dir (cleaned + recreated per remote copy). See
    // `MacCliprdr::paste_temp_dir`.
    #[cfg(target_os = "macos")]
    paste_temp_dir: Arc<Mutex<Option<std::path::PathBuf>>>,
    // Set by the download task after writing remote files to NSPasteboard
    // so the poller can skip its own write. See `MacCliprdr::self_change_count`.
    #[cfg(target_os = "macos")]
    self_change_count: crate::file_promise::SelfChangeCount,
    // Lets the `on_format_list_response` hook tell the poller's retry loop
    // that the current advertise wave was accepted, so it stops re-advertising.
    advertise_state: Arc<AdvertiseState>,
    // Routes on_remote_file_list to the lazy NSFilePresenter path when
    // true (the default; --no-lazy-paste flips it). See MacCliprdr::lazy_paste.
    #[cfg(target_os = "macos")]
    lazy_paste: bool,
}

/// Drop runs when the RDP connection ends and ironrdp_server releases
/// the per-session backend box. macrdp serves one client at a time, so
/// this also doubles as our "client disconnected" hook: tear down lazy
/// paste presenters, blow away the per-paste temp dir, and clear the
/// pasteboard if our URLs are still on it (otherwise NSPasteboard would
/// be holding `file:///tmp/macrdp-lazy-paste-…/foo` URLs whose backing
/// files we just deleted — Finder paste would error out for the user).
///
/// Best-effort: presenter removal is async (hops to the runloop thread),
/// and temp-dir removal is std::fs blocking — we do NOT wait on either,
/// because Drop runs synchronously on the ironrdp_server task thread
/// and we don't want to stall the disconnect path.
impl Drop for MacCliprdrBackend {
    fn drop(&mut self) {
        // Re-park the pasteboard poller if this was the last connected
        // clipboard channel (balances build_cliprdr_backend's increment).
        self.active_backends
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        #[cfg(target_os = "macos")]
        crate::file_promise_lazy::cleanup_on_disconnect(
            &self.paste_temp_dir,
            &self.self_change_count,
        );
    }
}

impl ironrdp_core::AsAny for MacCliprdrBackend {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
}

impl MacCliprdrBackend {
    fn push(&self, msg: ClipboardMessage) {
        if let Some(s) = self.sender.lock().unwrap().as_ref() {
            let _ = s.send(ServerEvent::Clipboard(msg));
        }
    }

    /// Serve a single FileContentsRequest against the path snapshot built
    /// during the most recent file-copy advertise. Returns `None` on any
    /// failure so the caller can synthesize a CB_RESPONSE_FAIL.
    fn serve_file_contents(
        &self,
        request: FileContentsRequest,
    ) -> Option<FileContentsResponse<'static>> {
        let idx = usize::try_from(request.index).ok()?;
        let path = {
            let guard = self.file_paths.lock().unwrap();
            guard.get(idx).cloned()?
        };
        let meta = std::fs::metadata(&path)
            .map_err(|e| warn!(?path, "metadata failed: {e}"))
            .ok()?;
        // Directories appear in FILEGROUPDESCRIPTORW so the client can render
        // them in the paste UI, but they aren't byte-readable. Phase 3 (if
        // we ever do recursive directory copy) would generate per-entry
        // descriptors with relative_path set instead.
        if meta.is_dir() {
            debug!(?path, "file contents requested on a directory; refusing");
            return None;
        }
        if request.flags.contains(FileContentsFlags::SIZE) {
            debug!(
                stream = request.stream_id,
                ?path,
                size = meta.len(),
                "SIZE response"
            );
            return Some(FileContentsResponse::new_size_response(
                request.stream_id,
                meta.len(),
            ));
        }
        if request.flags.contains(FileContentsFlags::RANGE) {
            let bytes = read_file_range(&path, request.position, request.requested_size)
                .map_err(|e| warn!(?path, "read failed: {e}"))
                .ok()?;
            debug!(
                stream = request.stream_id,
                ?path,
                position = request.position,
                returned = bytes.len(),
                "RANGE response",
            );
            return Some(FileContentsResponse::new_data_response(
                request.stream_id,
                bytes,
            ));
        }
        // Upstream's decode rejects flag combinations other than exactly-
        // one-of {SIZE, RANGE}, so this is unreachable in practice.
        None
    }
}

/// Read the current Mac pasteboard and push the appropriate "we have
/// something to copy" event into the server. Returns `false` if the sender
/// has been dropped (i.e. the server is shutting down) so callers know to
/// stop polling.
///
/// File copies and non-file copies take different code paths inside
/// ironrdp-cliprdr: a regular format list goes via `SendInitiateCopy`, but
/// file lists must go via `initiate_file_copy` (exposed here through the
/// vendored `ServerEvent::ClipboardFileCopy` variant) so that the cliprdr
/// server populates its `local_file_list` and accepts subsequent
/// FileContentsRequests instead of short-circuiting them with
/// CB_RESPONSE_FAIL.
fn advertise_pasteboard(sender: &Sender, paths: &Paths) -> bool {
    if pb::has_files() {
        let entries = pb::read_files();
        if !entries.is_empty() {
            let mut snapshot = Vec::with_capacity(entries.len());
            let mut files = Vec::with_capacity(entries.len());
            for e in entries {
                let mut fd = FileDescriptor::new(e.name);
                if let Some(rp) = e.relative_path {
                    fd = fd.with_relative_path(rp);
                }
                if e.is_dir {
                    fd = fd.with_attributes(ClipboardFileAttributes::DIRECTORY);
                } else {
                    fd = fd.with_attributes(ClipboardFileAttributes::NORMAL);
                    if let Some(sz) = e.size {
                        fd = fd.with_file_size(sz);
                    }
                }
                files.push(fd);
                snapshot.push(e.path);
            }
            *paths.lock().unwrap() = snapshot;
            debug!(
                file_count = files.len(),
                "advertising file copy to client (recursive)"
            );
            return send(sender, ServerEvent::ClipboardFileCopy(files));
        }
        // Files claimed but read empty (race) — fall through to format list.
    }

    let mut formats = Vec::new();
    if pb::has_image() {
        formats.push(ClipboardFormat::new(ClipboardFormatId::CF_DIB));
    }
    if pb::has_string() {
        formats.push(ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT));
    }
    if formats.is_empty() {
        // Nothing to advertise but the sender is presumably still alive.
        return true;
    }
    // Clear any stale file-paths snapshot so a leftover index can't be
    // exploited by a slow follow-up FileContentsRequest.
    paths.lock().unwrap().clear();
    send(
        sender,
        ServerEvent::Clipboard(ClipboardMessage::SendInitiateCopy(formats)),
    )
}

fn send(sender: &Sender, event: ServerEvent) -> bool {
    let guard = sender.lock().unwrap();
    match guard.as_ref() {
        Some(s) => s.send(event).is_ok(),
        None => false,
    }
}

/// Read a `position..position+requested_size` slice out of `path`,
/// honoring `MAX_FILE_RANGE_BYTES` and returning a short read at EOF.
/// Centralized so the SIZE/RANGE handler logic stays compact and the read
/// path has unit tests of its own.
fn read_file_range(
    path: &std::path::Path,
    position: u64,
    requested_size: u32,
) -> std::io::Result<Vec<u8>> {
    let cap = requested_size.min(MAX_FILE_RANGE_BYTES) as usize;
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(position))?;
    let mut buf = vec![0u8; cap];
    let mut filled = 0usize;
    // Loop because `Read::read` is allowed to return a short read even
    // before EOF; we want to either fill the buffer or stop at EOF.
    while filled < cap {
        match f.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    buf.truncate(filled);
    Ok(buf)
}

impl CliprdrBackend for MacCliprdrBackend {
    fn temporary_directory(&self) -> &str {
        "/tmp"
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        // STREAM_FILECLIP_ENABLED is the gate that lets either side use
        // FileGroupDescriptorW + FileContents{Request,Response}. Without
        // it, clients won't advertise file paste at all.
        //
        // CAN_LOCK_CLIPDATA enables cliprdr's automatic Lock/Unlock cycle
        // around incoming file-list pastes ([MS-RDPECLIP] 1.3.2.3 / Figure
        // 3). Without it, the upstream `send_lock` short-circuits (see
        // vendor/ironrdp-cliprdr/src/lib.rs:929) and Windows Explorer is
        // never told when we're "done" with a FileGroupDescriptorW it
        // gave us. Symptom on mstsc: after a successful file paste, a
        // rapid follow-up Ctrl-C in Windows is silently dropped (no
        // FormatList reaches the Mac), and very large downloads can be
        // released mid-stream (CB_RESPONSE_FAIL) when the source app
        // decides the descriptor isn't being held. Advertising the cap
        // lets cliprdr issue LockData on the incoming format list and
        // UnlockData on supersession/timeout automatically.
        ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED
            | ClipboardGeneralCapabilityFlags::CAN_LOCK_CLIPDATA
    }

    fn on_ready(&mut self) {
        advertise_pasteboard(&self.sender, &self.file_paths);
    }

    fn on_request_format_list(&mut self) {
        advertise_pasteboard(&self.sender, &self.file_paths);
    }

    fn on_format_list_response(&mut self, ok: bool) {
        // mstsc commonly rejects an advertise sent right after an in-session
        // Cmd-C (it's still processing the keystroke), then accepts one a
        // moment later. The poller retries on Fail; we ONLY mark the wave
        // locked on Ok so a subsequent retry that would otherwise wipe an
        // accepted state is suppressed. Stamping with the *current*
        // generation matches what the retry loop checks against.
        use std::sync::atomic::Ordering;
        if ok {
            let gen = self.advertise_state.generation.load(Ordering::Relaxed);
            self.advertise_state
                .locked_gen
                .store(gen, Ordering::Relaxed);
            debug!(gen, "remote accepted our format list");
        } else {
            debug!("remote rejected our format list (will be retried by poller)");
        }
    }

    fn on_process_negotiated_capabilities(
        &mut self,
        capabilities: ClipboardGeneralCapabilityFlags,
    ) {
        // The flags here are the AND of what we advertised and what the
        // client advertised. If STREAM_FILECLIP_ENABLED is missing, file
        // paste will silently fail downstream with CB_RESPONSE_FAIL — log
        // it once so the cause is obvious in the trace.
        let has_stream_files =
            capabilities.contains(ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED);
        tracing::info!(
            ?capabilities,
            file_clipboard_negotiated = has_stream_files,
            "clipboard capabilities negotiated"
        );
    }

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        // Remote (e.g. Windows) put something on its clipboard. Files are
        // checked first because a Finder paste of files is the richer
        // experience; image and text fall back if the remote didn't copy a
        // file. FileGroupDescriptorW is identified by *name* per
        // MS-RDPECLIP — the numeric format ID is assigned by the remote and
        // varies, but the name is constant across all implementations.
        if let Some(fmt) = available_formats.iter().find(|f| {
            f.name
                .as_ref()
                .map(|n| n.value() == "FileGroupDescriptorW")
                .unwrap_or(false)
        }) {
            debug!(format_id = ?fmt.id, "remote advertised files; requesting file list");
            self.last_requested = Some(fmt.id);
            self.push(ClipboardMessage::SendInitiatePaste(fmt.id));
            return;
        }

        // Image/text fall-back. Prefer DIBV5 over DIB (better color), then
        // text. Asking for one format doesn't preclude later asking for
        // another; we only need the user's single paste action so the first
        // match wins.
        let priority = [
            ClipboardFormatId::CF_DIBV5,
            ClipboardFormatId::CF_DIB,
            ClipboardFormatId::CF_UNICODETEXT,
        ];
        for pref in priority {
            if let Some(fmt) = available_formats.iter().find(|f| f.id == pref) {
                self.last_requested = Some(fmt.id);
                self.push(ClipboardMessage::SendInitiatePaste(fmt.id));
                return;
            }
        }
    }

    fn on_remote_file_list(&mut self, files: &[FileDescriptor], clip_data_id: Option<u32>) {
        debug!(
            file_count = files.len(),
            clip_data_id, "remote file list received"
        );
        #[cfg(target_os = "macos")]
        {
            let entries: Vec<crate::file_promise::RemoteEntry> = files
                .iter()
                .enumerate()
                .map(|(i, f)| {
                    let is_dir = f
                        .attributes
                        .map(|a| a.contains(ClipboardFileAttributes::DIRECTORY))
                        .unwrap_or(false);
                    crate::file_promise::RemoteEntry {
                        index: i as i32,
                        name: f.name.clone(),
                        size: f.file_size,
                        is_dir,
                        relative_path: f.relative_path.clone(),
                    }
                })
                .collect();
            let rt = tokio::runtime::Handle::current();
            // Try lazy first if enabled; it returns false for folder
            // copies (Phase 1 scope) and we fall through to eager.
            let mut handled = false;
            if self.lazy_paste {
                debug!("attempting lazy paste path");
                handled = crate::file_promise_lazy::spawn_lazy_paste(
                    entries.clone(),
                    self.download_router.clone(),
                    self.sender.clone(),
                    self.paste_temp_dir.clone(),
                    self.self_change_count.clone(),
                    rt.clone(),
                );
            }
            if !handled {
                debug!("dispatching eager paste path");
                crate::file_promise::spawn_remote_paste(
                    entries,
                    self.download_router.clone(),
                    self.sender.clone(),
                    self.paste_temp_dir.clone(),
                    self.self_change_count.clone(),
                    rt,
                );
            }
        }
        #[cfg(not(target_os = "macos"))]
        let _ = (files, clip_data_id);
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        // FileGroupDescriptorW is handled internally by upstream cliprdr
        // once we go through `initiate_file_copy` (the
        // ServerEvent::ClipboardFileCopy path) — it answers the FormatData
        // request from its stored `local_file_list` without ever reaching
        // us. So we only deal with CF_UNICODETEXT and CF_DIB here.
        let response = match request.format {
            ClipboardFormatId::CF_UNICODETEXT => match pb::read_string() {
                Some(s) => {
                    let mut units: Vec<u16> = s.encode_utf16().collect();
                    units.push(0);
                    let mut bytes = Vec::with_capacity(units.len() * 2);
                    for u in units {
                        bytes.extend_from_slice(&u.to_le_bytes());
                    }
                    OwnedFormatDataResponse::new_data(bytes)
                }
                None => OwnedFormatDataResponse::new_error(),
            },
            ClipboardFormatId::CF_DIB => match pb::read_image_bytes() {
                Some((_enc, bytes)) => match png_or_tiff_to_dib(&bytes) {
                    Ok(dib) => OwnedFormatDataResponse::new_data(dib),
                    Err(e) => {
                        warn!("DIB encode failed: {e}");
                        OwnedFormatDataResponse::new_error()
                    }
                },
                None => OwnedFormatDataResponse::new_error(),
            },
            other => {
                debug!(?other, "unsupported format requested by remote");
                OwnedFormatDataResponse::new_error()
            }
        };
        self.push(ClipboardMessage::SendFormatData(response));
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        let requested = self.last_requested.take();
        if response.is_error() {
            warn!("remote returned error for format data");
            return;
        }
        let data = response.data();
        if data.len() > MAX_INCOMING_PAYLOAD {
            warn!(
                len = data.len(),
                cap = MAX_INCOMING_PAYLOAD,
                "clipboard payload exceeds cap; dropping"
            );
            return;
        }
        match requested {
            Some(ClipboardFormatId::CF_UNICODETEXT) | None => {
                // Default to text if we don't know what we asked for —
                // matches the previous text-only behaviour.
                if data.len() % 2 != 0 {
                    warn!(len = data.len(), "odd-length UTF-16 payload");
                    return;
                }
                let mut units: Vec<u16> = data
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                if matches!(units.last(), Some(0)) {
                    units.pop();
                }
                match String::from_utf16(&units) {
                    Ok(s) => {
                        debug!(
                            len = s.len(),
                            "writing remote clipboard text to NSPasteboard"
                        );
                        pb::write_string(&s);
                    }
                    Err(e) => warn!("UTF-16 decode failed: {e}"),
                }
            }
            Some(ClipboardFormatId::CF_DIB) | Some(ClipboardFormatId::CF_DIBV5) => {
                match dib_to_png(data) {
                    Ok(png) => {
                        debug!(
                            len = png.len(),
                            "writing remote clipboard image to NSPasteboard"
                        );
                        pb::write_png(&png);
                    }
                    Err(e) => warn!("DIB decode failed: {e}"),
                }
            }
            Some(other) => {
                warn!(?other, "unexpected format in data response");
            }
        }
    }

    fn on_file_contents_request(&mut self, request: FileContentsRequest) {
        let stream_id = request.stream_id;
        let response = self
            .serve_file_contents(request)
            .unwrap_or_else(|| FileContentsResponse::new_error(stream_id));
        self.push(ClipboardMessage::SendFileContentsResponse(response));
    }
    fn on_file_contents_response(&mut self, response: FileContentsResponse<'_>) {
        // Owned copy so we can hand it to the awaiting task (which lives
        // past the lifetime of this borrow).
        #[cfg(target_os = "macos")]
        {
            use ironrdp_core::IntoOwned;
            self.download_router.deliver(response.into_owned());
        }
        #[cfg(not(target_os = "macos"))]
        let _ = response;
    }
    fn on_lock(&mut self, _data_id: LockDataId) {}
    fn on_unlock(&mut self, _data_id: LockDataId) {}

    /// Fires once per Windows clipboard transition, with the lock IDs
    /// that just expired. This is our "Windows clipboard changed"
    /// signal regardless of whether the new content carries
    /// `FileGroupDescriptorW`. We use it to clear the Mac pasteboard
    /// if our previously-published URLs are still on it — so when a
    /// shell extension (e.g. for `.zip`/`.gz`/`.7z` archives) swallows
    /// the file representation on the Windows side, Cmd-V in Finder
    /// beeps clearly instead of silently pasting the previous file.
    /// In-flight downloads from the prior paste are not disturbed:
    /// they hold strong refs into REGISTRY and can complete on their
    /// own; presenters tied to URLs that just left the pasteboard are
    /// effectively zombies that get reaped on the next supersede.
    #[cfg(target_os = "macos")]
    fn on_outgoing_locks_expired(&mut self, _clip_data_ids: &[LockDataId]) {
        crate::file_promise_lazy::clear_pasteboard_if_stale(&self.self_change_count);
    }
}

/// Serializes every access to the process-global `NSPasteboard`. AppKit's
/// pasteboard is NOT thread-safe — its internal type cache
/// (`_updateTypeCacheIfNeeded`) corrupts if a reader (the advertise poller
/// walking `pasteboardItems()`/`types()`) overlaps a writer (`clearContents` /
/// `writeObjects` / `setData` from the paste, download, or disconnect-Drop
/// paths, all on different threads), which segfaulted `objc_msgSend` during
/// connection churn. Every `pb::` accessor and every `file_promise*`
/// pasteboard touch holds this for the span of its raw objc calls; results are
/// copied into owned Rust types before the guard drops, so the lock only spans
/// the unsafe access. Poison is recovered (the guarded data is `()` — there is
/// no state to corrupt) so one panicking access can't cascade-poison the rest.
#[cfg(target_os = "macos")]
pub(crate) fn pasteboard_guard() -> std::sync::MutexGuard<'static, ()> {
    static PB_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    PB_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(target_os = "macos")]
mod pb {
    use objc2::rc::autoreleasepool;
    use objc2_app_kit::{
        NSPasteboard, NSPasteboardTypeFileURL, NSPasteboardTypePNG, NSPasteboardTypeString,
        NSPasteboardTypeTIFF,
    };
    use objc2_foundation::{NSData, NSString, NSURL};

    pub fn change_count() -> i64 {
        let _pb_guard = super::pasteboard_guard();
        unsafe {
            let pb = NSPasteboard::generalPasteboard();
            pb.changeCount() as i64
        }
    }

    pub fn has_string() -> bool {
        unsafe { has_type(NSPasteboardTypeString) }
    }

    pub fn has_image() -> bool {
        unsafe { has_type(NSPasteboardTypePNG) || has_type(NSPasteboardTypeTIFF) }
    }

    pub fn has_files() -> bool {
        unsafe { has_type(NSPasteboardTypeFileURL) }
    }

    pub struct FileEntry {
        pub name: String,
        pub size: Option<u64>,
        pub is_dir: bool,
        pub path: std::path::PathBuf,
        /// MS-RDPECLIP relative directory path within the copied root,
        /// using `\` as the separator (e.g. `MyFolder\sub`). `None` for the
        /// top-level entries that came directly off the pasteboard.
        pub relative_path: Option<String>,
    }

    /// Cap on total descriptors produced by one pasteboard read. Upstream
    /// `PackedFileList` rejects beyond `MAX_FILE_COUNT = 100_000`, but we cut
    /// off earlier — paste of a giant tree (e.g. node_modules) shouldn't
    /// stall the advertise round-trip for tens of seconds.
    const MAX_FILES_PER_COPY: usize = 10_000;

    /// Return one entry per file URL item on the general pasteboard, with
    /// directories expanded recursively. Cocoa stores multi-file selections
    /// as one pasteboard item per file; for any item that resolves to a
    /// directory we emit one entry for the directory itself plus one for
    /// each descendant, with `relative_path` set so the wire `cFileName`
    /// reconstructs the full path inside the copied root.
    ///
    /// Symlinks are skipped entirely (both as top-level items and inside a
    /// walked directory) to avoid following them into unintended paths and
    /// to prevent cycles. Unreadable paths or directories we can't open are
    /// logged but don't abort the rest of the walk.
    pub fn read_files() -> Vec<FileEntry> {
        let _pb_guard = super::pasteboard_guard();
        autoreleasepool(|_| unsafe {
            let pb = NSPasteboard::generalPasteboard();
            let Some(items) = pb.pasteboardItems() else {
                return Vec::new();
            };
            let mut out: Vec<FileEntry> = Vec::new();
            'items: for i in 0..items.count() {
                let item = items.objectAtIndex(i);
                let Some(url_str) = item.stringForType(NSPasteboardTypeFileURL) else {
                    continue;
                };
                let Some(path) = resolve_file_url(&url_str) else {
                    continue;
                };
                let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_owned)
                else {
                    continue;
                };
                // symlink_metadata so we don't transparently follow a
                // top-level symlink into someone else's filesystem subtree.
                let meta = match std::fs::symlink_metadata(&path) {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!(?path, "metadata failed for pasteboard item: {e}");
                        continue;
                    }
                };
                if meta.file_type().is_symlink() {
                    tracing::debug!(?path, "skipping symlink on pasteboard");
                    continue;
                }
                let is_dir = meta.is_dir();
                let size = if is_dir { None } else { Some(meta.len()) };
                out.push(FileEntry {
                    name: name.clone(),
                    size,
                    is_dir,
                    path: path.clone(),
                    relative_path: None,
                });
                if out.len() >= MAX_FILES_PER_COPY {
                    break 'items;
                }
                if is_dir && !walk_inner(&mut out, &path, name) {
                    break 'items;
                }
            }
            if out.len() >= MAX_FILES_PER_COPY {
                tracing::warn!(
                    cap = MAX_FILES_PER_COPY,
                    "file list truncated at cap; deeper entries omitted from this paste"
                );
            }
            out
        })
    }

    /// Recursively append entries under `dir` to `out`. `relative_prefix`
    /// is the wire-format directory path (using `\` separators) describing
    /// `dir`'s location relative to the copied root — for example, when
    /// expanding the top-level pasteboard item `MyFolder`, the first call
    /// passes `relative_prefix = "MyFolder"`; descending into `MyFolder/sub`
    /// recurses with `"MyFolder\\sub"`. Returns `false` if the per-copy cap
    /// was hit so the caller can stop the outer walk.
    pub(super) fn walk_inner(
        out: &mut Vec<FileEntry>,
        dir: &std::path::Path,
        relative_prefix: String,
    ) -> bool {
        let entries = match std::fs::read_dir(dir) {
            Ok(it) => it,
            Err(e) => {
                tracing::warn!(?dir, "skipping unreadable directory: {e}");
                return true;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
                continue;
            };
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            let is_dir = meta.is_dir();
            let size = if is_dir { None } else { Some(meta.len()) };
            out.push(FileEntry {
                name: name.clone(),
                size,
                is_dir,
                path: path.clone(),
                relative_path: Some(relative_prefix.clone()),
            });
            if out.len() >= MAX_FILES_PER_COPY {
                return false;
            }
            if is_dir {
                let nested = format!("{relative_prefix}\\{name}");
                if !walk_inner(out, &path, nested) {
                    return false;
                }
            }
        }
        true
    }

    /// Turn a `NSPasteboardTypeFileURL` string into an absolute filesystem
    /// path. Finder hands us either a percent-encoded `file:///Users/...`
    /// URL or — frequently — a *file-reference* URL of the form
    /// `file:///.file/id=NNNN.MMMM`. The latter can't be stat'd directly
    /// (`/.file/id=…` is a volfs magic mount that only resolves through
    /// the kernel's NSURL machinery), so we let NSURL convert it before
    /// handing the result back to Rust's std::fs.
    fn resolve_file_url(url_str: &NSString) -> Option<std::path::PathBuf> {
        unsafe {
            let url = NSURL::URLWithString(url_str)?;
            // `URLByResolvingSymlinksInPath` is what turns the file-
            // reference form into a real `/Users/...` URL; it is a no-op
            // for already-resolved URLs.
            let resolved = url.URLByResolvingSymlinksInPath().unwrap_or(url);
            let path = resolved.path()?;
            Some(std::path::PathBuf::from(path.to_string()))
        }
    }

    fn has_type(target: &objc2_app_kit::NSPasteboardType) -> bool {
        let _pb_guard = super::pasteboard_guard();
        unsafe {
            let pb = NSPasteboard::generalPasteboard();
            let Some(types) = pb.types() else {
                return false;
            };
            for i in 0..types.count() {
                let t = types.objectAtIndex(i);
                if t.isEqualToString(target) {
                    return true;
                }
            }
            false
        }
    }

    pub fn read_string() -> Option<String> {
        let _pb_guard = super::pasteboard_guard();
        autoreleasepool(|_| unsafe {
            let pb = NSPasteboard::generalPasteboard();
            pb.stringForType(NSPasteboardTypeString)
                .map(|s| s.to_string())
        })
    }

    pub fn write_string(s: &str) {
        let _pb_guard = super::pasteboard_guard();
        unsafe {
            let pb = NSPasteboard::generalPasteboard();
            pb.clearContents();
            let ns = NSString::from_str(s);
            pb.setString_forType(&ns, NSPasteboardTypeString);
        }
    }

    /// Return the Mac clipboard's image, normalized to PNG bytes. Tries
    /// PNG first, falls back to TIFF (which we re-encode in clipboard.rs
    /// via the `image` crate so this returns PNG either way).
    pub fn read_image_bytes() -> Option<(ImageEncoding, Vec<u8>)> {
        let _pb_guard = super::pasteboard_guard();
        autoreleasepool(|_| unsafe {
            let pb = NSPasteboard::generalPasteboard();
            if let Some(d) = pb.dataForType(NSPasteboardTypePNG) {
                return Some((ImageEncoding::Png, nsdata_to_vec(&d)));
            }
            if let Some(d) = pb.dataForType(NSPasteboardTypeTIFF) {
                return Some((ImageEncoding::Tiff, nsdata_to_vec(&d)));
            }
            None
        })
    }

    pub fn write_png(bytes: &[u8]) {
        let _pb_guard = super::pasteboard_guard();
        unsafe {
            let pb = NSPasteboard::generalPasteboard();
            pb.clearContents();
            let data = NSData::with_bytes(bytes);
            pb.setData_forType(Some(&data), NSPasteboardTypePNG);
        }
    }

    fn nsdata_to_vec(d: &NSData) -> Vec<u8> {
        unsafe {
            let len = d.length();
            let ptr = d.bytes().as_ptr();
            std::slice::from_raw_parts(ptr, len).to_vec()
        }
    }

    pub enum ImageEncoding {
        Png,
        Tiff,
    }
}

#[cfg(not(target_os = "macos"))]
mod pb {
    pub enum ImageEncoding {
        Png,
        Tiff,
    }
    pub struct FileEntry {
        pub name: String,
        pub size: Option<u64>,
        pub is_dir: bool,
        pub path: std::path::PathBuf,
        pub relative_path: Option<String>,
    }
    pub fn change_count() -> i64 {
        0
    }
    pub fn has_string() -> bool {
        false
    }
    pub fn has_image() -> bool {
        false
    }
    pub fn has_files() -> bool {
        false
    }
    pub fn read_string() -> Option<String> {
        None
    }
    pub fn write_string(_: &str) {}
    pub fn read_image_bytes() -> Option<(ImageEncoding, Vec<u8>)> {
        None
    }
    pub fn write_png(_: &[u8]) {}
    pub fn read_files() -> Vec<FileEntry> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmpfile(content: &[u8]) -> tempfile_path::TempPath {
        let tp = tempfile_path::new();
        std::fs::File::create(&tp.0)
            .unwrap()
            .write_all(content)
            .unwrap();
        tp
    }

    /// Manual tempfile helper — we don't want a dev-dep on the `tempfile`
    /// crate just for these few tests, and the std fallback is one path.
    mod tempfile_path {
        use std::path::PathBuf;
        use std::sync::atomic::{AtomicU64, Ordering};

        static SEQ: AtomicU64 = AtomicU64::new(0);

        pub struct TempPath(pub PathBuf);
        impl Drop for TempPath {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }

        pub fn new() -> TempPath {
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let p = std::env::temp_dir().join(format!(
                "macrdp-cliprdr-test-{}-{n}.bin",
                std::process::id()
            ));
            TempPath(p)
        }
    }

    #[test]
    fn read_full_file_returns_all_bytes() {
        let data = b"hello world";
        let f = tmpfile(data);
        let got = read_file_range(&f.0, 0, 1024).unwrap();
        assert_eq!(got, data);
    }

    #[test]
    fn read_with_position_skips_prefix() {
        let f = tmpfile(b"ABCDEFGHIJ");
        let got = read_file_range(&f.0, 4, 3).unwrap();
        assert_eq!(got, b"EFG");
    }

    #[test]
    fn read_past_eof_returns_short() {
        let f = tmpfile(b"abc");
        // Request 10 bytes from offset 1 — file only has 2 bytes left.
        let got = read_file_range(&f.0, 1, 10).unwrap();
        assert_eq!(got, b"bc");
    }

    #[test]
    fn read_at_eof_returns_empty() {
        let f = tmpfile(b"xyz");
        let got = read_file_range(&f.0, 3, 100).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn read_caps_at_max_range_bytes() {
        // 5 MiB file; ask for 8 MiB; should be clamped to MAX_FILE_RANGE_BYTES (4 MiB).
        let data = vec![0xABu8; 5 * 1024 * 1024];
        let f = tmpfile(&data);
        let got = read_file_range(&f.0, 0, 8 * 1024 * 1024).unwrap();
        assert_eq!(got.len(), MAX_FILE_RANGE_BYTES as usize);
        assert!(got.iter().all(|&b| b == 0xAB));
    }

    /// Disposable temp directory; removed on drop. Standalone for the same
    /// reason as `tempfile_path` above — avoids pulling in `tempfile` as a
    /// dev-dep when std fs is enough.
    #[cfg(target_os = "macos")]
    struct TempDir(std::path::PathBuf);
    #[cfg(target_os = "macos")]
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[cfg(target_os = "macos")]
    fn tmpdir(label: &str) -> TempDir {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "macrdp-cliprdr-walk-{label}-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }

    /// Verify the recursive walk:
    ///   root/
    ///     a.txt
    ///     sub/
    ///       b.txt
    ///       deep/
    ///         c.txt
    /// Should yield 5 descriptors with the right name + relative_path pairs
    /// so the wire `cFileName` (relative_path\name) reconstructs the full
    /// path under the copied root.
    #[cfg(target_os = "macos")]
    #[test]
    fn walk_inner_emits_recursive_entries_with_relative_paths() {
        use std::io::Write;
        let root = tmpdir("nested");
        let sub = root.0.join("sub");
        let deep = sub.join("deep");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::File::create(root.0.join("a.txt"))
            .unwrap()
            .write_all(b"a")
            .unwrap();
        std::fs::File::create(sub.join("b.txt"))
            .unwrap()
            .write_all(b"bb")
            .unwrap();
        std::fs::File::create(deep.join("c.txt"))
            .unwrap()
            .write_all(b"ccc")
            .unwrap();

        let root_name = root.0.file_name().unwrap().to_str().unwrap().to_owned();
        let mut entries: Vec<pb::FileEntry> = Vec::new();
        let ok = pb::walk_inner(&mut entries, &root.0, root_name.clone());
        assert!(ok, "walk_inner should not hit the cap on a tiny tree");

        // Build a (name, relative_path, is_dir) set so we can assert without
        // depending on filesystem iteration order.
        let mut seen: Vec<(String, Option<String>, bool, Option<u64>)> = entries
            .into_iter()
            .map(|e| (e.name, e.relative_path, e.is_dir, e.size))
            .collect();
        seen.sort();

        let expected_sub_prefix = format!("{root_name}\\sub");
        let mut expected: Vec<(String, Option<String>, bool, Option<u64>)> = vec![
            ("a.txt".into(), Some(root_name.clone()), false, Some(1)),
            ("sub".into(), Some(root_name.clone()), true, None),
            (
                "b.txt".into(),
                Some(expected_sub_prefix.clone()),
                false,
                Some(2),
            ),
            ("deep".into(), Some(expected_sub_prefix.clone()), true, None),
            (
                "c.txt".into(),
                Some(format!("{expected_sub_prefix}\\deep")),
                false,
                Some(3),
            ),
        ];
        expected.sort();
        assert_eq!(seen, expected);
    }

    /// Empty / unreadable / nonexistent directory should not panic and not
    /// stop the outer walk (returns `true`).
    #[cfg(target_os = "macos")]
    #[test]
    fn walk_inner_handles_missing_dir() {
        let bogus = std::path::PathBuf::from("/no/such/dir/macrdp-test-does-not-exist");
        let mut entries: Vec<pb::FileEntry> = Vec::new();
        let ok = pb::walk_inner(&mut entries, &bogus, "root".to_owned());
        assert!(ok);
        assert!(entries.is_empty());
    }
}
