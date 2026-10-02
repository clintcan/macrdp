//! Server-direction MS-RDPEAI (audio *input* redirection — the `AUDIO_INPUT` DVC).
//!
//! macrdp is the RDP **server**: the RDP client owns a microphone (a standalone
//! mic, or a webcam's built-in mic) and, when the user enables "Record from this
//! computer" (mstsc) / passes `/microphone` (FreeRDP), the client redirects it
//! over the MS-RDPEAI *Audio Input Redirection Virtual Channel Extension*. Unlike
//! MS-RDPECAM (camera) or MS-RDPEUSB (generic USB), audio input is a SINGLE
//! dynamic virtual channel named `AUDIO_INPUT` — there is no enumerator + per-device
//! split, and the server never has to open a second channel, so this collapses to
//! one [`DvcProcessor`] that drives the whole handshake through its `start()`/
//! `process()` return values (no event-sender needed).
//!
//! Full protocol negotiation, then the client's mic PCM is handed to the
//! [`AudinSampleSink`] the factory supplies (macrdp's feeds the "macrdp Microphone"
//! Core Audio device). To be replaced by upstream's `ironrdp-rdpeai` at the next
//! IronRDP version bump. The reference is FreeRDP's server `channels/audin/server/audin_main.c` — this
//! mirrors its state machine + wire format. Gated behind
//! `--enable-microphone-redirection`; when the factory isn't installed the channel
//! is never advertised and the build is byte-identical.
//!
//! **Handshake (MS-RDPEAI 1.3.3 / 3.2.5).** The SERVER speaks first once the DVC
//! opens: Version → (client Version) → Formats → (client Formats) → Open →
//! (client Open Reply) → the client streams Data Incoming + Data. macrdp is the
//! **receiver** (the client's mic is the source), so steady state is inbound Data
//! PDUs, not a request/response pull loop.
//!
//! Every message begins with a single `MessageId` byte (there is NO 2-byte
//! version+id `SHARED_MSG_HEADER` like MS-RDPECAM); all multi-byte integers are
//! little-endian. The processor is TOLERANT (log + `Ok(vec![])` on any decode
//! issue) so a malformed PDU never tears down the whole RDP session for an opt-in
//! feature.

use ironrdp_core::{Decode, Encode, EncodeResult, ReadCursor, WriteCursor, impl_as_any};
use ironrdp_dvc::{DvcEncode, DvcMessage, DvcProcessor, DvcServerProcessor};
use ironrdp_pdu::PduResult;
use ironrdp_rdpsnd::pdu::{AudioFormat, WaveFormat};
use tracing::{debug, info, warn};

/// The MS-RDPEAI audio-input dynamic virtual channel name (MS-RDPEAI 1.5 / 2.1).
pub const AUDIO_INPUT_CHANNEL_NAME: &str = "AUDIO_INPUT";

/// MS-RDPEAI `MessageId` values (the first byte of every PDU, MS-RDPEAI 2.2.2.1).
mod msg_id {
    pub const VERSION: u8 = 0x01;
    pub const FORMATS: u8 = 0x02;
    pub const OPEN: u8 = 0x03;
    pub const OPEN_REPLY: u8 = 0x04;
    pub const DATA_INCOMING: u8 = 0x05;
    pub const DATA: u8 = 0x06;
    pub const FORMAT_CHANGE: u8 = 0x07;
}

/// The MS-RDPEAI protocol version macrdp speaks. Version 1 is the lowest there is,
/// so there is nothing to negotiate down to: whatever the client supports, the
/// session runs at version 1. Later versions add nothing this path uses.
const OUR_VERSION: u32 = 1;

/// The one capture format macrdp accepts: 16-bit PCM at 44.1 kHz, mono or stereo.
/// It is what the "macrdp Microphone" device runs at, and there is no resampler,
/// so any other rate would play at the wrong pitch. mstsc offers it.
const PCM_BITS_PER_SAMPLE: u16 = 16;
const PCM_SAMPLE_RATE: u32 = 44_100;

// ---------------------------------------------------------------------------
// Outbound message encoder (server → client)
// ---------------------------------------------------------------------------

/// A server→client MS-RDPEAI message: the 1-byte `MessageId` followed by a
/// message-specific body, wrapped as a [`DvcEncode`] so the DVC layer frames it.
struct AudinMsg {
    msg_id: u8,
    body: Vec<u8>,
}

