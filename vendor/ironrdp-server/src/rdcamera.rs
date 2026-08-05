//! Server-direction MS-RDPECAM (`RDCamera_Device_Enumerator` + per-device DVCs).
//!
//! macrdp is the RDP **server**: the RDP client owns a physical webcam and, when
//! the user enables "Video capture devices" in the client's local-resources, the
//! client redirects it over the MS-RDPECAM *Video Capture Virtual Channel
//! Extension*. Redirection begins on a single enumeration channel,
//! `RDCamera_Device_Enumerator`; the client announces each camera on it
//! (`DEVICE_ADDED_NOTIFICATION`) with a per-device DVC name, and the server opens
//! that per-device channel and drives the stream. See
//! `docs/rdp-camera-redirection-feasibility.md` +
//! `~/.claude/plans/camera-redirection-phase1.md`.
//!
//! **Phase 1 (this module): full protocol negotiation → receiving samples over
//! TCP, logged.** No decode (Phase 2 — VideoToolbox), no macOS camera (Phase 3 —
//! CoreMediaIO), no UDP migration (Phase 4). The reference is FreeRDP's *server*
//! (`channels/rdpecam/server/camera_device_main.c`) — this mirrors its state
//! machine + wire format. Gated behind `--enable-camera-redirection`; when the
//! factory isn't installed the channel is never advertised and the build is
//! byte-identical.
//!
//! **Two DVCs.** The enumerator ([`RdCameraServer`]) handles version negotiation
//! and device add/remove; on a device add it can't open a DVC itself (only the
//! event loop reaches `DrdynvcServer`), so it signals
//! [`ServerEvent::Camera`](crate::ServerEvent)`(`[`CameraServerMessage::OpenDeviceChannel`]`)`.
//! The event loop opens the client-named per-device channel with an
//! [`RdCameraDeviceProcessor`], which drives Activate → StreamList → MediaTypeList
//! → StartStreams → the SampleRequest↔SampleResponse pull loop — entirely through
//! its `start()`/`process()` return values (no event-sender needed on the device
//! side).
//!
//! Every message begins with a 2-byte `SHARED_MSG_HEADER` = `Version(u8)` +
//! `MessageId(u8)`; all multi-byte integers are little-endian. Both processors are
//! TOLERANT (log + `Ok(vec![])` on any decode issue) so a malformed PDU never
//! tears down the whole RDP session for an opt-in feature.

use std::time::Instant;
use tokio::sync::mpsc;

use ironrdp_core::{Encode, EncodeResult, WriteCursor, impl_as_any};
use ironrdp_dvc::{DvcEncode, DvcMessage, DvcProcessor, DvcServerProcessor};
use ironrdp_pdu::PduResult;
use tracing::{debug, info, warn};

use crate::{ServerEvent, ServerEventSender};

/// The MS-RDPECAM enumeration dynamic virtual channel name (MS-RDPECAM 1.5).
pub const RDCAMERA_CHANNEL_NAME: &str = "RDCamera_Device_Enumerator";

/// MS-RDPECAM `MessageId` values (the 2nd byte of `SHARED_MSG_HEADER`). Property
/// messages (0x14–0x18, v2-only) are intentionally unused — Phase 1 needs only
/// video.
mod msg_id {
    pub const SUCCESS_RESPONSE: u8 = 0x01;
    pub const ERROR_RESPONSE: u8 = 0x02;
    pub const SELECT_VERSION_REQUEST: u8 = 0x03;
    pub const SELECT_VERSION_RESPONSE: u8 = 0x04;
    pub const DEVICE_ADDED_NOTIFICATION: u8 = 0x05;
    pub const DEVICE_REMOVED_NOTIFICATION: u8 = 0x06;
    pub const ACTIVATE_DEVICE_REQUEST: u8 = 0x07;
    #[allow(dead_code)] // teardown — sent in Phase 4 lifecycle, defined now
    pub const DEACTIVATE_DEVICE_REQUEST: u8 = 0x08;
    pub const STREAM_LIST_REQUEST: u8 = 0x09;
    pub const STREAM_LIST_RESPONSE: u8 = 0x0A;
    pub const MEDIA_TYPE_LIST_REQUEST: u8 = 0x0B;
    pub const MEDIA_TYPE_LIST_RESPONSE: u8 = 0x0C;
    pub const START_STREAMS_REQUEST: u8 = 0x0F;
    #[allow(dead_code)] // teardown — Phase 4
    pub const STOP_STREAMS_REQUEST: u8 = 0x10;
    pub const SAMPLE_REQUEST: u8 = 0x11;
    pub const SAMPLE_RESPONSE: u8 = 0x12;
    pub const SAMPLE_ERROR_RESPONSE: u8 = 0x13;
}

