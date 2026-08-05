//! Forward macOS system audio to the RDP client via the RDPSND SVC.
//!
//! We tap the same display ScreenCaptureKit gives us video for, but with a
//! second SCStream configured for audio output (`captures_audio = true`,
//! `SCStreamOutputType::Audio`). SCK delivers 32-bit float PCM at the
//! configured sample rate; we convert to 16-bit signed PCM interleaved and
//! ship via `RdpsndServerMessage::Wave`.

use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};

use ironrdp_rdpsnd::pdu::{AudioFormat, WaveFormat};
use ironrdp_rdpsnd::server::{NegotiatedFormat, RdpsndError, RdpsndServerHandler};
// Only the macOS capture path ships waves via the unified ServerEvent fallback;
// on the Linux cross-compile stub this name is unused.
#[cfg(target_os = "macos")]
use ironrdp_rdpsnd::server::RdpsndServerMessage;
use ironrdp_server::{AudioWave, ServerEvent, ServerEventSender, SoundServerFactory};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

// ScreenCaptureKit only honors 8000/16000/24000/48000 Hz, so we capture at
// 48 kHz. The advertised RDPSND format, however, is 44.1 kHz: Windows audio
// endpoints are commonly 44.1 native, and feeding clients at that rate lets
// them play directly without internal resampling, which is what was causing
// the ~20% over-feed / drift on mstsc. The capture loop resamples 48 -> 44.1
// before handing PCM to the client.
const SCK_SAMPLE_RATE: u32 = 48000;
const SAMPLE_RATE: u32 = 44100;
const CHANNELS: u16 = 2;
const BITS_PER_SAMPLE: u16 = 16;

type Sender = Arc<Mutex<Option<mpsc::UnboundedSender<ServerEvent>>>>;
type AudioSender = Arc<Mutex<Option<mpsc::Sender<AudioWave>>>>;

#[derive(Debug)]
pub struct MacRdpsnd {
    sender: Sender,
    /// Dedicated bounded channel for Wave PDUs. Set by ironrdp-server
    /// via `set_audio_sender`. When present, the capture loop sends
    /// every Wave directly here, bypassing the unified `ServerEvent`
    /// stream. The server's `dispatch_audio` task is the sole consumer,
    /// independent of the inbound-PDU and outbound-event dispatch
    /// branches that share `Mutex<Self>` — so a sustained inbound
    /// cliprdr stream (e.g., large `--lazy-paste` Windows→Mac transfer)
    /// no longer starves audio output.
    audio_sender: AudioSender,
    // Monotonic capture-loop generation, shared with every backend this
    // factory builds. mstsc's cert-prompt reconnect makes ironrdp build a
    // second backend (and thus a second capture loop) while the first may
    // still be alive; both would feed the shared `sender` and the client
    // would receive ~2x the audio. Each `start()` claims a new generation;
    // older capture loops observe the bump and exit, so at most one runs.
    generation: Arc<AtomicU64>,
    /// Shared "client minimized / SuppressOutput" flag, same Arc the
    /// vendor server flips and capture.rs reads. `None` disables the
    /// audio mute (e.g., test/Linux-stub builds without server plumbing).
    display_suppressed: Option<Arc<AtomicBool>>,
    /// When true (default) and `display_suppressed` is set, the capture
    /// loop stops emitting Wave PDUs while the client is minimized so
    /// the client's audio renderer drains naturally. Pass
    /// `--no-mute-on-minimize` on the CLI to flip this off.
    mute_on_minimize: bool,
    /// When true (`--enable-aac`), advertise AAC-LC ahead of PCM so clients
    /// that decode it negotiate compressed audio (~11x smaller than PCM).
    /// PCM stays in the list as the automatic fallback.
    enable_aac: bool,
    /// Target AAC bitrate in bits/sec (`--aac-bitrate`, default 128_000).
    /// Used both to size the advertised format and to configure the encoder.
    aac_bitrate: u32,
    /// Display the audio SCStream binds its content filter to — the SAME
    /// display the video path captures. `Some(id)` for a virtual display,
    /// `None` for the primary panel. Critical for `--detach-primary` /
    /// `--capture-primary`: binding to a physical display that those modes
    /// then disable/capture kills the audio stream's content source. The
    /// virtual display survives both, so audio must follow it, not
    /// `displays.first()` (which is the physical primary).
    target_display_id: Option<u32>,
}

impl MacRdpsnd {
    pub fn new(
        display_suppressed: Option<Arc<AtomicBool>>,
        mute_on_minimize: bool,
        enable_aac: bool,
        aac_bitrate: u32,
        target_display_id: Option<u32>,
    ) -> Self {
        Self {
            sender: Arc::new(Mutex::new(None)),
            audio_sender: Arc::new(Mutex::new(None)),
            generation: Arc::new(AtomicU64::new(0)),
            display_suppressed,
            mute_on_minimize,
            enable_aac,
            aac_bitrate,
            target_display_id,
        }
    }
}

