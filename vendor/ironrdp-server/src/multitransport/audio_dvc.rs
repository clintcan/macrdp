//! (vendored, feature=multitransport, Phase 2 / P2.4b) The
//! `AUDIO_PLAYBACK_LOSSY_DVC` dynamic virtual channel — MS-RDPEA audio output over
//! a DVC, destined for the lossy UDP tunnel.
//!
//! MS-RDPEA §2.1: the audio output channel is named `AUDIO_PLAYBACK_DVC` over a
//! reliable transport and **`AUDIO_PLAYBACK_LOSSY_DVC` over an unreliable UDP
//! transport**.
//!
//! **DUAL-CHANNEL TOPOLOGY (P2.4b-iv — corrects the earlier "handshake over the
//! tunnel" plan).** mstsc tears the whole session down if it receives Server Audio
//! Formats on the `_LOSSY_`-named channel — verified BOTH over TCP/DRDYNVC
//! (P2.4b-1) AND over the migrated lossy/DTLS tunnel (P2.4b-iii). The transport is
//! not the discriminator; the channel *name* is. So the server opens BOTH channels:
//! the format/quality/training handshake runs on the reliable `AUDIO_PLAYBACK_DVC`
//! over TCP, while `AUDIO_PLAYBACK_LOSSY_DVC` is Soft-Synced onto the lossy
//! (`UDPFECL`) tunnel and carries **only `Wave2` audio data**, stamped with the
//! format index the reliable channel negotiated (the lossy channel inherits that
//! negotiation — it never negotiates its own). This matches how Windows' own RDP
//! server drives lossy audio.
//!
//! **Gating (MS-RDPEA Appendix A note <2>):** a client uses the lossy DVC only when
//! all of (a) a lossy UDP transport is available, (b) both ends are protocol
//! version ≥ 8, and (c) AAC is the codec. So we advertise `Version::V8` and the
//! caller passes a format list that includes AAC (`WAVE_FORMAT_AAC_MS`).
//!
//! **STATUS: P2.4b-2 spike, VERIFIED on real mstsc 2026-06-27.** Registered only when
//! the application calls
//! [`RdpServer::set_multitransport_lossy_audio_formats`](crate::RdpServer::set_multitransport_lossy_audio_formats)
//! (macrdp gates that behind the experimental `MACRDP_UDP_LOSSY_AUDIO` env), so the
//! default build is byte-unchanged.
//!
//! **KEY FINDING #1 (P2.4b-1) — the EGFX "negotiate-on-TCP-then-Soft-Sync" pattern
//! does NOT carry to a *lossy*-named channel.** With the literal name
//! `AUDIO_PLAYBACK_LOSSY_DVC`, mstsc accepts the DVC Create but, on receiving Server
//! Audio Formats over TCP/DRDYNVC, goes silent and tears down the whole TCP connection
//! (broken pipe a few seconds later — it kills EGFX too, not just audio). The reliable
//! name `AUDIO_PLAYBACK_DVC` (diagnostic env `MACRDP_AUDIO_DVC_RELIABLE=1`) handshakes
//! perfectly over TCP (formats → client formats(AAC) → quality mode → training →
//! confirm), audio plays, EGFX stays healthy — proving the channel *name* is the
//! blocker, not the PDU/framing and not coexistence with static rdpsnd (dual
//! negotiation is fine).
//!
//! **KEY FINDING #2 (P2.4b-2, the linchpin de-risk) — mstsc ACCEPTS a *lossy* Soft-Sync
//! of the audio DVC without tearing down.** This handler now opens the lossy DVC and
//! returns NO formats from `start()` (defer over TCP — finding #1), then the server
//! Soft-Syncs the channel onto the lossy (`TUNNELTYPE_UDPFECL=0x03`) tunnel. Live mstsc
//! result: `SoftSyncResponsePdu { tunnels: [3] }` (UDPFECL accepted), session stayed alive.
//! So the lossy Soft-Sync itself is fine.
//!
//! **KEY FINDING #3 (P2.4b-iii) — formats over the lossy tunnel ALSO tear mstsc down.**
//! Running the handshake over the tunnel (the earlier plan) was verified to fail with the
//! identical 3–4 s teardown as over TCP, while an EGFX-over-DTLS isolation test rendered
//! cleanly (so the tunnel data path is correct). Conclusion: the `_LOSSY_` channel must
//! never carry formats at all — hence the dual-channel topology above (handshake on the
//! reliable DVC, waves on the lossy one). See `docs/rdp-udp-multitransport-feasibility.md`
//! ("P2.4b").
//!
//! **Sequence (MS-RDPEA Initialization Sequence, confirmed live):** for v6+ the client
//! sends a Quality Mode PDU immediately after Client Audio Formats, and the server
//! sends Training only after that — so this handler records the chosen format on Client
//! Audio Formats and replies with Training on Quality Mode.
//!
//! **PDU framing:** the DVC carries *byte-identical* RDPSND PDUs — the `SNDPROLOG`
//! header is **kept**, not stripped — so we reuse `ironrdp_rdpsnd`'s
//! `ServerAudioOutputPdu` / `ClientAudioOutputPdu` codecs verbatim; only the channel
//! envelope (DVC vs the static SVC) differs.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use ironrdp_core::{Encode, EncodeResult, WriteCursor, decode, impl_as_any};
use ironrdp_dvc::{DvcEncode, DvcMessage, DvcProcessor, DvcServerProcessor};
use ironrdp_pdu::{PduResult, decode_err};
use ironrdp_rdpsnd::pdu::{
    AudioFormat, ClientAudioOutputPdu, ServerAudioFormatPdu, ServerAudioOutputPdu, TrainingPdu, Version, Wave2Pdu,
    WaveFormat,
};
use tracing::{debug, warn};