/// MS-RDPECAM `CAM_MEDIA_FORMAT` (the `Format` byte of MEDIA_TYPE_DESCRIPTION).
mod format {
    pub const H264: u8 = 0x01;
    pub const MJPG: u8 = 0x02;
    pub const NV12: u8 = 0x04;
    pub const I420: u8 = 0x05;
}

/// `CAM_MEDIA_TYPE_DESCRIPTION_FLAGS::DecodingRequired` — set for compressed
/// formats (H264/MJPG) in the media type we ask the client to start.
const FLAG_DECODING_REQUIRED: u8 = 0x01;

/// Highest MS-RDPECAM version this server advertises (v2 adds device properties,
/// which we don't drive; the header version just has to match what we negotiate).
const OUR_MAX_VERSION: u8 = 2;

/// A fixed-size MEDIA_TYPE_DESCRIPTION (MS-RDPECAM §2.2.3.8.1) — 26 bytes packed
/// little-endian. We keep the raw 26 bytes verbatim so StartStreamsRequest can
/// echo the client's chosen entry byte-for-byte.
const MEDIA_TYPE_DESC_LEN: usize = 26;

/// How many `SampleRequest`s to keep outstanding. MS-RDPECAM is a PULL model —
/// one SampleResponse per SampleRequest — so throughput is round-trip-bound
/// unless several requests are in flight. See §2.2.3.13/§2.2.3.14.
const SAMPLE_PIPELINE_DEPTH: u32 = 4;

// ---------------------------------------------------------------------------
// Outbound message encoder (server → client)
// ---------------------------------------------------------------------------

/// A server→client MS-RDPECAM message: the 2-byte `SHARED_MSG_HEADER` followed by
/// a message-specific body, wrapped as a [`DvcEncode`] so the DVC layer frames it.
struct CameraMsg {
    version: u8,
    msg_id: u8,
    body: Vec<u8>,
}

impl CameraMsg {
    // Returns the boxed DvcMessage wire form, not Self — an intentional
    // message-constructor shape (mirrors rdpeusb.rs's dvc_msg helpers).
    #[allow(clippy::new_ret_no_self)]
    fn new(version: u8, msg_id: u8, body: Vec<u8>) -> DvcMessage {
        Box::new(Self { version, msg_id, body })
    }
}

impl Encode for CameraMsg {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        ironrdp_core::ensure_size!(in: dst, size: self.size());
        dst.write_u8(self.version);
        dst.write_u8(self.msg_id);
        dst.write_slice(&self.body);
        Ok(())
    }

    fn name(&self) -> &'static str {
        "RDCAMERA_MSG"
    }

    fn size(&self) -> usize {
        2 + self.body.len()
    }
}

impl DvcEncode for CameraMsg {}

// ---------------------------------------------------------------------------
// Enumerator channel processor (RDCamera_Device_Enumerator)
// ---------------------------------------------------------------------------

/// Server-loop action the enumerator processor can't perform itself (opening a
/// DVC needs `&mut DrdynvcServer`, which only the event loop holds). Delivered via
/// [`ServerEvent::Camera`](crate::ServerEvent).
pub enum CameraServerMessage {
    /// The client announced a camera; open a per-device DVC named `channel_name`
    /// with an [`RdCameraDeviceProcessor`] negotiating at `version`.
    OpenDeviceChannel { channel_name: String, version: u8 },
}