impl AudinMsg {
    #[expect(
        clippy::new_ret_no_self,
        reason = "builds the boxed DVC message the processor returns"
    )]
    fn new(msg_id: u8, body: Vec<u8>) -> DvcMessage {
        Box::new(Self { msg_id, body })
    }
}

impl Encode for AudinMsg {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        ironrdp_core::ensure_size!(in: dst, size: self.size());
        dst.write_u8(self.msg_id);
        dst.write_slice(&self.body);
        Ok(())
    }

    fn name(&self) -> &'static str {
        "AUDIO_INPUT_MSG"
    }

    fn size(&self) -> usize {
        1 + self.body.len()
    }
}

impl DvcEncode for AudinMsg {}

/// Encode any `Encode` value into an exact-size `Vec<u8>` (the `AUDIO_FORMAT`
/// WAVEFORMATEX bodies embedded in the Formats/Open PDUs).
fn encode_to_vec<T: Encode>(item: &T) -> Vec<u8> {
    let mut buf = vec![0u8; item.size()];
    let mut cursor = WriteCursor::new(&mut buf);
    // The buffer is sized to `item.size()` and every `Encode` here writes exactly
    // that many bytes, so this never fails.
    item.encode(&mut cursor)
        .expect("AUDIO_FORMAT encode into an exact-size buffer");
    buf
}

/// One PCM `AUDIO_FORMAT` (WAVEFORMATEX with `cbSize = 0`).
fn pcm_format(n_channels: u16, n_samples_per_sec: u32) -> AudioFormat {
    let n_block_align = n_channels * (PCM_BITS_PER_SAMPLE / 8);
    AudioFormat {
        format: WaveFormat::PCM,
        n_channels,
        n_samples_per_sec,
        n_avg_bytes_per_sec: n_samples_per_sec * u32::from(n_block_align),
        n_block_align,
        bits_per_sample: PCM_BITS_PER_SAMPLE,
        data: None,
    }
}

/// The PCM formats macrdp is willing to RECEIVE from the client's mic: 44.1 kHz
/// 16-bit, mono or stereo — see [`PCM_SAMPLE_RATE`].
pub fn server_input_formats() -> Vec<AudioFormat> {
    vec![pcm_format(1, PCM_SAMPLE_RATE), pcm_format(2, PCM_SAMPLE_RATE)]
}

/// Whether macrdp can take audio in this format — the check applied to whatever
/// the client offers, since a client may list formats the server didn't
/// advertise.
pub fn is_acceptable_capture_format(format: &AudioFormat) -> bool {
    format.format == WaveFormat::PCM
        && format.bits_per_sample == PCM_BITS_PER_SAMPLE
        && format.n_samples_per_sec == PCM_SAMPLE_RATE
        && matches!(format.n_channels, 1 | 2)
}

/// The first acceptable format in the client's Formats reply, with its index in
/// that list — the Open PDU's `initialFormat` must be that index, not 0.
pub fn choose_capture_format(client_formats: &[AudioFormat]) -> Option<(u32, &AudioFormat)> {
    client_formats
        .iter()
        .enumerate()
        .find(|(_, f)| is_acceptable_capture_format(f))
        .and_then(|(i, f)| Some((u32::try_from(i).ok()?, f)))
}

/// Build a `MSG_SNDIN_FORMATS` body: `NumFormats(u32) | cbSizeFormatsPacket(u32) |
/// AUDIO_FORMAT[]` (MS-RDPEAI 2.2.2.2). `cbSizeFormatsPacket` is the total byte
/// size of the encoded `SoundFormats` array.
fn formats_body(formats: &[AudioFormat]) -> Vec<u8> {
    let encoded: Vec<u8> = formats.iter().flat_map(encode_to_vec).collect();
    let mut body = Vec::with_capacity(8 + encoded.len());
    body.extend_from_slice(&(formats.len() as u32).to_le_bytes());
    body.extend_from_slice(&(encoded.len() as u32).to_le_bytes());
    body.extend_from_slice(&encoded);
    body
}

