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
use std::time::Duration;

use std::io::{Cursor, Read, Seek, SeekFrom};

use image::{ImageEncoder, ImageReader};
use ironrdp_cliprdr::backend::{ClipboardMessage, CliprdrBackend, CliprdrBackendFactory};
use ironrdp_cliprdr::pdu::{
    ClipboardFileAttributes, ClipboardFormat, ClipboardFormatId, ClipboardFormatName,
    ClipboardGeneralCapabilityFlags, FileContentsFlags, FileContentsRequest, FileContentsResponse,
    FileDescriptor, FormatDataRequest, FormatDataResponse, LockDataId, OwnedFormatDataResponse,
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

/// How long `on_ready`'s very first (connect-time) call defers our own
/// Mac->client advertise, so it lands safely after the client's own
/// FormatListResponse::Ok ack instead of racing it through the shared
/// writer (see `MacCliprdrBackend::initial_advertise_pending`). Comfortably
/// past any realistic ack-write latency (microseconds to low-single-digit
/// ms, even under the write contention that provokes the race) while still
/// being imperceptible to the user.
const CONNECT_ADVERTISE_DELAY: Duration = Duration::from_millis(300);

/// Registered-format IDs macrdp uses when *advertising* rich text to the
/// remote. For registered formats the sender picks the ID and the receiver
/// matches by NAME (MS-RDPECLIP 2.2.3.1), so any value in the registered
/// range (0xC000–0xFFFF) that's unique within our own format list works.
const HTML_FORMAT_ID: u32 = 0xC0F0;
const RTF_FORMAT_ID: u32 = 0xC0F1;

/// One representation to fetch from the remote as part of a single copy.
/// The format ID is the remote's own (it varies for registered formats); the
/// variant says how to decode the reply, since a FormatDataResponse carries
/// no format ID of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Want {
    Text(ClipboardFormatId),
    Image(ClipboardFormatId),
    Html(ClipboardFormatId),
    Rtf(ClipboardFormatId),
    /// A request whose reply isn't ours to decode here (the file-list
    /// descriptor, which upstream cliprdr consumes itself).
    Other(ClipboardFormatId),
}