impl core::fmt::Debug for CameraServerMessage {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::OpenDeviceChannel { channel_name, version } => f
                .debug_struct("OpenDeviceChannel")
                .field("channel_name", channel_name)
                .field("version", version)
                .finish(),
        }
    }
}

/// MS-RDPECAM enumeration-channel processor: answers version negotiation and, on a
/// `DEVICE_ADDED_NOTIFICATION`, asks the event loop to open the per-device channel.
pub struct RdCameraServer {
    sender: Option<mpsc::UnboundedSender<ServerEvent>>,
    negotiated_version: u8,
}

impl RdCameraServer {
    pub fn new() -> Self {
        Self {
            sender: None,
            negotiated_version: 1,
        }
    }

    /// Build a processor wired to the connection's server-event sender so it can
    /// request per-device channel creation.
    pub fn with_sender(sender: Option<mpsc::UnboundedSender<ServerEvent>>) -> Self {
        Self {
            sender,
            negotiated_version: 1,
        }
    }
}

impl Default for RdCameraServer {
    fn default() -> Self {
        Self::new()
    }
}

impl_as_any!(RdCameraServer);

impl DvcProcessor for RdCameraServer {
    fn channel_name(&self) -> &str {
        RDCAMERA_CHANNEL_NAME
    }

    fn start(&mut self, _channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        // The client speaks first (SelectVersionRequest); nothing to send on open.
        info!("MS-RDPECAM enumeration channel opened — waiting for the client's SelectVersionRequest");
        Ok(Vec::new())
    }

    fn process(&mut self, _channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        // TOLERANT: never propagate — a decode error would tear down the whole
        // session for an opt-in feature.
        let Some((version, message_id, body)) = split_header(payload) else {
            warn!(
                len = payload.len(),
                "MS-RDPECAM enumerator message too short — ignoring"
            );
            return Ok(Vec::new());
        };

        match message_id {
            msg_id::SELECT_VERSION_REQUEST => {
                let selected = version.clamp(1, OUR_MAX_VERSION);
                self.negotiated_version = selected;
                info!(
                    client_version = version,
                    selected_version = selected,
                    "MS-RDPECAM SelectVersionRequest — replying SelectVersionResponse"
                );
                Ok(vec![CameraMsg::new(
                    selected,
                    msg_id::SELECT_VERSION_RESPONSE,
                    Vec::new(),
                )])
            }
            msg_id::DEVICE_ADDED_NOTIFICATION => {
                // Layout after the header: DeviceName (null-term UTF-16LE) then
                // VirtualChannelName (null-term ASCII). The per-device DVC name is
                // the VirtualChannelName; the client owns it (we open exactly it).
                let (device_name, rest) = read_utf16_z(body);
                let channel_name = read_ascii_z(rest);
                info!(
                    version,
                    device_name = %device_name,
                    virtual_channel = %channel_name,
                    "MS-RDPECAM DEVICE_ADDED — opening the per-device channel"
                );
                if channel_name.is_empty() {
                    warn!("MS-RDPECAM DEVICE_ADDED with empty VirtualChannelName — cannot open a per-device channel");
                    return Ok(Vec::new());
                }
                if let Some(sender) = self.sender.as_ref() {
                    let _ = sender.send(ServerEvent::Camera(CameraServerMessage::OpenDeviceChannel {
                        channel_name,
                        version: self.negotiated_version,
                    }));
                } else {
                    warn!("MS-RDPECAM DEVICE_ADDED but no server-event sender — cannot open the per-device channel");
                }
                Ok(Vec::new())
            }
            msg_id::DEVICE_REMOVED_NOTIFICATION => {
                let channel_name = read_ascii_z(body);
                info!(version, virtual_channel = %channel_name, "MS-RDPECAM DEVICE_REMOVED");
                Ok(Vec::new())
            }
            other => {
                debug!(
                    version,
                    message_id = format_args!("0x{other:02x}"),
                    "MS-RDPECAM enumerator message not handled — ignoring"
                );
                Ok(Vec::new())
            }
        }
    }
}