/// Channel name for the lossy (unreliable-UDP) audio output DVC (MS-RDPEA §2.1).
pub const AUDIO_PLAYBACK_LOSSY_DVC: &str = "AUDIO_PLAYBACK_LOSSY_DVC";
/// Channel name for the reliable audio output DVC (MS-RDPEA §2.1).
pub const AUDIO_PLAYBACK_DVC: &str = "AUDIO_PLAYBACK_DVC";

/// The client-list index of the audio format negotiated on the RELIABLE
/// `AUDIO_PLAYBACK_DVC`, shared with the lossy wave path so it can stamp the
/// `Wave2` PDUs it ships over the lossy/DTLS tunnel with the right `wFormatNo`
/// (MS-RDPEA: the lossy channel inherits the reliable channel's format
/// negotiation — it never negotiates its own). `u32::MAX` = not yet negotiated.
#[derive(Clone)]
pub struct NegotiatedAudioFormat(Arc<AtomicU32>);

impl NegotiatedAudioFormat {
    pub fn new() -> Self {
        Self(Arc::new(AtomicU32::new(u32::MAX)))
    }

    /// Publish the negotiated client-list format index (called by the reliable
    /// handler once the client confirms training).
    pub fn set(&self, format_no: u16) {
        self.0.store(u32::from(format_no), Ordering::Relaxed);
    }

    /// The negotiated client-list format index, or `None` until the reliable
    /// handshake completes. Read by the lossy wave path (2b-iv-B, not yet wired).
    #[allow(dead_code)]
    pub fn get(&self) -> Option<u16> {
        match self.0.load(Ordering::Relaxed) {
            u32::MAX => None,
            v => Some(v as u16),
        }
    }
}

impl Default for NegotiatedAudioFormat {
    fn default() -> Self {
        Self::new()
    }
}

/// An owned RDPSND server PDU (Format/Training — no borrowed wave data) wrapped as
/// a `DvcMessage`. `ironrdp_rdpsnd`'s PDUs implement `SvcEncode` (for the static
/// channel) but not `DvcEncode`; rather than orphan-impl, we hold the PDU and
/// delegate `Encode` (which already writes the full `SNDPROLOG` + body). The DVC
/// layer adds only its own framing around this payload.
struct OwnedAudioPdu(ServerAudioOutputPdu<'static>);

impl Encode for OwnedAudioPdu {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        self.0.encode(dst)
    }

    fn name(&self) -> &'static str {
        "RdpsndDvcPdu"
    }

    fn size(&self) -> usize {
        self.0.size()
    }
}