impl ServerEventSender for MacRdpsnd {
    fn set_sender(&mut self, sender: mpsc::UnboundedSender<ServerEvent>) {
        *self.sender.lock().unwrap() = Some(sender);
    }
}

impl SoundServerFactory for MacRdpsnd {
    fn build_backend(&self) -> Box<dyn RdpsndServerHandler> {
        // AAC first so the negotiation in `start()` (first server format the
        // client also accepts) prefers it; PCM stays as the fallback for
        // clients without AAC decode.
        let formats = server_audio_formats(self.enable_aac, self.aac_bitrate);
        Box::new(MacRdpsndBackend {
            sender: self.sender.clone(),
            audio_sender: self.audio_sender.clone(),
            generation: self.generation.clone(),
            my_gen: 0,
            formats,
            display_suppressed: self.display_suppressed.clone(),
            mute_on_minimize: self.mute_on_minimize,
            aac_bitrate: self.aac_bitrate,
            target_display_id: self.target_display_id,
        })
    }

    fn set_audio_sender(&mut self, audio_sender: mpsc::Sender<AudioWave>) {
        *self.audio_sender.lock().unwrap() = Some(audio_sender);
    }
}

/// The server audio format list, ordered by our preference (AAC ahead of PCM
/// when `enable_aac`), exactly as `build_backend` advertises on the static
/// RDPSND channel. Exposed so the UDP-multitransport lossy audio DVC
/// (`AUDIO_PLAYBACK_LOSSY_DVC`) can advertise the *same* list it will encode in.
pub fn server_audio_formats(enable_aac: bool, aac_bitrate: u32) -> Vec<AudioFormat> {
    if enable_aac {
        vec![aac_format(aac_bitrate), pcm_format()]
    } else {
        vec![pcm_format()]
    }
}

fn pcm_format() -> AudioFormat {
    let block_align = (CHANNELS as u32) * (BITS_PER_SAMPLE as u32 / 8);
    AudioFormat {
        format: WaveFormat::PCM,
        n_channels: CHANNELS,
        n_samples_per_sec: SAMPLE_RATE,
        n_avg_bytes_per_sec: SAMPLE_RATE * block_align,
        n_block_align: block_align as u16,
        bits_per_sample: BITS_PER_SAMPLE,
        data: None,
    }
}

/// AAC-LC `AUDIO_FORMAT` for `WAVE_FORMAT_AAC_MS` (0xA106).
///
/// Mirrors what FreeRDP's server advertises: `cbSize = 0` (no HEAACWAVEINFO /
/// AudioSpecificConfig blob — `data: None`), `n_block_align = 4`,
/// `bits_per_sample = 16`, with `n_avg_bytes_per_sec` set to the target
/// bitrate/8 so the client sizes its buffers. The wire payload is raw AAC-LC
/// access units (see `src/aac.rs`). `bitrate` is bits/sec.
fn aac_format(bitrate: u32) -> AudioFormat {
    let block_align = (CHANNELS as u32) * (BITS_PER_SAMPLE as u32 / 8);
    AudioFormat {
        format: WaveFormat::AAC_MS,
        n_channels: CHANNELS,
        n_samples_per_sec: SAMPLE_RATE,
        n_avg_bytes_per_sec: bitrate / 8,
        n_block_align: block_align as u16,
        bits_per_sample: BITS_PER_SAMPLE,
        data: None,
    }
}

#[derive(Debug)]
struct MacRdpsndBackend {
    sender: Sender,
    audio_sender: AudioSender,
    generation: Arc<AtomicU64>,
    // Generation claimed by this backend's capture loop, 0 until `start()`.
    my_gen: u64,
    formats: Vec<AudioFormat>,
    /// Shared with the vendor server's SuppressOutput handler — see
    /// [`MacRdpsnd::display_suppressed`].
    display_suppressed: Option<Arc<AtomicBool>>,
    /// Default-on opt-out via `--no-mute-on-minimize`.
    mute_on_minimize: bool,
    /// Target AAC bitrate (bits/sec); only consulted when the negotiated
    /// format is `WAVE_FORMAT_AAC_MS`.
    aac_bitrate: u32,
    /// Display the audio SCStream binds to — see [`MacRdpsnd::target_display_id`].
    target_display_id: Option<u32>,
}

// Note: format negotiation (server-preference selection + the client-list
// `wFormatNo` index arithmetic that used to live in a local `choose_audio_format`)
// is now owned by ironrdp-rdpsnd (PR #1359): the crate hands `choose_format` the
// mutually-supported formats in our preference order, each carrying the correct
// `wFormatNo`, so the load-bearing "wFormatNo indexes the CLIENT's list" rule is
// enforced upstream. macrdp only picks the top entry (see `choose_format`).