impl DvcServerProcessor for RdCameraServer {}

impl ServerEventSender for RdCameraServer {
    fn set_sender(&mut self, sender: mpsc::UnboundedSender<ServerEvent>) {
        self.sender = Some(sender);
    }
}

/// A macrdp-provided sink for the decoded-camera path: it receives the negotiated
/// media type once and then each raw sample (one complete frame). The vendored
/// device processor just feeds it — all the platform work (Phase 2 VideoToolbox
/// decode, Phase 3 CoreMediaIO presentation) lives on the macrdp side behind this
/// trait, so the vendored crate stays platform-independent. Mirrors the URBDRC
/// `device_callback` seam.
pub trait CameraSampleSink: Send {
    /// The media type the server started: the `CAM_MEDIA_FORMAT` byte (H264=0x01,
    /// MJPG=0x02, NV12=0x04, …) + pixel dimensions. Called once before samples.
    fn on_media_type(&mut self, format: u8, width: u32, height: u32);
    /// One complete sample = one frame. For H264 this is an Annex-B access unit
    /// with in-band SPS/PPS (per MS-RDPECAM §2.2.3.8.1).
    fn on_sample(&mut self, data: &[u8]);
}

/// Factory for the MS-RDPECAM enumeration processor. `ServerEventSender` so the
/// server can hand each connection's event sender to the enumerator (to request
/// per-device channel opens), mirroring the URBDRC factory.
pub trait RdCameraServerFactory: ServerEventSender + Send {
    fn build_processor(&self) -> RdCameraServer;
    /// Build a per-device sample sink (Phase 2+: decode/present). `None` (default)
    /// keeps Phase-1 behavior — negotiate + log samples, drop them.
    fn build_sample_sink(&self) -> Option<Box<dyn CameraSampleSink>> {
        None
    }
}

// ---------------------------------------------------------------------------
// Per-device channel processor
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeviceState {
    /// Sent ActivateDeviceRequest, awaiting Success.
    Activating,
    /// Sent StreamListRequest, awaiting StreamListResponse.
    ListingStreams,
    /// Sent MediaTypeListRequest, awaiting MediaTypeListResponse.
    ListingMediaTypes,
    /// Sent StartStreamsRequest, awaiting Success.
    Starting,
    /// Streaming — SampleRequest↔SampleResponse pull loop.
    Streaming,
    /// Negotiation failed / stopped; inert.
    Done,
}

/// Per-device MS-RDPECAM processor (Phase 1): drives the negotiation and logs the
/// incoming sample stream. Entirely `start()`/`process()`-driven — no event
/// sender, since every step is a synchronous request/response on this channel.
pub struct RdCameraDeviceProcessor {
    channel_name: String,
    version: u8,
    state: DeviceState,
    /// The stream we picked to drive (Phase 1: the first, index 0).
    stream_index: u8,
    samples: u64,
    sample_bytes: u64,
    first_sample_at: Option<Instant>,
    last_log_at: Option<Instant>,
    /// Phase 2+ decode/present sink (macrdp side); `None` = Phase-1 log-and-drop.
    sink: Option<Box<dyn CameraSampleSink>>,
}

impl RdCameraDeviceProcessor {
    pub fn new(channel_name: String, version: u8, sink: Option<Box<dyn CameraSampleSink>>) -> Self {
        Self {
            channel_name,
            version,
            state: DeviceState::Activating,
            stream_index: 0,
            samples: 0,
            sample_bytes: 0,
            first_sample_at: None,
            last_log_at: None,
            sink,
        }
    }