/// Build a `MSG_SNDIN_OPEN` body: `FramesPerPacket(u32) | initialFormat(u32) |
/// captureDeviceFormat(WAVEFORMATEX)` (MS-RDPEAI 2.2.2.3). `initialFormat` indexes
/// the *client's* Formats reply; we pick 0 and echo that format.
fn open_body(initial_format: u32, format: &AudioFormat) -> Vec<u8> {
    // ~10 ms of frames per packet, a conventional capture chunk.
    let frames_per_packet = (format.n_samples_per_sec / 100).max(1);
    let fmt = encode_to_vec(format);
    let mut body = Vec::with_capacity(8 + fmt.len());
    body.extend_from_slice(&frames_per_packet.to_le_bytes());
    body.extend_from_slice(&initial_format.to_le_bytes());
    body.extend_from_slice(&fmt);
    body
}

/// Decode a client `MSG_SNDIN_FORMATS` body into its `AUDIO_FORMAT` list. Returns
/// an empty vec on any short/garbled input (the processor is tolerant).
fn parse_client_formats(body: &[u8]) -> Vec<AudioFormat> {
    if body.len() < 8 {
        return Vec::new();
    }
    let num_formats = u32::from_le_bytes([body[0], body[1], body[2], body[3]]) as usize;
    // Skip NumFormats + cbSizeFormatsPacket; decode the AUDIO_FORMAT array.
    let mut cursor = ReadCursor::new(&body[8..]);
    let mut out = Vec::with_capacity(num_formats.min(64));
    for _ in 0..num_formats {
        match AudioFormat::decode(&mut cursor) {
            Ok(fmt) => out.push(fmt),
            Err(_) => break,
        }
    }
    out
}

/// The single little-endian `u32` that makes up the body of the client's Version
/// (its version), Open Reply (an HRESULT) and Format Change (a format index)
/// PDUs (MS-RDPEAI 2.2.2.1 / 2.2.2.4 / 2.2.2.7).
fn read_u32(body: &[u8]) -> Option<u32> {
    (body.len() >= 4).then(|| u32::from_le_bytes([body[0], body[1], body[2], body[3]]))
}

// ---------------------------------------------------------------------------
// Inbound mic sink (macrdp-provided; platform work lives behind it)
// ---------------------------------------------------------------------------

/// A macrdp-provided sink for the received-microphone path: it receives the
/// negotiated capture format once, then each inbound audio chunk. All the platform
/// work (Phase 2: an `AudioServerPlugIn` virtual mic + a shared-memory feed) lives
/// on the macrdp side behind this trait, so the vendored crate stays
/// platform-independent. Mirrors the MS-RDPECAM `CameraSampleSink` seam.
pub trait AudinSampleSink: Send {
    /// The negotiated capture format (from the Open PDU). Called once before data.
    fn on_format(&mut self, format: &AudioFormat);
    /// One inbound audio chunk (PCM in the negotiated format).
    fn on_data(&mut self, data: &[u8]);
}

// ---------------------------------------------------------------------------
// AUDIO_INPUT channel processor
// ---------------------------------------------------------------------------

/// MS-RDPEAI processor: drives Version → Formats → Open negotiation, then consumes
/// the client's inbound mic Data PDUs, handing them to the sink (with `None`,
/// the audio is only logged and dropped).
pub struct AudinServer {
    sink: Option<Box<dyn AudinSampleSink>>,
    /// The client's Formats reply, kept so a Format Change (an index into it) can
    /// be resolved.
    client_formats: Vec<AudioFormat>,
    /// The format the client is sending in, when it is one macrdp accepts. `None`
    /// before the Open, or after the client switched to one it doesn't: data is
    /// then dropped rather than fed to the sink as the wrong format.
    negotiated_format: Option<AudioFormat>,
    /// Running total of inbound audio bytes + a periodic-log throttle.
    data_bytes: u64,
    data_packets: u64,
}

impl AudinServer {
    pub fn new(sink: Option<Box<dyn AudinSampleSink>>) -> Self {
        Self {
            sink,
            client_formats: Vec::new(),
            negotiated_format: None,
            data_bytes: 0,
            data_packets: 0,
        }
    }
}

impl_as_any!(AudinServer);

impl DvcProcessor for AudinServer {
    fn channel_name(&self) -> &str {
        AUDIO_INPUT_CHANNEL_NAME
    }

    fn start(&mut self, _channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        // The server speaks first: advertise our protocol version.
        info!("MS-RDPEAI AUDIO_INPUT channel opened — sending Version");
        Ok(vec![AudinMsg::new(msg_id::VERSION, OUR_VERSION.to_le_bytes().to_vec())])
    }