impl RdpsndServerHandler for MacRdpsndBackend {
    fn get_formats(&self) -> &[AudioFormat] {
        &self.formats
    }

    fn choose_format<'a>(
        &mut self,
        common: &'a [NegotiatedFormat],
    ) -> Option<&'a NegotiatedFormat> {
        // `common` is the mutually-supported formats in OUR preference order (AAC
        // ahead of PCM — see `server_audio_formats`), each already carrying the
        // client-list `wFormatNo` the crate stamps onto every wave. So the top entry
        // is our preferred client-accepted format; the crate owns the index arithmetic
        // `choose_audio_format` used to. `common` is never empty here — the crate skips
        // this call when server and client share no format.
        common.first()
    }

    fn start(&mut self, format: &NegotiatedFormat) -> Result<(), Box<dyn RdpsndError>> {
        // `format` is the one `choose_format` just returned; the crate stamps its
        // wFormatNo onto every wave, so we only need to know whether to encode AAC or
        // ship PCM.
        let use_aac = format.format().format == WaveFormat::AAC_MS;
        debug!(
            use_aac,
            "rdpsnd audio streaming starting (format negotiated by the crate)"
        );

        // Claim a fresh generation. Any capture loop from a previous
        // connection sees the bump on its next iteration and exits, so it
        // never feeds the shared event channel alongside this one.
        self.my_gen = self.generation.fetch_add(1, Ordering::SeqCst) + 1;

        let sender = self.sender.clone();
        let audio_sender = self.audio_sender.clone();
        let generation = self.generation.clone();
        let my_gen = self.my_gen;
        let display_suppressed = self.display_suppressed.clone();
        let mute_on_minimize = self.mute_on_minimize;
        let aac_bitrate = self.aac_bitrate;
        let target_display_id = self.target_display_id;
        // Dedicated OS thread at USER_INTERACTIVE QoS for the entire
        // capture / resample / channel-send pipeline. Tokio workers ride
        // USER_INITIATED (see main.rs::boost_thread_qos) which a cargo
        // build can preempt for hundreds of ms; that starved capture and
        // produced multi-second audio gaps. A dedicated OS thread at the
        // highest QoS class keeps the SCK pump and rubato resample running
        // even under heavy local CPU load. One thread per connection;
        // the generation counter retires stale loops on reconnect.
        std::thread::Builder::new()
            .name(format!("macrdp-audio-{my_gen}"))
            .spawn(move || {
                #[cfg(target_os = "macos")]
                boost_audio_qos();
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        warn!("audio: failed to build dedicated runtime: {e}");
                        return;
                    }
                };
                rt.block_on(async move {
                    if let Err(e) = capture_loop(
                        sender,
                        audio_sender,
                        generation,
                        my_gen,
                        display_suppressed,
                        mute_on_minimize,
                        use_aac,
                        aac_bitrate,
                        target_display_id,
                    )
                    .await
                    {
                        warn!("audio capture loop ended: {e}");
                    }
                });
            })
            .expect("spawn audio capture thread");
        Ok(())
    }

    fn stop(&mut self) {
        // Retire our capture loop, but only if it is still the active one — a
        // newer connection may have already superseded us.
        let _ = self.generation.compare_exchange(
            self.my_gen,
            self.my_gen + 1,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
    }
}

/// Promote the calling thread to `QOS_CLASS_USER_INTERACTIVE` (0x21).
/// Reserved for the audio capture thread spawned in `MacRdpsndBackend::start`.
/// USER_INTERACTIVE is the same class WindowServer uses for its event loop,
/// so the scheduler keeps us runnable even when a parallel cargo build pins
/// every other thread; the audio pipeline then never starves long enough
/// for the vendor-server lag tracker to declare a writer stall.
#[cfg(target_os = "macos")]
fn boost_audio_qos() {
    use std::os::raw::{c_int, c_uint};
    const QOS_CLASS_USER_INTERACTIVE: c_uint = 0x21;
    unsafe extern "C" {
        fn pthread_set_qos_class_self_np(qos_class: c_uint, relative_priority: c_int) -> c_int;
    }
    unsafe {
        let _ = pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0);
    }
}