impl DvcEncode for OwnedAudioPdu {}

fn dvc_msg(pdu: ServerAudioOutputPdu<'static>) -> DvcMessage {
    Box::new(OwnedAudioPdu(pdu))
}

/// (2b-iv-B) Build one `Wave2` audio-data DVC message for the lossy
/// `AUDIO_PLAYBACK_LOSSY_DVC` channel: the same `Wave2Pdu` the static rdpsnd path
/// ships (`RdpsndServer::wave`), but wrapped as a `DvcMessage` so the server can
/// `encode_dvc_messages` it onto channel 5 and route it over the lossy/DTLS tunnel.
/// `format_no` is the client-list index the RELIABLE `AUDIO_PLAYBACK_DVC` handshake
/// negotiated (read from [`NegotiatedAudioFormat::get`]); `block_no` is a per-wave
/// sequence the caller advances. `data` is the codec payload (an AAC access unit for
/// the AAC path). MS-RDPEA v8 → `Wave2`.
pub fn lossy_wave_dvc_message(block_no: u8, audio_timestamp: u32, format_no: u16, data: Vec<u8>) -> DvcMessage {
    let pdu = Wave2Pdu {
        block_no,
        timestamp: 0,
        audio_timestamp,
        format_no,
        data: data.into(),
    };
    dvc_msg(ServerAudioOutputPdu::Wave2(pdu))
}

/// MS-RDPEA server handler for an audio output dynamic channel.
///
/// **Dual-channel topology (P2.4b):** mstsc tears the session down if it receives
/// Server Audio Formats on the `_LOSSY_`-named channel — over TCP *or* over the
/// migrated lossy tunnel (verified). So the format/quality/training handshake runs
/// on the RELIABLE `AUDIO_PLAYBACK_DVC` (over TCP) while `AUDIO_PLAYBACK_LOSSY_DVC`
/// carries only `Wave2` data, Soft-Synced onto the lossy/DTLS tunnel and stamped
/// with the format index the reliable channel negotiated. One handler type, two
/// roles, selected at construction.
pub struct AudioLossyDvc {
    /// Channel name: `AUDIO_PLAYBACK_DVC` (reliable) or `AUDIO_PLAYBACK_LOSSY_DVC`.
    channel_name: &'static str,
    /// When `true` (the lossy channel) `start()` sends NO formats — the channel is
    /// data-only and never negotiates (doing so tears mstsc down).
    defer_formats: bool,
    /// The server audio format list to advertise (PCM + AAC). Only the reliable
    /// channel sends these; passed in so it matches what the wave path encodes.
    formats: Vec<AudioFormat>,
    /// The client-list index of the format we'll send waves in, once negotiated.
    chosen_format_no: Option<u16>,
    /// Whether the client confirmed our Training PDU (handshake complete).
    training_confirmed: bool,
    /// Shared cell the RELIABLE handler publishes the negotiated format index to,
    /// for the lossy wave path to read (`None` on the lossy handler).
    negotiated: Option<NegotiatedAudioFormat>,
}

impl AudioLossyDvc {
    /// The RELIABLE `AUDIO_PLAYBACK_DVC`: runs the full MS-RDPEA handshake over its
    /// transport (TCP) and publishes the negotiated client-list format index to
    /// `negotiated` for the lossy wave path.
    pub fn reliable(formats: Vec<AudioFormat>, negotiated: NegotiatedAudioFormat) -> Self {
        Self {
            channel_name: AUDIO_PLAYBACK_DVC,
            defer_formats: false,
            formats,
            chosen_format_no: None,
            training_confirmed: false,
            negotiated: Some(negotiated),
        }
    }

    /// The LOSSY `AUDIO_PLAYBACK_LOSSY_DVC`: data-only (Wave2 over the lossy/DTLS
    /// tunnel). Never sends formats — mstsc tears the socket down if it does.
    pub fn lossy() -> Self {
        Self {
            channel_name: AUDIO_PLAYBACK_LOSSY_DVC,
            defer_formats: true,
            formats: Vec::new(),
            chosen_format_no: None,
            training_confirmed: false,
            negotiated: None,
        }
    }
}

