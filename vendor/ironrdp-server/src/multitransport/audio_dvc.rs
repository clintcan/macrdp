//! (vendored, feature=multitransport, Phase 2 / P2.4b) The
//! `AUDIO_PLAYBACK_LOSSY_DVC` dynamic virtual channel — MS-RDPEA audio output over
//! a DVC, destined for the lossy UDP tunnel.
//!
//! MS-RDPEA §2.1: the audio output channel is named `AUDIO_PLAYBACK_DVC` over a
//! reliable transport and **`AUDIO_PLAYBACK_LOSSY_DVC` over an unreliable UDP
//! transport**. The server opens it (DRDYNVC Create Request), runs the normal
//! RDPSND format/training handshake over it (on the main TCP connection), and then
//! — via MS-RDPEDYC Soft-Sync — migrates it onto the lossy (`UDPFECL`) tunnel, over
//! which the Wave2 audio PDUs flow.
//!
//! **Gating (MS-RDPEA Appendix A note <2>):** a client uses the lossy DVC only when
//! all of (a) a lossy UDP transport is available, (b) both ends are protocol
//! version ≥ 8, and (c) AAC is the codec. So we advertise `Version::V8` and the
//! caller passes a format list that includes AAC (`WAVE_FORMAT_AAC_MS`).
//!
//! **STATUS: P2.4b-1 spike, PAUSED (verified on real mstsc 2026-06-27).** Registered
//! only when the application calls
//! [`RdpServer::set_multitransport_lossy_audio_formats`](crate::RdpServer::set_multitransport_lossy_audio_formats)
//! (macrdp gates that behind the experimental `MACRDP_UDP_LOSSY_AUDIO` env), so the
//! default build is byte-unchanged.
//!
//! **KEY FINDING — the EGFX "negotiate-on-TCP-then-Soft-Sync" pattern does NOT carry
//! to a *lossy*-named channel.** With the literal name `AUDIO_PLAYBACK_LOSSY_DVC`,
//! mstsc accepts the DVC Create but, on receiving Server Audio Formats over
//! TCP/DRDYNVC, goes silent and tears down the whole TCP connection (broken pipe a
//! few seconds later — it kills EGFX too, not just audio). The reliable name
//! `AUDIO_PLAYBACK_DVC` (diagnostic env `MACRDP_AUDIO_DVC_RELIABLE=1`) handshakes
//! perfectly over TCP (formats → client formats(AAC) → quality mode → training →
//! confirm), audio plays, EGFX stays healthy — proving the channel *name* is the
//! blocker, not the PDU/framing and not coexistence with static rdpsnd (dual
//! negotiation is fine). So the **lossy** DVC must be Soft-Synced onto the lossy
//! tunnel BEFORE any data, with the handshake over the tunnel — the opposite of EGFX
//! (which migrates to the RELIABLE tunnel *after* a TCP handshake). That, plus routing
//! the AAC waves over the tunnel, is deferred until the lossy data path (P2.2/P2.3) is
//! mature; the reliable-DVC path that works isn't worth landing on its own (a reliable
//! tunnel HOL-blocks under loss like TCP). See `docs/rdp-udp-multitransport-feasibility.md`
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

use ironrdp_core::{Encode, EncodeResult, WriteCursor, decode, impl_as_any};
use ironrdp_dvc::{DvcEncode, DvcMessage, DvcProcessor, DvcServerProcessor};
use ironrdp_pdu::{PduResult, decode_err};
use ironrdp_rdpsnd::pdu::{
    AudioFormat, ClientAudioOutputPdu, ServerAudioFormatPdu, ServerAudioOutputPdu, TrainingPdu, Version, WaveFormat,
};
use tracing::{debug, warn};

/// Channel name for the lossy (unreliable-UDP) audio output DVC (MS-RDPEA §2.1).
pub const AUDIO_PLAYBACK_LOSSY_DVC: &str = "AUDIO_PLAYBACK_LOSSY_DVC";
/// Channel name for the reliable audio output DVC (MS-RDPEA §2.1).
pub const AUDIO_PLAYBACK_DVC: &str = "AUDIO_PLAYBACK_DVC";

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

/// MS-RDPEA server handler for the audio output dynamic channel.
pub struct AudioLossyDvc {
    /// Channel name. Defaults to `AUDIO_PLAYBACK_LOSSY_DVC`; the diagnostic env
    /// `MACRDP_AUDIO_DVC_RELIABLE=1` switches it to the reliable
    /// `AUDIO_PLAYBACK_DVC` to test whether mstsc rejects the format handshake
    /// specifically on a *lossy*-named channel over TCP (vs. any audio DVC).
    channel_name: &'static str,
    /// The server audio format list to advertise (PCM + AAC). Passed in by the
    /// application so it matches exactly what the wave path will encode.
    formats: Vec<AudioFormat>,
    /// The client-list index of the format we'll send waves in, once negotiated.
    chosen_format_no: Option<u16>,
    /// Whether the client confirmed our Training PDU (handshake complete).
    training_confirmed: bool,
}

impl AudioLossyDvc {
    pub fn new(formats: Vec<AudioFormat>) -> Self {
        let channel_name = if std::env::var_os("MACRDP_AUDIO_DVC_RELIABLE").is_some() {
            AUDIO_PLAYBACK_DVC
        } else {
            AUDIO_PLAYBACK_LOSSY_DVC
        };
        Self {
            channel_name,
            formats,
            chosen_format_no: None,
            training_confirmed: false,
        }
    }
}

impl_as_any!(AudioLossyDvc);

impl DvcProcessor for AudioLossyDvc {
    fn channel_name(&self) -> &str {
        self.channel_name
    }

    fn start(&mut self, _channel_id: u32) -> PduResult<Vec<DvcMessage>> {
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
                        warn!(
                            channel = self.channel_name,
                            client_formats = cf.formats.len(),
                            chosen_wformatno = idx,
                            aac = is_aac,
                            "P2.4b: audio DVC client formats received — awaiting Quality Mode"
                        );
                    }
                    None => warn!(channel = self.channel_name, "audio DVC: client accepted none of the server formats"),
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
                warn!(
                    channel = self.channel_name,
                    chosen_wformatno = ?self.chosen_format_no,
                    "P2.4b GREEN: audio DVC negotiated + training confirmed over TCP — ready for Soft-Sync wave migration"
                );
                Ok(Vec::new())
            }
            ClientAudioOutputPdu::WaveConfirm(_) => Ok(Vec::new()),
        }
    }
}

impl DvcServerProcessor for AudioLossyDvc {}