#[cfg(target_os = "macos")]
#[allow(clippy::too_many_arguments)]
async fn capture_loop(
    sender: Sender,
    audio_sender: AudioSender,
    generation: Arc<AtomicU64>,
    my_gen: u64,
    display_suppressed: Option<Arc<AtomicBool>>,
    mute_on_minimize: bool,
    use_aac: bool,
    aac_bitrate: u32,
    target_display_id: Option<u32>,
) -> anyhow::Result<()> {
    use anyhow::{anyhow, Context};
    use rubato::Resampler;
    use screencapturekit::async_api::{AsyncSCShareableContent, AsyncSCStream};
    use screencapturekit::prelude::{SCContentFilter, SCStreamConfiguration, SCStreamOutputType};

    let content = AsyncSCShareableContent::get()
        .await
        .map_err(|e| anyhow!("SCShareableContent for audio: {e:?}"))?;
    let displays = content.displays();
    // Bind to the SAME display the video path captures. With --detach-primary /
    // --capture-primary the physical primary is disabled/captured once a client
    // connects, so binding the audio stream to it (the old `displays.first()`)
    // killed the stream's content source — audio cut out, or the restart loop
    // thrashed it in and out. The virtual display survives both, so follow it.
    // Falls back to the first display if the id isn't enumerated (transient).
    let display = match target_display_id {
        Some(id) => displays
            .iter()
            .find(|d| d.display_id() == id)
            .or_else(|| {
                warn!(
                    target_id = id,
                    "audio: no SCK display with that id; using the first display"
                );
                displays.first()
            })
            .context("no displays for audio capture")?,
        None => displays.first().context("no displays for audio capture")?,
    };
    let bound_display_id = display.display_id();
    tracing::debug!(
        bound_display_id,
        requested = ?target_display_id,
        "audio: SCStream content filter bound to display"
    );

    let filter = SCContentFilter::create()
        .with_display(display)
        .with_excluding_windows(&[])
        .build();

    let config = SCStreamConfiguration::new()
        .with_captures_audio(true)
        .with_sample_rate(SCK_SAMPLE_RATE as i32)
        .with_channel_count(CHANNELS as i32);

    // SCK only delivers 8/16/24/48 kHz. We capture at 48 and resample to the
    // advertised SAMPLE_RATE (44.1 kHz) so the client plays at its native rate
    // without internal resampling. Chunk size matches a typical SCK audio
    // buffer (~21 ms at 48 kHz) — input is buffered until we have a full
    // chunk, then fed to the resampler.
    const RESAMPLE_CHUNK: usize = 1024;
    let mut resampler = rubato::FftFixedIn::<f32>::new(
        SCK_SAMPLE_RATE as usize,
        SAMPLE_RATE as usize,
        RESAMPLE_CHUNK,
        1,
        CHANNELS as usize,
    )
    .map_err(|e| anyhow!("rubato resampler init: {e}"))?;
    let mut input_buf: [Vec<f32>; 2] = [
        Vec::with_capacity(RESAMPLE_CHUNK * 2),
        Vec::with_capacity(RESAMPLE_CHUNK * 2),
    ];
    // Reused per resampler invocation so the hot path doesn't allocate two
    // Vecs per chunk (`drain(..N).collect()` did at ~46 chunks/sec).
    let mut chunk: [Vec<f32>; 2] = [vec![0.0; RESAMPLE_CHUNK], vec![0.0; RESAMPLE_CHUNK]];

    // Sender is lazily resolved on first emit, then cached. Resolving at the
    // top of capture_loop instead breaks Microsoft Remote Desktop for Mac:
    // that client appears to call `set_sender` *after* `start()`, so an
    // up-front grab returns None and exits silently. The old per-emit lock
    // worked because SCK's first sample takes ~21 ms to arrive, leaving a
    // window for set_sender to populate the Mutex. Lazy-resolve here gives
    // the same robustness without paying for a lock on every wave.
    let mut s: Option<mpsc::UnboundedSender<ServerEvent>> = None;
    let mut audio_s: Option<mpsc::Sender<AudioWave>> = None;

    // AAC encoder, present only when the client negotiated WAVE_FORMAT_AAC_MS
    // (`--enable-aac` and the client advertised AAC decode). Built up front so
    // a failure surfaces immediately rather than mid-stream; if the encoder
    // can't be created we end the loop (no audio) instead of shipping raw PCM
    // bytes the client would try to decode as AAC.
    let mut aac_encoder = if use_aac {
        Some(
            crate::aac::AacEncoder::new(SAMPLE_RATE, CHANNELS, aac_bitrate)
                .context("init AAC encoder")?,
        )
    } else {
        None
    };
    if use_aac {
        info!(bitrate = aac_bitrate, "AAC-LC audio encoding enabled");
    }

    // Shallow queue: SCK's async buffer is a drop-oldest ring of this depth.
    // Each slot is ~20 ms of audio, so 2 caps capture-side staleness at ~40 ms
    // while leaving one slot of headroom against scheduler jitter. Lower would
    // trade dropouts for marginal latency; the real backlog is downstream.
    // Restart bookkeeping. Over a long session ScreenCaptureKit can stop
    // delivering audio samples (the async stream yields `None`) or transiently
    // fail to (re)start; without recovery that left the connection permanently
    // silent while video — a separate SCStream — kept running. We rebuild the
    // stream with capped exponential backoff instead of exiting. `my_gen` is
    // unchanged across restarts, so the generation guard still retires this
    // loop on reconnect and there is no double-capture risk.
    const RESTART_BACKOFF_BASE_MS: u64 = 250;
    const RESTART_BACKOFF_MAX_MS: u64 = 5000;
    let mut consecutive_failures: u32 = 0;
    // Capped exponential backoff for capture (re)start failures (250ms→5s).
    let backoff_ms = |failures: u32| {
        RESTART_BACKOFF_MAX_MS.min(RESTART_BACKOFF_BASE_MS << failures.saturating_sub(1).min(5))
    };

    let start_instant = std::time::Instant::now();
    // Set false at the top of every (re)connect so SCK's delivered format is
    // re-logged after a restart; the first read is always dominated by that
    // assignment.
    let mut format_logged: bool;

    // Per-reader debounce for the mute-on-minimize gate (mirrors
    // `capture.rs`'s `suppressed_since`). `None` while the shared
    // SuppressOutput flag is `false`; set to `Some(Instant::now())` the
    // first chunk we read `true`. The gate only engages once that's
    // been stable for `SUPPRESS_DEBOUNCE` (1 s) so brief flaps under
    // heavy CPU/IO (e.g., mstsc backing off to drain backlog during a
    // cargo build) don't oscillate the mute and cause stutter.
    const SUPPRESS_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(1);
    let mut suppressed_since: Option<std::time::Instant> = None;

    // SCK delivery-rate measurement. The vendor server's audio-lag model
    // assumes one Wave = `WAVE_MS = 1024/48 = 21.33 ms` of real audio,
    // which is only true if SCK actually delivers at 48 kHz. Some
    // configurations (system audio rate ≠ requested capture rate) cause
    // SCK to deliver samples at a different effective rate while still
    // *reporting* 48 kHz in the format description, which manifests as
    // a steady-state audio-shipped-vs-wall-clock deficit that the resync
    // mechanism papers over every few seconds. Logging measured arrival
    // rate over a wall-clock window tells us empirically whether that's
    // the bug. Enable with `RUST_LOG=macrdp::audio=debug`.
    let mut samples_received: u64 = 0;
    let mut last_rate_log = std::time::Instant::now();
    // Output-side counters. If output samples don't match 44.1 kHz × elapsed
    // (after warm-up), the resampler is producing less audio than it should.
    // If output samples match but wave count is below 46.875/sec, average
    // wave size is bigger than the server's hardcoded WAVE_MS expects.
    let mut output_samples: u64 = 0;
    let mut waves_emitted: u64 = 0;
    // Encoded bytes actually shipped per window. Logged as `wire_kbps` so the
    // compression is directly visible: raw PCM sits at ~1411 kbps, AAC-LC at
    // roughly `--aac-bitrate` (~128). The clearest in-log proof `--enable-aac`
    // is doing what it claims.
    let mut bytes_emitted: u64 = 0;

    'reconnect: loop {
        if generation.load(Ordering::SeqCst) != my_gen {
            debug!(my_gen, "audio capture loop superseded; exiting");
            break 'reconnect;
        }

        // (Re)build the audio SCStream. On a start_capture error, back off and
        // retry rather than ending the loop — a transient failure mid-session
        // must not silence the rest of the connection.
        let stream = AsyncSCStream::new(&filter, &config, 2, SCStreamOutputType::Audio);
        if let Err(e) = stream.start_capture() {
            consecutive_failures = consecutive_failures.saturating_add(1);
            let backoff = backoff_ms(consecutive_failures);
            warn!(
                attempt = consecutive_failures,
                backoff_ms = backoff,
                "audio start_capture failed; retrying: {e:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(backoff)).await;
            continue 'reconnect;
        }
        debug!(my_gen, "audio capture started");
        // A fresh format line per (re)start doubles as a "stream healthy again"
        // marker in the log.
        format_logged = false;
        // Drop any partial pre-gap chunk so we don't stitch samples across the
        // restart discontinuity.
        input_buf[0].clear();
        input_buf[1].clear();

        loop {
            if generation.load(Ordering::SeqCst) != my_gen {
                debug!(my_gen, "audio capture loop superseded; exiting");
                let _ = stream.stop_capture();
                break 'reconnect;
            }
            // Manual A/V resync hotkey (Ctrl+Alt+Shift+R, set in input.rs via
            // crate::RESYNC_AUDIO): rebuild the SCK stream now. The brief capture
            // gap lets the client's downstream audio backlog (audiodg) drain and
            // re-baselines the server-side wave timing — the same effect a
            // minimize→unminimize achieves. This is a deliberate resync, not a
            // failure, so reset the backoff and rebuild immediately.
            if crate::RESYNC_AUDIO.swap(false, Ordering::Relaxed) {
                let _ = stream.stop_capture();
                consecutive_failures = 0;
                info!(
                    my_gen,
                    "manual A/V resync (Ctrl+Alt+Shift+R): rebuilding audio capture stream"
                );
                continue 'reconnect;
            }
            let Some(sample) = stream.next().await else {
                // SCK stopped delivering. Stop, back off, and rebuild rather
                // than ending the loop (which would silence audio for the rest
                // of the session).
                let _ = stream.stop_capture();
                consecutive_failures = consecutive_failures.saturating_add(1);
                let backoff = backoff_ms(consecutive_failures);
                warn!(
                    attempt = consecutive_failures,
                    backoff_ms = backoff,
                    "audio SCK stream ended; restarting capture"
                );
                tokio::time::sleep(std::time::Duration::from_millis(backoff)).await;
                continue 'reconnect;
            };
            // Delivered a sample → the stream is healthy; reset the backoff.
            consecutive_failures = 0;

            // Log the format SCK actually delivers, once per session. SCK does not
            // always honor the requested rate/channels; a mismatch against the
            // advertised RDPSND format makes the client play at the wrong frame
            // rate — audible as drift and lowered pitch.
            if !format_logged {
                format_logged = true;
                if let Some(fd) = sample.format_description() {
                    let rate = fd.audio_sample_rate().unwrap_or(0.0);
                    let channels = fd.audio_channel_count().unwrap_or(0);
                    info!(rate, channels, "SCK audio format");
                    if rate != 0.0 && (rate - f64::from(SCK_SAMPLE_RATE)).abs() > 1.0 {
                        warn!(
                            delivered = rate,
                            requested = SCK_SAMPLE_RATE,
                            "SCK audio rate differs from what we requested; \
                         the 48->44.1 resampler assumes 48 kHz input"
                        );
                    }
                }
            }

            let Some(list) = sample.audio_buffer_list() else {
                continue;
            };

            // SCK delivers float32 PCM as planar (one buffer per channel) or a
            // single interleaved buffer. Normalize to planar stereo f32 at the
            // SCK rate, accumulate into the resampler input buffer, then emit
            // resampled chunks as interleaved 16-bit stereo at SAMPLE_RATE.
            let (in_left, in_right) = float_list_to_planar_f32_stereo(&list);
            if in_left.is_empty() {
                continue;
            }
            input_buf[0].extend_from_slice(&in_left);
            input_buf[1].extend_from_slice(&in_right);

            // Track actual delivery rate. One sample = one frame (per channel),
            // so left-channel count is what we compare against SCK_SAMPLE_RATE.
            samples_received += in_left.len() as u64;
            if last_rate_log.elapsed() >= std::time::Duration::from_secs(5) {
                let elapsed = last_rate_log.elapsed().as_secs_f64();
                let measured_in = samples_received as f64 / elapsed;
                let measured_out = output_samples as f64 / elapsed;
                let measured_waves = waves_emitted as f64 / elapsed;
                let expected_in = f64::from(SCK_SAMPLE_RATE);
                let expected_out = f64::from(SAMPLE_RATE);
                // PCM emits one wave per resampled chunk (RESAMPLE_CHUNK input
                // samples at the SCK rate → ~46.875/s). AAC emits one wave per
                // 1024-frame access unit at the *output* rate → ~43.07/s. Pick the
                // right baseline so `waves_per_sec` vs `waves_expected` is a real
                // health check for the active codec, not always the PCM number.
                let expected_waves = if use_aac {
                    expected_out / crate::aac::FRAMES_PER_PACKET as f64
                } else {
                    expected_in / RESAMPLE_CHUNK as f64
                };
                let avg_wave_samples = if waves_emitted > 0 {
                    output_samples as f64 / waves_emitted as f64
                } else {
                    0.0
                };
                // Server's vendored audio-lag model assumes each wave is exactly
                // 1024/48 = 21.33 ms. If avg_wave_samples / SAMPLE_RATE * 1000
                // differs from that, the model drifts.
                let avg_wave_ms = if waves_emitted > 0 {
                    avg_wave_samples / f64::from(SAMPLE_RATE) * 1000.0
                } else {
                    0.0
                };
                let wire_kbps = bytes_emitted as f64 * 8.0 / elapsed / 1000.0;
                debug!(
                    in_rate = measured_in,
                    in_expected = expected_in,
                    out_rate = measured_out,
                    out_expected = expected_out,
                    waves_per_sec = measured_waves,
                    waves_expected = expected_waves,
                    avg_wave_samples,
                    avg_wave_ms,
                    wire_kbps,
                    codec = if use_aac { "aac" } else { "pcm" },
                    model_wave_ms = 1024.0 / 48.0,
                    "audio capture rates"
                );
                samples_received = 0;
                output_samples = 0;
                waves_emitted = 0;
                bytes_emitted = 0;
                last_rate_log = std::time::Instant::now();
            }

            while input_buf[0].len() >= RESAMPLE_CHUNK && input_buf[1].len() >= RESAMPLE_CHUNK {
                // Mute-on-minimize gate (default-on; opt out with
                // `--no-mute-on-minimize`). When the client has sent
                // `SuppressOutput { None }` (mstsc is minimized), drop incoming
                // audio at the source — mstsc's audio renderer drains naturally,
                // and on refocus fresh waves resume in sync with the freshly
                // IDR'd video. Without this, audio kept flowing during minimize
                // accumulates in audiodg.exe's buffer (invisible to the server)
                // and plays out late on refocus, leaving audio drifted by however
                // long was spent minimized. We drain the input buffer here so it
                // doesn't grow unbounded across a long minimize; the cheap drain
                // also avoids the rubato resample + channel send below.
                //
                // **Debounced** (see `SUPPRESS_DEBOUNCE` and the matching gate
                // in `capture.rs`): mstsc emits transient
                // `SuppressOutput`→`RefreshRectangle` pairs under wire pressure
                // (tens of ms each), and reacting to them thrashes the mute and
                // causes audible stutter. Only honor the flag after it's been
                // steady-`true` for >= 1 s.
                if mute_on_minimize {
                    if let Some(flag) = display_suppressed.as_ref() {
                        if flag.load(Ordering::Relaxed) {
                            let started =
                                *suppressed_since.get_or_insert_with(std::time::Instant::now);
                            if started.elapsed() >= SUPPRESS_DEBOUNCE {
                                input_buf[0].drain(..RESAMPLE_CHUNK);
                                input_buf[1].drain(..RESAMPLE_CHUNK);
                                continue;
                            }
                        } else {
                            suppressed_since = None;
                        }
                    }
                }

                chunk[0].copy_from_slice(&input_buf[0][..RESAMPLE_CHUNK]);
                chunk[1].copy_from_slice(&input_buf[1][..RESAMPLE_CHUNK]);
                let resampled = resampler
                    .process(&chunk, None)
                    .map_err(|e| anyhow!("rubato resample: {e}"))?;
                input_buf[0].drain(..RESAMPLE_CHUNK);
                input_buf[1].drain(..RESAMPLE_CHUNK);
                // Per-call output sample count (one channel; planar so all
                // channels have the same len). Counted before pcm.is_empty so
                // we capture empties too.
                output_samples += resampled[0].len() as u64;
                let pcm = planar_f32_to_interleaved_i16(&resampled);
                if pcm.is_empty() {
                    continue;
                }

                // The wave(s) to ship for this resampled chunk. PCM emits exactly
                // one wave with the bytes as-is (`None` duration → the dispatcher
                // derives playback time from byte length). AAC feeds the chunk to
                // the encoder, which returns zero or more raw access units (it
                // buffers internally to 1024-frame packets, with a one-time
                // priming delay at stream start); each AU carries an explicit
                // duration so the vendored audio-lag model gets a real playback
                // time instead of dividing the compressed byte count by 176.4.
                let waves: Vec<(Vec<u8>, Option<f64>)> = if let Some(enc) = aac_encoder.as_mut() {
                    let dur = crate::aac::packet_duration_ms(enc.sample_rate());
                    match enc.encode(&pcm) {
                        Ok(aus) => aus.into_iter().map(|au| (au, Some(dur))).collect(),
                        Err(e) => {
                            warn!("AAC encode failed: {e}");
                            Vec::new()
                        }
                    }
                } else {
                    vec![(pcm, None)]
                };

                for (data, duration_ms) in waves {
                    waves_emitted += 1;
                    bytes_emitted += data.len() as u64;
                    let ts_ms = start_instant.elapsed().as_millis() as u32;

                    // Lazy resolve both senders on first need. If set_sender /
                    // set_audio_sender haven't populated their Mutexes yet, retry
                    // next iteration — at 48 kHz / 1024 samples this is ~21 ms
                    // later.
                    if audio_s.is_none() {
                        audio_s = audio_sender.lock().unwrap().clone();
                    }
                    if let Some(audio_ref) = audio_s.as_ref() {
                        // Bounded channel: send().await applies backpressure if
                        // the dispatch_audio task is behind, but if we're at
                        // capacity we'd rather drop this wave at the SCK level
                        // than block the capture loop (which would back up the
                        // SCK ring buffer and lose newer audio anyway). Use
                        // try_send: on Full, log+drop; on Closed, exit.
                        match audio_ref.try_send((data, ts_ms, duration_ms)) {
                            Ok(()) => {}
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                debug!("audio channel full; dropping wave (dispatch_audio behind)");
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => return Ok(()),
                        }
                    } else {
                        // Audio sender not wired (older server build / non-macrdp
                        // host). Fall back to the unified ServerEvent channel.
                        // Note: this path can't carry an explicit duration, so AAC
                        // over it would mistime — but our vendored server always
                        // wires the audio sender, so AAC never takes this branch.
                        if s.is_none() {
                            s = sender.lock().unwrap().clone();
                        }
                        let Some(s_ref) = s.as_ref() else { continue };
                        if s_ref
                            .send(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, ts_ms)))
                            .is_err()
                        {
                            return Ok(());
                        }
                    }
                }
            }
        }
    }

    debug!(my_gen, "audio capture loop exiting");
    Ok(())
}