impl_as_any!(AudioLossyDvc);

impl DvcProcessor for AudioLossyDvc {
    fn channel_name(&self) -> &str {
        self.channel_name
    }

    fn start(&mut self, _channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        // The lossy-named DVC must NOT receive format data — mstsc tears down the
        // whole TCP socket if it does, over TCP *or* over the migrated tunnel
        // (verified). It's data-only; the handshake runs on the reliable
        // AUDIO_PLAYBACK_DVC instead, and this channel inherits its format index.
        if self.defer_formats {
            debug!(
                channel = self.channel_name,
                "lossy audio DVC opened — data-only (no format handshake; it runs on the reliable AUDIO_PLAYBACK_DVC)"
            );
            return Ok(Vec::new());
        }
        debug!(
            channel = self.channel_name,
            formats = self.formats.len(),
            "audio output DVC opened — sending Server Audio Formats (v8)"
        );
        let pdu = ServerAudioOutputPdu::AudioFormat(ServerAudioFormatPdu {
            version: Version::V8,
            formats: self.formats.clone(),
        });
        Ok(vec![dvc_msg(pdu)])
    }

    fn process(&mut self, _channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        match decode::<ClientAudioOutputPdu>(payload).map_err(|e| decode_err!(e))? {
            ClientAudioOutputPdu::AudioFormat(cf) => {
                // Per MS-RDPEA the client sends a Quality Mode PDU immediately
                // after Client Audio Formats (v6+), and the server sends Training
                // only after that — so here we just record the chosen format and
                // WAIT for Quality Mode before replying with Training. Prefer AAC
                // (the lossy DVC requires it); else fall back to the first format
                // the client accepted. wFormatNo indexes the CLIENT list.
                let chosen = cf
                    .formats
                    .iter()
                    .position(|f| f.format == WaveFormat::AAC_MS)
                    .or_else(|| (!cf.formats.is_empty()).then_some(0));
                match chosen {
                    Some(idx) => {
                        let is_aac = cf.formats[idx].format == WaveFormat::AAC_MS;
                        self.chosen_format_no = Some(u16::try_from(idx).unwrap_or(0));
                        debug!(
                            channel = self.channel_name,
                            client_formats = cf.formats.len(),
                            chosen_wformatno = idx,
                            aac = is_aac,
                            "P2.4b: audio DVC client formats received — awaiting Quality Mode"
                        );
                    }
                    None => warn!(
                        channel = self.channel_name,
                        "audio DVC: client accepted none of the server formats"
                    ),
                }
                Ok(Vec::new())
            }
            ClientAudioOutputPdu::QualityMode(q) => {
                debug!(channel = self.channel_name, quality = ?q.quality_mode, "audio DVC quality mode — sending Training");
                // Training PDU: a fixed timestamp + a 1 KiB training block (the
                // client echoes its size in the Training Confirm).
                let training = ServerAudioOutputPdu::Training(TrainingPdu {
                    timestamp: 0,
                    data: vec![0u8; 1024],
                });
                Ok(vec![dvc_msg(training)])
            }
            ClientAudioOutputPdu::TrainingConfirm(_) => {
                self.training_confirmed = true;
                // Publish the negotiated client-list format index so the lossy wave
                // path can stamp Wave2 PDUs with it (the lossy channel inherits this
                // reliable channel's negotiation — it never negotiates its own).
                if let (Some(shared), Some(idx)) = (self.negotiated.as_ref(), self.chosen_format_no) {
                    shared.set(idx);
                }
                debug!(
                    channel = self.channel_name,
                    chosen_wformatno = ?self.chosen_format_no,
                    "P2.4b GREEN: reliable audio DVC negotiated + training confirmed over TCP — \
                     lossy wave path can now stream Wave2 over the tunnel with this format index"
                );
                Ok(Vec::new())
            }
            ClientAudioOutputPdu::WaveConfirm(_) => Ok(Vec::new()),
        }
    }
}

impl DvcServerProcessor for AudioLossyDvc {}