impl Want {
    fn id(self) -> ClipboardFormatId {
        match self {
            Want::Text(id) | Want::Image(id) | Want::Html(id) | Want::Rtf(id) | Want::Other(id) => {
                id
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum RemotePlan {
    /// The remote copied files: request the file list and nothing else.
    Files(ClipboardFormatId),
    /// Fetch these, one at a time, then publish them as one pasteboard item.
    Fetch(Vec<Want>),
    Nothing,
}

/// Decide what to fetch for a remote copy. Files ALWAYS win and are decided
/// first — a copy that also offers rich text must still take the file path,
/// exactly as before rich text existed. Otherwise fetch every representation
/// we can use, so a copy that offers an image AND rich text (Chrome's "Copy
/// Image" also puts HTML on the clipboard) keeps the image instead of
/// degrading to an `<img>` tag: plain text, then one rich format (HTML
/// preferred — browsers only offer HTML, and it's usually the smaller), then
/// one image (DIBV5 preferred over DIB for colour).
/// A short, escaped prefix of a rejected clipboard payload for the log, so a
/// client that answers with something unexpected can be diagnosed without a
/// packet capture. Bounded, and escaped so it can't inject log lines.
fn payload_preview(data: &[u8]) -> String {
    data.iter()
        .take(64)
        .flat_map(|&b| std::ascii::escape_default(b))
        .map(char::from)
        .collect()
}

fn plan_remote_fetch(formats: &[ClipboardFormat], rich: bool) -> RemotePlan {
    let named = |name: &str| {
        formats
            .iter()
            .find(|f| {
                f.name
                    .as_ref()
                    .is_some_and(|n| n.value().eq_ignore_ascii_case(name))
            })
            .map(|f| f.id)
    };
    // FileGroupDescriptorW is identified by name per MS-RDPECLIP — the
    // numeric ID is assigned by the remote and varies.
    if let Some(fmt) = formats.iter().find(|f| {
        f.name
            .as_ref()
            .map(|n| n.value() == "FileGroupDescriptorW")
            .unwrap_or(false)
    }) {
        return RemotePlan::Files(fmt.id);
    }
    let has = |id: ClipboardFormatId| formats.iter().any(|f| f.id == id);

    let mut wants = Vec::new();
    if has(ClipboardFormatId::CF_UNICODETEXT) {
        wants.push(Want::Text(ClipboardFormatId::CF_UNICODETEXT));
    }
    if rich {
        if let Some(id) = named(crate::clipboard_rich::HTML_FORMAT_NAME) {
            wants.push(Want::Html(id));
        } else if let Some(id) = named(crate::clipboard_rich::RTF_FORMAT_NAME) {
            wants.push(Want::Rtf(id));
        }
    }
    if let Some(id) = [ClipboardFormatId::CF_DIBV5, ClipboardFormatId::CF_DIB]
        .into_iter()
        .find(|&id| has(id))
    {
        wants.push(Want::Image(id));
    }
    if wants.is_empty() {
        RemotePlan::Nothing
    } else {
        RemotePlan::Fetch(wants)
    }
}

/// Everything fetched so far for the current remote copy.
#[derive(Debug, Default)]
struct Collected {
    text: Option<String>,
    html: Option<String>,
    rtf: Option<Vec<u8>>,
    png: Option<Vec<u8>>,
}

impl Collected {
    fn is_empty(&self) -> bool {
        self.text.is_none() && self.html.is_none() && self.rtf.is_none() && self.png.is_none()
    }
}

/// Windows→Mac fetch in progress. A FormatDataResponse names no format, so
/// the representations of one copy are requested strictly in sequence — one
/// request on the wire at a time — which keeps each reply's attribution
/// exact: `current` is the request on the wire, `queue` what follows, `got`
/// what has arrived. (MS-RDPECLIP itself allows several outstanding requests,
/// each answered in order; sequencing is macrdp's choice, not a spec rule.)
#[derive(Debug, Default)]
struct RemoteFetch {
    current: Option<Want>,
    /// Whether `current` is already a retry. A failed representation is
    /// asked for once more before it's skipped: Windows apps commonly
    /// announce a copy twice in quick succession, and the second fetch can
    /// hit the source app still holding its clipboard open — a transient
    /// error that, unretried, published the copy without its rich format.
    retried: bool,
    queue: std::collections::VecDeque<Want>,
    got: Collected,
    /// Set while the reply to a SUPERSEDED request is still due. Browsers
    /// and Word announce one copy twice in quick succession; restarting the
    /// fetch at once let the first copy's reply be attributed to the second
    /// copy's request — text landed in the HTML slot and vice versa, both
    /// were rejected, and the copy published without its rich format (seen
    /// live with Firefox). So the new copy's first request waits for that
    /// reply, which is discarded. Holds when the superseded request went
    /// out, so a reply that never comes can't stall the clipboard for long.
    awaiting_stale: Option<std::time::Instant>,
}

/// How long a superseded request's reply is waited for before it's presumed
/// lost. Every FormatDataRequest is answered (data or a failure), so this only
/// bounds a misbehaving client.
const STALE_REPLY_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

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
    /// The changeCount of our most recent publish of a remote text / rich /
    /// image copy. The poller skips it exactly like `self_change_count`, so a
    /// Windows copy isn't advertised straight back to Windows — but it is a
    /// SEPARATE marker on purpose: `self_change_count` also drives
    /// `cleanup_on_disconnect` and `clear_pasteboard_if_stale`, which clear the
    /// pasteboard while it still holds our write. That's right for stale
    /// temp-file URLs and wrong for text the user copied, which must outlive
    /// the session.
    #[cfg(target_os = "macos")]
    remote_write_cc: Arc<std::sync::atomic::AtomicI64>,
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
    /// Rich-text clipboard (HTML + RTF) in both directions. On by default;
    /// `--no-rich-clipboard` restores plain text + images only.
    rich_clipboard: bool,
}

#[cfg(target_os = "macos")]
impl MacCliprdr {
    pub fn new(lazy_paste: bool, rich_clipboard: bool) -> Self {
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
            remote_write_cc: Arc::new(std::sync::atomic::AtomicI64::new(-1)),
            advertise_state: Arc::new(AdvertiseState::default()),
            active_backends: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            lazy_paste,
            rich_clipboard,
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
            rich_clipboard: true,
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
        #[cfg(target_os = "macos")]
        let remote_write_cc = self.remote_write_cc.clone();
        let advertise_state = self.advertise_state.clone();
        let active_backends = self.active_backends.clone();
        let rich = self.rich_clipboard;
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
                if current == self_cc.load(std::sync::atomic::Ordering::Relaxed)
                    || current == remote_write_cc.load(std::sync::atomic::Ordering::Relaxed)
                {
                    debug!(current, "skipping pasteboard tick (self-write)");
                    continue;
                }
                // Start a new advertise wave. `my_gen` identifies it for the
                // retry loop and for the cliprdr backend's
                // `on_format_list_response` hook, which stamps `locked_gen` with
                // the current generation on `Ok` so we can stop re-advertising.
                use std::sync::atomic::Ordering;
                let my_gen = advertise_state.generation.fetch_add(1, Ordering::Relaxed) + 1;

                if !advertise_pasteboard(&sender_arc, &paths_arc, rich) {
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
                    if !advertise_pasteboard(&sender_arc, &paths_arc, rich) {
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
            fetch: RemoteFetch::default(),
            rich_clipboard: self.rich_clipboard,
            active_backends: self.active_backends.clone(),
            file_paths: self.file_paths.clone(),
            #[cfg(target_os = "macos")]
            download_router: self.download_router.clone(),
            #[cfg(target_os = "macos")]
            paste_temp_dir: self.paste_temp_dir.clone(),
            #[cfg(target_os = "macos")]
            self_change_count: self.self_change_count.clone(),
            #[cfg(target_os = "macos")]
            remote_write_cc: self.remote_write_cc.clone(),
            advertise_state: self.advertise_state.clone(),
            #[cfg(target_os = "macos")]
            lazy_paste: self.lazy_paste,
            initial_advertise_pending: true,
        })
    }
}

impl CliprdrServerFactory for MacCliprdr {}

#[derive(Debug)]
struct MacCliprdrBackend {
    sender: Sender,
    // Windows→Mac fetch in progress: on_format_data_response doesn't include
    // the format ID, so this records what each reply is. See RemoteFetch.
    fetch: RemoteFetch,
    // See MacCliprdr::rich_clipboard.
    rich_clipboard: bool,
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
    // See MacCliprdr::remote_write_cc — why this is NOT self_change_count.
    #[cfg(target_os = "macos")]
    remote_write_cc: Arc<std::sync::atomic::AtomicI64>,
    // Lets the `on_format_list_response` hook tell the poller's retry loop
    // that the current advertise wave was accepted, so it stops re-advertising.
    advertise_state: Arc<AdvertiseState>,
    // Routes on_remote_file_list to the lazy NSFilePresenter path when
    // true (the default; --no-lazy-paste flips it). See MacCliprdr::lazy_paste.
    #[cfg(target_os = "macos")]
    lazy_paste: bool,
    // True until the first `on_ready()` call is handled. That first call
    // fires synchronously while the client's OWN initial (pre-connection)
    // FormatList PDU is still being processed on the inbound dispatch path
    // — its FormatListResponse::Ok ack is written moments later by that
    // same caller. If we fire our own Mac->client advertise here too, it
    // queues onto the shared ServerEvent channel that the sibling
    // dispatch_events loop drains independently, racing the ack through
    // ironrdp-server's shared writer mutex (see vendor/ironrdp-server
    // SharedWriter): under any write contention (e.g. the initial
    // full-frame video paint happening at the same moment), our advertise
    // or the client-clipboard fetch it can crowd out may reach the wire
    // before the client's own ack. See the clipboard preconnect-sync quirk
    // note this fix is paired with.
    //
    // We DEFER (not skip) this first advertise, via a short `tokio::spawn`
    // sleep — CONNECT_ADVERTISE_DELAY, comfortably past when the ack write
    // completes — so it still reaches a reconnecting/second client whose
    // Mac clipboard hasn't changed since the last advertise (the
    // changeCount poller only fires on a CHANGE, so skipping outright would
    // silently drop Mac->client sync for that case; caught in review on
    // #173).
    initial_advertise_pending: bool,
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

    /// If this reply answers a superseded request, drop it and start the
    /// current copy's fetch. Returns whether the reply was consumed.
    fn discard_stale_reply(&mut self) -> bool {
        if self.fetch.awaiting_stale.take().is_none() {
            return false;
        }
        debug!("discarding the reply to a superseded clipboard request");
        self.request_next();
        true
    }

    /// Request the next representation of the current remote copy, or — once
    /// the queue is drained — publish everything that arrived.
    fn request_next(&mut self) {
        match self.fetch.queue.pop_front() {
            Some(want) => {
                debug!(?want, "requesting remote format data");
                self.fetch.current = Some(want);
                self.fetch.retried = false;
                self.push(ClipboardMessage::SendInitiatePaste(want.id()));
            }
            None => self.publish_fetched(),
        }
    }

    /// Write one remote copy to NSPasteboard as a single multi-type item and
    /// mark that write as our own, so the pasteboard poller doesn't advertise
    /// it straight back to the remote. Without that mark the echo makes the
    /// client take over its own clipboard again with only what we fetched —
    /// e.g. Word's formatted selection replaced by our plain text/HTML copy.
    /// The mark goes in `remote_write_cc`, NOT `self_change_count` — see
    /// MacCliprdr::remote_write_cc for why that distinction is load-bearing.
    fn publish_fetched(&mut self) {
        let got = std::mem::take(&mut self.fetch.got);
        if got.is_empty() {
            return;
        }
        debug!(
            text = got.text.as_ref().map(String::len),
            html = got.html.as_ref().map(String::len),
            rtf = got.rtf.as_ref().map(Vec::len),
            image = got.png.as_ref().map(Vec::len),
            "writing remote clipboard to NSPasteboard"
        );
        #[cfg(target_os = "macos")]
        let mark = &*self.remote_write_cc;
        #[cfg(not(target_os = "macos"))]
        let mark = &std::sync::atomic::AtomicI64::new(-1);
        pb::write_items(
            got.text.as_deref(),
            got.html.as_deref(),
            got.rtf.as_deref(),
            got.png.as_deref(),
            mark,
        );
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
/// file lists must go via `initiate_file_copy` (reached through
/// `ClipboardMessage::SendInitiateFileCopy`) so that the cliprdr
/// server populates its `local_file_list` and accepts subsequent
/// FileContentsRequests instead of short-circuiting them with
/// CB_RESPONSE_FAIL.
fn advertise_pasteboard(sender: &Sender, paths: &Paths, rich: bool) -> bool {
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
            return send(
                sender,
                ServerEvent::Clipboard(ClipboardMessage::SendInitiateFileCopy(files)),
            );
        }
        // Files claimed but read empty (race) — fall through to format list.
    }

    let mut formats = Vec::new();
    // Rich formats go first — the list order is our stated preference. They
    // are registered formats, advertised by NAME; see HTML_FORMAT_ID.
    if rich {
        if pb::has_html() {
            formats.push(
                ClipboardFormat::new(ClipboardFormatId::new(HTML_FORMAT_ID)).with_name(
                    ClipboardFormatName::new(crate::clipboard_rich::HTML_FORMAT_NAME),
                ),
            );
        }
        if pb::has_rtf() {
            formats.push(
                ClipboardFormat::new(ClipboardFormatId::new(RTF_FORMAT_ID)).with_name(
                    ClipboardFormatName::new(crate::clipboard_rich::RTF_FORMAT_NAME),
                ),
            );
        }
    }
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
        if self.initial_advertise_pending {
            self.initial_advertise_pending = false;
            debug!(
                delay_ms = CONNECT_ADVERTISE_DELAY.as_millis() as u64,
                "deferring connect-time pasteboard advertise past the client's own \
                 initial FormatList ack (racing it through the shared writer would \
                 confuse the client's still-open init handshake)"
            );
            let sender = self.sender.clone();
            let file_paths = self.file_paths.clone();
            let rich = self.rich_clipboard;
            tokio::spawn(async move {
                tokio::time::sleep(CONNECT_ADVERTISE_DELAY).await;
                advertise_pasteboard(&sender, &file_paths, rich);
            });
            return;
        }
        advertise_pasteboard(&self.sender, &self.file_paths, self.rich_clipboard);
    }

    fn on_request_format_list(&mut self) {
        advertise_pasteboard(&self.sender, &self.file_paths, self.rich_clipboard);
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
        debug!(
            format_count = available_formats.len(),
            formats = ?available_formats
                .iter()
                .map(|f| (f.id.value(), f.name.as_ref().map(|n| n.value().to_owned())))
                .collect::<Vec<_>>(),
            "remote clipboard format list received (connect-time announce or a live copy)"
        );
        // A new remote copy supersedes whatever is still in flight for the
        // previous one. Replies name no format, so if a request is still on
        // the wire its reply must be waited for and discarded before the new
        // copy's first request goes out — see RemoteFetch::awaiting_stale.
        let now = std::time::Instant::now();
        let awaiting_stale = if self.fetch.current.is_some() {
            Some(now)
        } else {
            self.fetch
                .awaiting_stale
                .filter(|since| now.duration_since(*since) < STALE_REPLY_GRACE)
        };
        self.fetch = RemoteFetch {
            awaiting_stale,
            ..RemoteFetch::default()
        };
        match plan_remote_fetch(available_formats, self.rich_clipboard) {
            RemotePlan::Files(id) => {
                debug!(format_id = ?id, "remote advertised files; requesting file list");
                self.fetch.queue.push_back(Want::Other(id));
            }
            RemotePlan::Fetch(wants) => {
                self.fetch.queue = wants.into();
            }
            RemotePlan::Nothing => {
                if !available_formats.is_empty() {
                    debug!(
                        "remote format list had none of our supported formats; nothing requested"
                    );
                }
            }
        }
        if self.fetch.awaiting_stale.is_some() {
            debug!("waiting for the superseded request's reply before fetching the new copy");
        } else {
            self.request_next();
        }
    }

    fn on_remote_file_list(&mut self, files: &[FileDescriptor], clip_data_id: Option<u32>) {
        // The file-list request is answered here, not via on_format_data_response.
        if self.discard_stale_reply() {
            return;
        }
        self.fetch.current = None;
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
        // `SendInitiateFileCopy` path) — it answers the FormatData
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
            f if f.value() == HTML_FORMAT_ID => match pb::read_html() {
                Some(html) => {
                    OwnedFormatDataResponse::new_data(crate::clipboard_rich::encode_cf_html(&html))
                }
                None => OwnedFormatDataResponse::new_error(),
            },
            f if f.value() == RTF_FORMAT_ID => match pb::read_rtf() {
                Some(rtf) => {
                    OwnedFormatDataResponse::new_data(crate::clipboard_rich::encode_rtf(&rtf))
                }
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
        if self.discard_stale_reply() {
            return;
        }
        let want = self.fetch.current.take();
        if let Some(Want::Other(id)) = want {
            warn!(format_id = ?id, "unexpected format in data response");
            return;
        }
        if response.is_error() {
            if let (Some(w), false) = (want, self.fetch.retried) {
                debug!(want = ?w, "remote returned error for format data; retrying once");
                self.fetch.current = Some(w);
                self.fetch.retried = true;
                self.push(ClipboardMessage::SendInitiatePaste(w.id()));
                return;
            }
            // One representation failing (e.g. an app that advertises HTML
            // but can't render it) mustn't cost the others: carry on.
            warn!(?want, "remote returned error for format data");
            self.request_next();
            return;
        }
        let data = response.data();
        if data.len() > MAX_INCOMING_PAYLOAD {
            warn!(
                len = data.len(),
                cap = MAX_INCOMING_PAYLOAD,
                "clipboard payload exceeds cap; dropping"
            );
            self.request_next();
            return;
        }
        match want {
            // Default to text if we don't know what we asked for — matches
            // the original text-only behaviour.
            Some(Want::Text(_)) | None => {
                if data.len() % 2 != 0 {
                    warn!(len = data.len(), "odd-length UTF-16 payload");
                } else {
                    let mut units: Vec<u16> = data
                        .chunks_exact(2)
                        .map(|c| u16::from_le_bytes([c[0], c[1]]))
                        .collect();
                    // CF_UNICODETEXT ends at its first NUL. Windows apps
                    // (Word) can hand over a buffer with MORE than one
                    // trailing NUL; popping just one left a NUL in the Mac
                    // text, which Terminal pastes as a visible `^@`.
                    if let Some(nul) = units.iter().position(|&u| u == 0) {
                        units.truncate(nul);
                    }
                    match String::from_utf16(&units) {
                        Ok(s) => self.fetch.got.text = Some(s),
                        Err(e) => warn!("UTF-16 decode failed: {e}"),
                    }
                }
            }
            Some(Want::Image(_)) => match dib_to_png(data) {
                Ok(png) => self.fetch.got.png = Some(png),
                Err(e) => warn!("DIB decode failed: {e}"),
            },
            Some(Want::Html(_)) => match crate::clipboard_rich::decode_cf_html(data) {
                Some(html) => self.fetch.got.html = Some(html),
                None => warn!(
                    len = data.len(),
                    preview = %payload_preview(data),
                    "remote HTML Format payload had no markup"
                ),
            },
            Some(Want::Rtf(_)) => match crate::clipboard_rich::decode_rtf(data) {
                Some(rtf) => self.fetch.got.rtf = Some(rtf),
                None => warn!(
                    len = data.len(),
                    preview = %payload_preview(data),
                    "remote Rich Text Format payload isn't RTF"
                ),
            },
            Some(Want::Other(_)) => unreachable!("handled above"),
        }
        self.request_next();
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

/// The pasteboard every clipboard path reads and writes: the system's general
/// pasteboard. Callers hold [`pasteboard_guard`] while using it.
#[cfg(all(target_os = "macos", not(test)))]
pub(crate) fn pasteboard() -> objc2::rc::Retained<objc2_app_kit::NSPasteboard> {
    unsafe { objc2_app_kit::NSPasteboard::generalPasteboard() }
}

/// Under test, each test thread gets its own uniquely named pasteboard instead.
/// The general one is shared by every process on the Mac, so tests that wrote
/// it and then asserted on it failed whenever anything else copied at the same
/// moment: a parallel test, another test process, Universal Clipboard, or the
/// user. libtest runs each test on its own thread, and the tests that reach the
/// pasteboard stay on that thread (`#[tokio::test]`'s default runtime is
/// single-threaded, so even the spawned deferred advertise does). The board is
/// released when the thread exits so runs don't leave named pasteboards behind.
#[cfg(all(target_os = "macos", test))]
pub(crate) fn pasteboard() -> objc2::rc::Retained<objc2_app_kit::NSPasteboard> {
    struct Private(objc2::rc::Retained<objc2_app_kit::NSPasteboard>);
    impl Drop for Private {
        fn drop(&mut self) {
            // Not generated in objc2-app-kit 0.2; the AppKit method is.
            let () = unsafe { objc2::msg_send![&self.0, releaseGlobally] };
        }
    }
    thread_local! {
        static PRIVATE: Private =
            Private(unsafe { objc2_app_kit::NSPasteboard::pasteboardWithUniqueName() });
    }
    PRIVATE.with(|pb| pb.0.clone())
}

#[cfg(target_os = "macos")]
mod pb {
    use objc2::rc::autoreleasepool;
    use objc2_app_kit::{
        NSPasteboardTypeFileURL, NSPasteboardTypeHTML, NSPasteboardTypePNG, NSPasteboardTypeRTF,
        NSPasteboardTypeString, NSPasteboardTypeTIFF,
    };
    use objc2_foundation::{NSData, NSString, NSURL};

    pub fn change_count() -> i64 {
        let _pb_guard = super::pasteboard_guard();
        unsafe {
            let pb = super::pasteboard();
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

    pub fn has_html() -> bool {
        unsafe { has_type(NSPasteboardTypeHTML) }
    }

    pub fn has_rtf() -> bool {
        unsafe { has_type(NSPasteboardTypeRTF) }
    }

    /// The pasteboard's HTML representation (`public.html`), UTF-8 decoded.
    pub fn read_html() -> Option<String> {
        let _pb_guard = super::pasteboard_guard();
        autoreleasepool(|_| unsafe {
            let pb = super::pasteboard();
            pb.dataForType(NSPasteboardTypeHTML)
                .map(|d| String::from_utf8_lossy(&nsdata_to_vec(&d)).into_owned())
        })
    }

    /// The pasteboard's RTF representation (`public.rtf`), as raw bytes.
    pub fn read_rtf() -> Option<Vec<u8>> {
        let _pb_guard = super::pasteboard_guard();
        autoreleasepool(|_| unsafe {
            let pb = super::pasteboard();
            pb.dataForType(NSPasteboardTypeRTF)
                .map(|d| nsdata_to_vec(&d))
        })
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
            let pb = super::pasteboard();
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
            let pb = super::pasteboard();
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
            let pb = super::pasteboard();
            pb.stringForType(NSPasteboardTypeString)
                .map(|s| s.to_string())
        })
    }

    /// Test-only now: remote copies publish through [`write_items`].
    #[cfg(test)]
    pub fn write_string(s: &str) {
        let _pb_guard = super::pasteboard_guard();
        unsafe {
            let pb = super::pasteboard();
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
            let pb = super::pasteboard();
            if let Some(d) = pb.dataForType(NSPasteboardTypePNG) {
                return Some((ImageEncoding::Png, nsdata_to_vec(&d)));
            }
            if let Some(d) = pb.dataForType(NSPasteboardTypeTIFF) {
                return Some((ImageEncoding::Tiff, nsdata_to_vec(&d)));
            }
            None
        })
    }

    /// Publish everything fetched from one remote copy as a single pasteboard
    /// item carrying every representation, so the app you paste into picks
    /// the richest type it understands (TextEdit takes RTF/HTML, a plain
    /// field takes the string, Preview takes the image). The resulting
    /// changeCount is stored in `mark` while the pasteboard guard is still
    /// held, so the poller (which reads changeCount under the same guard) can
    /// never observe our write before it is marked as ours.
    pub fn write_items(
        text: Option<&str>,
        html: Option<&str>,
        rtf: Option<&[u8]>,
        png: Option<&[u8]>,
        mark: &std::sync::atomic::AtomicI64,
    ) {
        let _pb_guard = super::pasteboard_guard();
        unsafe {
            let pb = super::pasteboard();
            pb.clearContents();
            // Richest first: the order is the owner's stated preference.
            if let Some(rtf) = rtf {
                pb.setData_forType(Some(&NSData::with_bytes(rtf)), NSPasteboardTypeRTF);
            }
            if let Some(html) = html {
                pb.setData_forType(
                    Some(&NSData::with_bytes(html.as_bytes())),
                    NSPasteboardTypeHTML,
                );
            }
            if let Some(png) = png {
                pb.setData_forType(Some(&NSData::with_bytes(png)), NSPasteboardTypePNG);
            }
            if let Some(text) = text {
                pb.setString_forType(&NSString::from_str(text), NSPasteboardTypeString);
            }
            mark.store(
                pb.changeCount() as i64,
                std::sync::atomic::Ordering::Relaxed,
            );
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
    #[cfg(test)]
    pub fn write_string(_: &str) {}
    pub fn read_image_bytes() -> Option<(ImageEncoding, Vec<u8>)> {
        None
    }
    pub fn has_html() -> bool {
        false
    }
    pub fn has_rtf() -> bool {
        false
    }
    pub fn read_html() -> Option<String> {
        None
    }
    pub fn read_rtf() -> Option<Vec<u8>> {
        None
    }
    pub fn write_items(
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&[u8]>,
        _: Option<&[u8]>,
        _: &std::sync::atomic::AtomicI64,
    ) {
    }
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

    #[cfg(target_os = "macos")]
    fn test_backend() -> (MacCliprdrBackend, mpsc::UnboundedReceiver<ServerEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let backend = MacCliprdrBackend {
            sender: Arc::new(Mutex::new(Some(tx))),
            fetch: RemoteFetch::default(),
            rich_clipboard: true,
            active_backends: Arc::new(std::sync::atomic::AtomicUsize::new(1)),
            file_paths: Arc::new(Mutex::new(Vec::new())),
            download_router: crate::file_promise::DownloadRouter::default(),
            paste_temp_dir: Arc::new(Mutex::new(None)),
            self_change_count: Arc::new(std::sync::atomic::AtomicI64::new(-1)),
            remote_write_cc: Arc::new(std::sync::atomic::AtomicI64::new(-1)),
            advertise_state: Arc::new(AdvertiseState::default()),
            lazy_paste: true,
            initial_advertise_pending: true,
        };
        (backend, rx)
    }

    /// The connect-time fix under test: the client's own initial FormatList
    /// (handled by `on_remote_copy`, below) fires `on_ready` synchronously
    /// from the same inbound-PDU handler that's about to write that
    /// client's FormatListResponse::Ok ack. A same-tick Mac->client
    /// advertise from `on_ready` races that ack through ironrdp-server's
    /// shared writer. The very first `on_ready` call per connection must
    /// therefore put nothing on the wire immediately — but (per review on
    /// #173) it must NOT be dropped outright either, since the changeCount
    /// poller only re-advertises on a CHANGE: a reconnecting/second client
    /// whose Mac clipboard hasn't changed since the last advertise would
    /// otherwise never receive it. So it must fire, just after a short
    /// deferral past the ack race.
    // Real (not paused) time: getting a spawned task to observe a manually
    // advanced paused clock needs runtime-version-sensitive yield dancing,
    // which is more fragile than it's worth for one 300 ms wait. Costs this
    // test ~0.3 s of real wall-clock; everything else in the suite is
    // effectively instant, so that's a non-issue.
    #[tokio::test]
    #[cfg(target_os = "macos")]
    async fn on_ready_defers_the_connect_time_advertise_past_the_ack_race() {
        let (mut backend, mut rx) = test_backend();

        backend.on_ready();
        assert!(
            rx.try_recv().is_err(),
            "connect-time on_ready must not put anything on the wire immediately"
        );
        assert!(!backend.initial_advertise_pending);

        pb::write_string("on_ready_deferred_probe");
        tokio::time::sleep(CONNECT_ADVERTISE_DELAY + Duration::from_millis(100)).await;

        match rx
            .try_recv()
            .expect("the deferred advertise must eventually fire")
        {
            ServerEvent::Clipboard(ClipboardMessage::SendInitiateCopy(formats)) => {
                assert!(formats
                    .iter()
                    .any(|f| f.id == ClipboardFormatId::CF_UNICODETEXT));
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    /// Once the connect-time handshake has settled (`initial_advertise_pending`
    /// already false — set up directly here rather than via a real first
    /// `on_ready()` call, so this test doesn't have to interact with that
    /// call's spawned deferred task), `on_ready` must advertise immediately,
    /// exactly as it did before the connect-time fix.
    #[test]
    #[cfg(target_os = "macos")]
    fn on_ready_advertises_immediately_once_settled() {
        let (mut backend, mut rx) = test_backend();
        backend.initial_advertise_pending = false;

        pb::write_string("on_ready_settled_probe");
        backend.on_ready();
        match rx
            .try_recv()
            .expect("post-handshake on_ready must advertise immediately")
        {
            ServerEvent::Clipboard(ClipboardMessage::SendInitiateCopy(formats)) => {
                assert!(formats
                    .iter()
                    .any(|f| f.id == ClipboardFormatId::CF_UNICODETEXT));
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    /// `on_remote_copy` is what the client's connect-time FormatList
    /// announce (its pre-connection clipboard) actually drives — unlike
    /// `on_ready`, it must NOT be gated, since it's the mechanism that
    /// fetches that content in the first place.
    #[test]
    #[cfg(target_os = "macos")]
    fn on_remote_copy_requests_the_announced_text_format() {
        let (mut backend, mut rx) = test_backend();
        let formats = vec![ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)];

        backend.on_remote_copy(&formats);

        match rx.try_recv().expect("must request the announced format") {
            ServerEvent::Clipboard(ClipboardMessage::SendInitiatePaste(id)) => {
                assert_eq!(id, ClipboardFormatId::CF_UNICODETEXT);
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert_eq!(
            backend.fetch.current,
            Some(Want::Text(ClipboardFormatId::CF_UNICODETEXT))
        );
    }

    fn named(id: u32, name: &'static str) -> ClipboardFormat {
        ClipboardFormat::new(ClipboardFormatId::new(id)).with_name(ClipboardFormatName::new(name))
    }

    const FGD: u32 = 0xC0A1;
    const HTML: u32 = 0xC0B2;
    const RTF: u32 = 0xC0C3;

    /// The guarantee rich text must not break: a copy that offers files is
    /// the file path, even when the same copy also offers rich text (Explorer
    /// selections can carry more than the descriptor).
    #[test]
    fn plan_files_win_even_when_rich_text_is_offered() {
        let formats = vec![
            ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT),
            named(HTML, "HTML Format"),
            named(RTF, "Rich Text Format"),
            named(FGD, "FileGroupDescriptorW"),
        ];
        assert_eq!(
            plan_remote_fetch(&formats, true),
            RemotePlan::Files(ClipboardFormatId::new(FGD))
        );
        assert_eq!(
            plan_remote_fetch(&formats, false),
            RemotePlan::Files(ClipboardFormatId::new(FGD))
        );
    }

    /// Chrome's "Copy Image" offers HTML alongside the bitmap. Fetching every
    /// representation keeps the image instead of degrading to an <img> tag.
    #[test]
    fn plan_fetches_text_rich_and_image_together() {
        let formats = vec![
            ClipboardFormat::new(ClipboardFormatId::CF_DIB),
            named(HTML, "HTML Format"),
            ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT),
        ];
        assert_eq!(
            plan_remote_fetch(&formats, true),
            RemotePlan::Fetch(vec![
                Want::Text(ClipboardFormatId::CF_UNICODETEXT),
                Want::Html(ClipboardFormatId::new(HTML)),
                Want::Image(ClipboardFormatId::CF_DIB),
            ])
        );
    }

    #[test]
    fn plan_prefers_html_and_falls_back_to_rtf() {
        let both = vec![named(RTF, "Rich Text Format"), named(HTML, "HTML Format")];
        assert_eq!(
            plan_remote_fetch(&both, true),
            RemotePlan::Fetch(vec![Want::Html(ClipboardFormatId::new(HTML))])
        );
        let rtf_only = vec![named(RTF, "Rich Text Format")];
        assert_eq!(
            plan_remote_fetch(&rtf_only, true),
            RemotePlan::Fetch(vec![Want::Rtf(ClipboardFormatId::new(RTF))])
        );
    }

    #[test]
    fn plan_matches_rich_format_names_case_insensitively() {
        // Windows registered clipboard format names are case-insensitive.
        let formats = vec![named(HTML, "html format")];
        assert_eq!(
            plan_remote_fetch(&formats, true),
            RemotePlan::Fetch(vec![Want::Html(ClipboardFormatId::new(HTML))])
        );
    }

    /// `--no-rich-clipboard`: rich formats are ignored; text and image still flow.
    #[test]
    fn plan_without_rich_ignores_html_and_rtf() {
        let formats = vec![
            named(HTML, "HTML Format"),
            ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT),
            ClipboardFormat::new(ClipboardFormatId::CF_DIBV5),
            ClipboardFormat::new(ClipboardFormatId::CF_DIB),
        ];
        assert_eq!(
            plan_remote_fetch(&formats, false),
            RemotePlan::Fetch(vec![
                Want::Text(ClipboardFormatId::CF_UNICODETEXT),
                Want::Image(ClipboardFormatId::CF_DIBV5),
            ])
        );
        assert_eq!(
            plan_remote_fetch(&[named(HTML, "HTML Format")], false),
            RemotePlan::Nothing
        );
    }

    #[test]
    fn plan_nothing_for_unsupported_formats() {
        let formats = vec![ClipboardFormat::new(ClipboardFormatId::CF_ENHMETAFILE)];
        assert_eq!(plan_remote_fetch(&formats, true), RemotePlan::Nothing);
    }

    /// End to end through the backend: a copy offering files AND rich text
    /// requests the file list and nothing else.
    #[test]
    #[cfg(target_os = "macos")]
    fn on_remote_copy_with_files_and_rich_text_requests_only_the_file_list() {
        let (mut backend, mut rx) = test_backend();
        let formats = vec![
            named(HTML, "HTML Format"),
            ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT),
            named(FGD, "FileGroupDescriptorW"),
        ];
        backend.on_remote_copy(&formats);
        match rx.try_recv().expect("must request the file list") {
            ServerEvent::Clipboard(ClipboardMessage::SendInitiatePaste(id)) => {
                assert_eq!(id, ClipboardFormatId::new(FGD));
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(rx.try_recv().is_err(), "nothing else may be requested");
        assert!(backend.fetch.queue.is_empty());
    }

    /// Requests go out one at a time (see RemoteFetch), so the second
    /// representation is only requested once the first reply lands — and a
    /// failed representation doesn't abandon the rest.
    #[test]
    #[cfg(target_os = "macos")]
    fn fetch_is_sequential_and_survives_a_failed_representation() {
        let (mut backend, mut rx) = test_backend();
        let formats = vec![
            ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT),
            named(HTML, "HTML Format"),
            ClipboardFormat::new(ClipboardFormatId::CF_DIB),
        ];
        let next_request = |rx: &mut mpsc::UnboundedReceiver<ServerEvent>| match rx.try_recv() {
            Ok(ServerEvent::Clipboard(ClipboardMessage::SendInitiatePaste(id))) => id,
            other => panic!("expected a paste request, got {other:?}"),
        };

        backend.on_remote_copy(&formats);
        assert_eq!(next_request(&mut rx), ClipboardFormatId::CF_UNICODETEXT);
        assert!(rx.try_recv().is_err(), "one request at a time");

        let text: Vec<u8> = "hi\0".encode_utf16().flat_map(u16::to_le_bytes).collect();
        backend.on_format_data_response(FormatDataResponse::new_data(text));
        assert_eq!(next_request(&mut rx), ClipboardFormatId::new(HTML));
        assert_eq!(backend.fetch.got.text.as_deref(), Some("hi"));

        // The HTML representation fails: it's retried once (a transient
        // error from a source app still holding its clipboard open)...
        backend.on_format_data_response(FormatDataResponse::new_error());
        assert_eq!(next_request(&mut rx), ClipboardFormatId::new(HTML));
        // ...and when the retry fails too, the image is still requested.
        backend.on_format_data_response(FormatDataResponse::new_error());
        assert_eq!(next_request(&mut rx), ClipboardFormatId::CF_DIB);
        assert!(backend.fetch.got.html.is_none());
    }

    /// CF_UNICODETEXT ends at its first NUL, however many trail it (seen live
    /// from Word: an extra NUL pasted into Terminal as `^@`).
    #[test]
    #[cfg(target_os = "macos")]
    fn remote_text_stops_at_the_first_nul() {
        let (mut backend, _rx) = test_backend();
        backend.on_remote_copy(&[ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)]);
        backend.fetch.queue.clear(); // keep the reply in `got` (no publish)
        backend
            .fetch
            .queue
            .push_back(Want::Text(ClipboardFormatId::CF_UNICODETEXT));
        let data: Vec<u8> = "hi\0\0\0"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        backend.on_format_data_response(FormatDataResponse::new_data(data));
        assert_eq!(backend.fetch.got.text.as_deref(), Some("hi"));
    }

    /// A retry that succeeds is decoded like a first-time reply, and the
    /// next representation gets its own retry allowance.
    #[test]
    #[cfg(target_os = "macos")]
    fn a_retried_representation_is_kept() {
        let (mut backend, mut rx) = test_backend();
        backend.on_remote_copy(&[
            named(RTF, "Rich Text Format"),
            ClipboardFormat::new(ClipboardFormatId::CF_DIB),
        ]);
        let _ = rx.try_recv();
        backend.on_format_data_response(FormatDataResponse::new_error());
        assert!(matches!(
            rx.try_recv(),
            Ok(ServerEvent::Clipboard(ClipboardMessage::SendInitiatePaste(id))) if id == ClipboardFormatId::new(RTF)
        ));
        backend.on_format_data_response(FormatDataResponse::new_data(b"{\\rtf1 hi}".to_vec()));
        assert!(backend.fetch.got.rtf.is_some());
        assert!(
            !backend.fetch.retried,
            "the image starts with a fresh retry"
        );
    }

    /// A new remote copy supersedes the one still being fetched — but its
    /// first request waits for the superseded request's reply, which is
    /// discarded rather than attributed to the new request.
    #[test]
    #[cfg(target_os = "macos")]
    fn a_new_remote_copy_restarts_the_fetch() {
        let (mut backend, mut rx) = test_backend();
        backend.on_remote_copy(&[
            ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT),
            named(HTML, "HTML Format"),
        ]);
        let _ = rx.try_recv();
        backend.on_remote_copy(&[ClipboardFormat::new(ClipboardFormatId::CF_DIB)]);
        assert!(
            rx.try_recv().is_err(),
            "nothing is requested while the old reply is still due"
        );
        // The superseded text request's reply arrives: dropped, then the new
        // copy's fetch starts.
        let text: Vec<u8> = "old\0".encode_utf16().flat_map(u16::to_le_bytes).collect();
        backend.on_format_data_response(FormatDataResponse::new_data(text));
        match rx.try_recv() {
            Ok(ServerEvent::Clipboard(ClipboardMessage::SendInitiatePaste(id))) => {
                assert_eq!(id, ClipboardFormatId::CF_DIB);
            }
            other => panic!("unexpected: {other:?}"),
        }
        assert!(
            backend.fetch.queue.is_empty(),
            "the old HTML request is dropped"
        );
        assert!(backend.fetch.got.is_empty(), "the stale reply isn't kept");
    }

    /// The live Firefox failure: one copy announced twice. The first
    /// announce's text reply must not land in the second fetch's slots.
    #[test]
    #[cfg(target_os = "macos")]
    fn a_double_announced_copy_keeps_its_rich_format() {
        let (mut backend, mut rx) = test_backend();
        let formats = vec![
            ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT),
            named(HTML, "HTML Format"),
        ];
        let utf16 = |t: &str| -> Vec<u8> {
            t.encode_utf16()
                .chain([0])
                .flat_map(u16::to_le_bytes)
                .collect()
        };
        backend.on_remote_copy(&formats);
        backend.on_remote_copy(&formats);
        backend.on_format_data_response(FormatDataResponse::new_data(utf16("hi")));
        backend.on_format_data_response(FormatDataResponse::new_data(utf16("hi")));
        assert_eq!(backend.fetch.got.text.as_deref(), Some("hi"));
        assert_eq!(
            backend.fetch.current,
            Some(Want::Html(ClipboardFormatId::new(HTML)))
        );
        let html = crate::clipboard_rich::encode_cf_html("<b>hi</b>");
        backend.on_format_data_response(FormatDataResponse::new_data(html));
        // Published: the fetch state is drained and nothing else is due.
        assert!(backend.fetch.current.is_none() && backend.fetch.queue.is_empty());
        let requests = std::iter::from_fn(|| rx.try_recv().ok()).count();
        assert_eq!(requests, 3, "text, text, html — one request at a time");
    }

    /// A superseded reply that never arrives can't stall the clipboard past
    /// the grace period.
    #[test]
    #[cfg(target_os = "macos")]
    fn a_lost_stale_reply_is_given_up_on() {
        let (mut backend, mut rx) = test_backend();
        backend.fetch.awaiting_stale =
            Some(std::time::Instant::now() - STALE_REPLY_GRACE - std::time::Duration::from_secs(1));
        backend.on_remote_copy(&[ClipboardFormat::new(ClipboardFormatId::CF_DIB)]);
        assert!(matches!(
            rx.try_recv(),
            Ok(ServerEvent::Clipboard(ClipboardMessage::SendInitiatePaste(id))) if id == ClipboardFormatId::CF_DIB
        ));
    }

    /// Regression pin for a bug caught in QA: publishing a remote TEXT/rich
    /// copy must mark it in `remote_write_cc` (echo suppression) and must NOT
    /// touch `self_change_count` — that one drives `cleanup_on_disconnect` and
    /// `clear_pasteboard_if_stale`, which would otherwise wipe text the user
    /// copied from Windows when the session ends. (Writes the real
    /// pasteboard, like the on_ready probes above.)
    #[test]
    #[cfg(target_os = "macos")]
    fn publishing_a_remote_copy_marks_the_echo_marker_not_the_file_marker() {
        use std::sync::atomic::Ordering;
        let (mut backend, _rx) = test_backend();
        backend.fetch.got.text = Some("remote text".into());
        backend.fetch.got.html = Some("<b>remote</b>".into());
        backend.publish_fetched();
        assert_eq!(
            backend.self_change_count.load(Ordering::Relaxed),
            -1,
            "the file-cleanup marker must be left alone"
        );
        assert_eq!(
            backend.remote_write_cc.load(Ordering::Relaxed),
            pb::change_count(),
            "our write must be recorded for the poller to skip"
        );
        assert_eq!(pb::read_html().as_deref(), Some("<b>remote</b>"));
        assert!(backend.fetch.got.is_empty(), "published state is consumed");
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