#[cfg(not(target_os = "macos"))]
#[allow(clippy::too_many_arguments)]
async fn capture_loop(
    _sender: Sender,
    _audio_sender: AudioSender,
    _generation: Arc<AtomicU64>,
    _my_gen: u64,
    _display_suppressed: Option<Arc<AtomicBool>>,
    _mute_on_minimize: bool,
    _use_aac: bool,
    _aac_bitrate: u32,
    _target_display_id: Option<u32>,
) -> anyhow::Result<()> {
    Ok(())
}

/// Normalize an SCK audio buffer list to planar stereo `f32` at the SCK rate.
/// Handles both planar (one buffer per channel) and interleaved (single buffer
/// with `number_channels` channels) layouts; a mono source is duplicated. The
/// result feeds the rubato resampler.
#[cfg(target_os = "macos")]
fn float_list_to_planar_f32_stereo(
    list: &screencapturekit::cm::AudioBufferList,
) -> (Vec<f32>, Vec<f32>) {
    let Some(first) = list.get(0) else {
        return (Vec::new(), Vec::new());
    };

    if list.num_buffers() >= 2 {
        // Planar: one mono buffer per channel. Channel 0 -> L, channel 1 -> R.
        let left = bytes_to_f32(first.data());
        let right = list
            .get(1)
            .map(|b| bytes_to_f32(b.data()))
            .unwrap_or_else(|| left.clone());
        let n = left.len().min(right.len());
        return (left[..n].to_vec(), right[..n].to_vec());
    }

    // Single buffer: interleaved across `number_channels`, or mono.
    match first.number_channels {
        0 | 1 => {
            let mono = bytes_to_f32(first.data());
            (mono.clone(), mono)
        }
        n => deinterleave_first_two_channels(first.data(), n as usize),
    }
}