    fn msg(&self, msg_id: u8, body: Vec<u8>) -> DvcMessage {
        CameraMsg::new(self.version, msg_id, body)
    }

    /// `n` SampleRequests to fill the pipeline (or one, to keep it full).
    fn sample_requests(&self, n: u32) -> Vec<DvcMessage> {
        (0..n)
            .map(|_| self.msg(msg_id::SAMPLE_REQUEST, vec![self.stream_index]))
            .collect()
    }
}

impl_as_any!(RdCameraDeviceProcessor);

impl DvcProcessor for RdCameraDeviceProcessor {
    fn channel_name(&self) -> &str {
        // The client named this channel in DEVICE_ADDED; we opened exactly it.
        &self.channel_name
    }

    fn start(&mut self, channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        info!(
            channel_id,
            channel = %self.channel_name,
            "MS-RDPECAM per-device channel opened — sending ActivateDeviceRequest"
        );
        self.state = DeviceState::Activating;
        Ok(vec![self.msg(msg_id::ACTIVATE_DEVICE_REQUEST, Vec::new())])
    }

    fn process(&mut self, _channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        // TOLERANT: log + Ok(vec![]) on any problem; never tear down the session.
        let Some((_version, message_id, body)) = split_header(payload) else {
            warn!(len = payload.len(), "MS-RDPECAM device message too short — ignoring");
            return Ok(Vec::new());
        };

        match message_id {
            msg_id::SUCCESS_RESPONSE => match self.state {
                DeviceState::Activating => {
                    debug!("MS-RDPECAM device activated — requesting stream list");
                    self.state = DeviceState::ListingStreams;
                    Ok(vec![self.msg(msg_id::STREAM_LIST_REQUEST, Vec::new())])
                }
                DeviceState::Starting => {
                    info!(
                        channel = %self.channel_name,
                        pipeline = SAMPLE_PIPELINE_DEPTH,
                        "MS-RDPECAM StartStreams acked — beginning the sample pull loop"
                    );
                    self.state = DeviceState::Streaming;
                    Ok(self.sample_requests(SAMPLE_PIPELINE_DEPTH))
                }
                _ => {
                    debug!(state = ?self.state, "MS-RDPECAM unexpected SuccessResponse — ignoring");
                    Ok(Vec::new())
                }
            },
            msg_id::ERROR_RESPONSE => {
                let code = read_u32(body).unwrap_or(0);
                warn!(
                    state = ?self.state,
                    error_code = format_args!("0x{code:08x}"),
                    error = error_name(code),
                    "MS-RDPECAM ErrorResponse — stopping this device"
                );
                self.state = DeviceState::Done;
                Ok(Vec::new())
            }
            msg_id::STREAM_LIST_RESPONSE => {
                // The response is an array of STREAM_DESCRIPTIONs; Phase 1 drives
                // the first stream (index 0), the camera's main video stream.
                info!(
                    len = body.len(),
                    "MS-RDPECAM StreamListResponse — requesting media types for stream 0"
                );
                self.stream_index = 0;
                self.state = DeviceState::ListingMediaTypes;
                Ok(vec![self.msg(msg_id::MEDIA_TYPE_LIST_REQUEST, vec![self.stream_index])])
            }
            msg_id::MEDIA_TYPE_LIST_RESPONSE => {
                // Body = N × 26-byte MEDIA_TYPE_DESCRIPTION. Pick the best format
                // we'll be able to decode (H264 ≫ MJPG ≫ NV12/I420), copy that
                // 26-byte descriptor verbatim into StartStreamsRequest.
                let Some(chosen) = pick_media_type(body) else {
                    warn!(
                        len = body.len(),
                        "MS-RDPECAM MediaTypeListResponse had no usable media type — stopping"
                    );
                    self.state = DeviceState::Done;
                    return Ok(Vec::new());
                };
                let d = describe_media_type(&chosen);
                info!(
                    channel = %self.channel_name,
                    media = %d,
                    "MS-RDPECAM chose a media type — sending StartStreamsRequest"
                );
                // Tell the sink the negotiated format + dimensions so it can
                // configure its decoder before the first sample.
                if let Some(sink) = self.sink.as_mut() {
                    let u32_at =
                        |o: usize| u32::from_le_bytes([chosen[o], chosen[o + 1], chosen[o + 2], chosen[o + 3]]);
                    sink.on_media_type(chosen[0], u32_at(1), u32_at(5));
                }
                // StartStreamsRequest body = one START_STREAM_INFO =
                // StreamIndex(u8) + 26-byte MediaTypeDescription. NO leading count:
                // on the wire the count is implicit (PDU length / 27) — FreeRDP's
                // server writes only the 27-byte entries and its client parser
                // reads 1+26 directly with no count field. A stray N_Infos byte
                // here shifts the struct by one → the client rejects it as
                // InvalidMessage (verified live). (The C struct's N_Infos/[255]
                // array is in-memory only; mstsc sends/accepts exactly one entry.)
                let mut b = Vec::with_capacity(1 + MEDIA_TYPE_DESC_LEN);
                b.push(self.stream_index);
                b.extend_from_slice(&chosen);
                self.state = DeviceState::Starting;
                Ok(vec![self.msg(msg_id::START_STREAMS_REQUEST, b)])
            }
            msg_id::SAMPLE_RESPONSE => {
                // Body = StreamIndex(u8) + raw frame bytes (one PDU = one frame;
                // the DVC layer already reassembled any chunking).
                let frame = body.get(1..).unwrap_or(&[]);
                let frame_len = frame.len();
                self.samples += 1;
                self.sample_bytes += frame_len as u64;
                // Hand the frame to the decode/present sink (Phase 2+).
                if let Some(sink) = self.sink.as_mut() {
                    sink.on_sample(frame);
                }
                let now = Instant::now();
                let first = *self.first_sample_at.get_or_insert(now);
                // Throttled ≤1/s summary at info, so it shows under the default filter.
                if self
                    .last_log_at
                    .map(|t| now.saturating_duration_since(t).as_millis() >= 1000)
                    .unwrap_or(true)
                {
                    self.last_log_at = Some(now);
                    let secs = now.saturating_duration_since(first).as_secs_f64().max(0.001);
                    let fps = self.samples as f64 / secs;
                    info!(
                        channel = %self.channel_name,
                        samples = self.samples,
                        last_frame_bytes = frame_len,
                        avg_fps = format_args!("{fps:.1}"),
                        "MS-RDPECAM sample received (Phase-1 GREEN — frames are flowing)"
                    );
                }
                // Keep the pipeline full: one request in, one out.
                Ok(self.sample_requests(1))
            }
            msg_id::SAMPLE_ERROR_RESPONSE => {
                debug!("MS-RDPECAM SampleErrorResponse — re-requesting");
                Ok(self.sample_requests(1))
            }
            other => {
                debug!(
                    message_id = format_args!("0x{other:02x}"),
                    state = ?self.state,
                    "MS-RDPECAM device message not handled — ignoring"
                );
                Ok(Vec::new())
            }
        }
    }
}