    fn process(&mut self, _channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        // TOLERANT: never propagate — a decode error would tear down the whole
        // session for an opt-in feature.
        let Some((&message_id, body)) = payload.split_first() else {
            warn!("MS-RDPEAI message empty — ignoring");
            return Ok(Vec::new());
        };

        match message_id {
            msg_id::VERSION => {
                // Version 1 is the minimum, so the session runs at 1 whatever the
                // client reports; the value is only logged.
                let client_version = read_u32(body);
                info!(
                    client_version = client_version.unwrap_or(0),
                    session_version = OUR_VERSION,
                    "MS-RDPEAI client Version — sending server Sound Formats"
                );
                let formats = server_input_formats();
                Ok(vec![AudinMsg::new(msg_id::FORMATS, formats_body(&formats))])
            }
            msg_id::FORMATS => {
                self.client_formats = parse_client_formats(body);
                let Some((index, chosen)) = choose_capture_format(&self.client_formats) else {
                    warn!(
                        offered = self.client_formats.len(),
                        "MS-RDPEAI client offered no 16-bit PCM 44.1 kHz format — not opening the mic"
                    );
                    return Ok(Vec::new());
                };
                let chosen = chosen.clone();
                info!(
                    index,
                    channels = chosen.n_channels,
                    rate = chosen.n_samples_per_sec,
                    bits = chosen.bits_per_sample,
                    "MS-RDPEAI client Sound Formats — opening the mic"
                );
                let open = open_body(index, &chosen);
                if let Some(sink) = self.sink.as_mut() {
                    sink.on_format(&chosen);
                }
                self.negotiated_format = Some(chosen);
                Ok(vec![AudinMsg::new(msg_id::OPEN, open)])
            }
            msg_id::OPEN_REPLY => {
                let hr = read_u32(body).unwrap_or(0xFFFF_FFFF);
                if hr == 0 {
                    info!("MS-RDPEAI Open Reply S_OK — client will now stream the mic");
                } else {
                    warn!(hresult = format_args!("0x{hr:08x}"), "MS-RDPEAI Open Reply failed");
                }
                Ok(Vec::new())
            }
            msg_id::DATA_INCOMING => {
                // A Data PDU follows; nothing to send.
                debug!("MS-RDPEAI Data Incoming");
                Ok(Vec::new())
            }
            msg_id::DATA => {
                self.data_bytes += body.len() as u64;
                self.data_packets += 1;
                if self.negotiated_format.is_some()
                    && let Some(sink) = self.sink.as_mut()
                {
                    sink.on_data(body);
                }
                // Log the first packet, then throttle to ~every 200 packets (~2 s).
                if self.data_packets == 1 || self.data_packets.is_multiple_of(200) {
                    info!(
                        packets = self.data_packets,
                        total_bytes = self.data_bytes,
                        last_len = body.len(),
                        "MS-RDPEAI receiving microphone audio from the client"
                    );
                }
                Ok(Vec::new())
            }
            msg_id::FORMAT_CHANGE => {
                // The client switched to another entry of its Formats reply. Feed
                // it on only if macrdp can take it; otherwise drop the data from
                // here on rather than feed it to the sink as the wrong format.
                let index = read_u32(body);
                let format = index
                    .and_then(|i| usize::try_from(i).ok())
                    .and_then(|i| self.client_formats.get(i))
                    .filter(|f| is_acceptable_capture_format(f))
                    .cloned();
                match format {
                    Some(format) => {
                        info!(index, "MS-RDPEAI Format Change — continuing in the new format");
                        if let Some(sink) = self.sink.as_mut() {
                            sink.on_format(&format);
                        }
                        self.negotiated_format = Some(format);
                    }
                    None => {
                        warn!(
                            index,
                            "MS-RDPEAI Format Change to an unsupported format — dropping the mic audio"
                        );
                        self.negotiated_format = None;
                    }
                }
                Ok(Vec::new())
            }
            other => {
                debug!(
                    message_id = format_args!("0x{other:02x}"),
                    "MS-RDPEAI message not handled — ignoring"
                );
                Ok(Vec::new())
            }
        }
    }
}

impl DvcServerProcessor for AudinServer {}

/// Factory for the MS-RDPEAI processor. Unlike the camera/USB factories it needs no
/// `ServerEventSender` — MS-RDPEAI is a single channel with no per-device open, so
/// the processor never asks the event loop to open anything.
pub trait AudinServerFactory: Send {
    fn build_processor(&self) -> AudinServer;
}