/// Pull the first two channels out of an interleaved float32 buffer.
#[cfg(target_os = "macos")]
fn deinterleave_first_two_channels(bytes: &[u8], channels: usize) -> (Vec<f32>, Vec<f32>) {
    let frame_bytes = channels * 4;
    if frame_bytes == 0 {
        return (Vec::new(), Vec::new());
    }
    let frames = bytes.len() / frame_bytes;
    let mut left = Vec::with_capacity(frames);
    let mut right = Vec::with_capacity(frames);
    for f in 0..frames {
        let base = f * frame_bytes;
        left.push(read_f32_le(&bytes[base..base + 4]));
        right.push(read_f32_le(&bytes[base + 4..base + 8]));
    }
    (left, right)
}

#[cfg(target_os = "macos")]
fn bytes_to_f32(bytes: &[u8]) -> Vec<f32> {
    bytes.chunks_exact(4).map(read_f32_le).collect()
}

#[cfg(target_os = "macos")]
fn read_f32_le(b: &[u8]) -> f32 {
    f32::from_bits(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Interleave two planar float32 channels into a 16-bit LE stereo PCM payload.
#[cfg(target_os = "macos")]
fn planar_f32_to_interleaved_i16(planar: &[Vec<f32>]) -> Vec<u8> {
    if planar.len() < 2 {
        return Vec::new();
    }
    let frames = planar[0].len().min(planar[1].len());
    let mut out = Vec::with_capacity(frames * 4);
    for (lf, rf) in planar[0].iter().zip(planar[1].iter()).take(frames) {
        out.extend_from_slice(&float_to_i16(*lf).to_le_bytes());
        out.extend_from_slice(&float_to_i16(*rf).to_le_bytes());
    }
    out
}

#[cfg(target_os = "macos")]
fn float_to_i16(v: f32) -> i16 {
    let clamped = v.clamp(-1.0, 1.0);
    (clamped * 32767.0).round() as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_formats_prefer_aac_ahead_of_pcm() {
        // The macrdp-side invariant `choose_format` (= `common.first()`) relies on:
        // when AAC is enabled we advertise it AHEAD of PCM, so the crate's
        // server-preference-ordered `common` list puts AAC first. (The client-list
        // `wFormatNo` arithmetic once tested here now lives in ironrdp-rdpsnd, #1359.)
        let with_aac = server_audio_formats(true, 128_000);
        assert_eq!(with_aac.len(), 2);
        assert_eq!(
            with_aac[0].format,
            WaveFormat::AAC_MS,
            "AAC is our top preference"
        );
        assert_eq!(with_aac[1].format, WaveFormat::PCM);

        let pcm_only = server_audio_formats(false, 128_000);
        assert_eq!(pcm_only.len(), 1);
        assert_eq!(pcm_only[0].format, WaveFormat::PCM);
    }
}