impl DvcServerProcessor for RdCameraDeviceProcessor {}

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

/// Split off the 2-byte `SHARED_MSG_HEADER`; `None` if too short.
fn split_header(payload: &[u8]) -> Option<(u8, u8, &[u8])> {
    if payload.len() < 2 {
        return None;
    }
    Some((payload[0], payload[1], &payload[2..]))
}

fn read_u32(buf: &[u8]) -> Option<u32> {
    (buf.len() >= 4).then(|| u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]))
}

/// MS-RDPECAM `CAM_ERROR_CODE` (§2.2.3.2). The enum ends at 0x0A; anything above
/// is an internal client/device error (e.g. Media Foundation) surfaced through
/// the ErrorCode field, NOT a protocol verdict — most often the client failing to
/// open its own physical camera (busy / privacy-blocked / no working device).
fn error_name(code: u32) -> &'static str {
    match code {
        0x01 => "UnexpectedError",
        0x02 => "InvalidMessage",
        0x03 => "NotInitialized",
        0x04 => "InvalidRequest",
        0x05 => "InvalidStreamNumber",
        0x06 => "InvalidMediaType",
        0x07 => "OutOfMemory",
        0x08 => "ItemNotFound",
        0x09 => "SetNotFound",
        0x0A => "OperationNotSupported",
        _ => "client/device-internal (not a protocol code — likely the client couldn't open its camera)",
    }
}

/// Choose the best decodable media type from a MediaTypeListResponse body (an
/// array of 26-byte MEDIA_TYPE_DESCRIPTIONs). Prefer H264, then MJPG, then a raw
/// format, then the first entry. Returns the chosen 26 bytes verbatim.
fn pick_media_type(body: &[u8]) -> Option<[u8; MEDIA_TYPE_DESC_LEN]> {
    let count = body.len() / MEDIA_TYPE_DESC_LEN;
    if count == 0 {
        return None;
    }
    let entry = |i: usize| -> [u8; MEDIA_TYPE_DESC_LEN] {
        let off = i * MEDIA_TYPE_DESC_LEN;
        let mut d = [0u8; MEDIA_TYPE_DESC_LEN];
        d.copy_from_slice(&body[off..off + MEDIA_TYPE_DESC_LEN]);
        d
    };
    let rank = |fmt: u8| -> u8 {
        match fmt {
            format::H264 => 4,
            format::MJPG => 3,
            format::NV12 | format::I420 => 2,
            _ => 1,
        }
    };
    let mut best: Option<(u8, [u8; MEDIA_TYPE_DESC_LEN])> = None; // (rank, desc)
    for i in 0..count {
        let d = entry(i);
        let r = rank(d[0]);
        if best.as_ref().map(|(br, _)| r > *br).unwrap_or(true) {
            best = Some((r, d));
        }
    }
    // Ensure the descriptor we START with carries DecodingRequired for a
    // compressed format (some clients leave it clear in the list entry).
    best.map(|(_, mut d)| {
        if matches!(d[0], format::H264 | format::MJPG) {
            d[25] |= FLAG_DECODING_REQUIRED;
        }
        d
    })
}

/// One-line human description of a 26-byte MEDIA_TYPE_DESCRIPTION for logging.
fn describe_media_type(d: &[u8; MEDIA_TYPE_DESC_LEN]) -> String {
    let fmt = match d[0] {
        format::H264 => "H264",
        format::MJPG => "MJPG",
        format::NV12 => "NV12",
        format::I420 => "I420",
        0x03 => "YUY2",
        0x06 => "RGB24",
        0x07 => "RGB32",
        other => return format!("fmt=0x{other:02x}"),
    };
    let u32_at = |o: usize| u32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]]);
    let (w, h) = (u32_at(1), u32_at(5));
    let (num, den) = (u32_at(9), u32_at(13));
    let fps = if den != 0 { num as f64 / den as f64 } else { 0.0 };
    format!("{fmt} {w}x{h} @{fps:.0}fps")
}

/// Read a null-terminated UTF-16LE string from the front of `buf`, bounded to
/// `buf`. Returns the decoded (lossy) string and the bytes AFTER the terminator.
fn read_utf16_z(buf: &[u8]) -> (String, &[u8]) {
    let mut units = Vec::new();
    let mut i = 0;
    while i + 1 < buf.len() {
        let u = u16::from_le_bytes([buf[i], buf[i + 1]]);
        i += 2;
        if u == 0 {
            return (String::from_utf16_lossy(&units), &buf[i..]);
        }
        units.push(u);
    }
    (String::from_utf16_lossy(&units), &[])
}

/// Read a null-terminated ASCII string from the front of `buf`, bounded to `buf`.
fn read_ascii_z(buf: &[u8]) -> String {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}
