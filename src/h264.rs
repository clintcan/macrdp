//! H.264 / EGFX video pipeline (app side).
//!
//! Rewritten from scratch after `h264-attempt-1` (which negotiated EGFX but
//! never rendered correctly). The salvaged VideoToolbox encoder lives in
//! `src/videotoolbox.rs`; this module bridges it to upstream's
//! `GraphicsPipelineServer` via the vendored `GfxServerFactory` hooks.
//!
//! Flow — two decoupled threads (the "push model"):
//!
//!   Capture thread (`submit_bgra`, once per SCK frame):
//!     1. First call lazily creates the EGFX surface + VT encoder AND spawns the
//!        ship thread (not in `on_ready`, which holds the server mutex).
//!     2. Drop-to-latest throttle: if `submitted - shipped` ≥
//!        `--h264-frames-in-flight`, skip this capture. Bounds latency under
//!        load without relying on frame acks (clients commonly suspend them).
//!        Skipping a capture *before* encode doesn't break the reference chain.
//!     3. Otherwise `Encoder::encode_bgra` submits to VideoToolbox (async) and
//!        returns immediately — the capture thread never blocks on the encoder,
//!        so it keeps pace with ScreenCaptureKit instead of falling behind under
//!        heavy frames (which would queue stale frames → growing latency).
//!
//!   Ship thread (`ship_loop`):
//!     4. Blocks on VT's output channel; for each encoded frame, frames it (see
//!        `WireFormat`), hands it to `GraphicsPipelineServer::send_avc420_frame`
//!        (StartFrame / WireToSurface1 / EndFrame), then ships the resulting
//!        `DvcMessage`s through DRDYNVC via `ServerEvent::Egfx(SendMessages)`,
//!        and bumps `shipped`.
//!
//!   The EGFX send window is `u32::MAX` (see `GfxHandler::max_frames_in_flight`)
//!   so `send_avc420_frame` NEVER drops an encoded frame — dropping one (a
//!   P-frame, or worse a keyframe) breaks the H.264 reference chain and causes
//!   client-side artifacts. All throttling is the capture-side drop above.
//!
//!   (Earlier this was single-threaded with a blocking `drain_wait`; that
//!   serialized capture with encode and fell behind under load. See memory
//!   h264-latency-tuning.)
//!
//! ## The bitstream-format question (see memory: avc420-bitstream-format-trap)
//!
//! VideoToolbox emits **AVCC** (4-byte big-endian length-prefixed NALs), with
//! SPS/PPS out-of-band. The AVC420 wire payload can be either AVCC
//! (length-prefixed) or Annex-B (start codes). ironrdp's own decoder expects
//! length-prefixed, but **Microsoft's mstsc decoder requires Annex-B** — this
//! was settled empirically 2026-05-20: mstsc renders with Annex-B, but with
//! length-prefixed it never sends a single frame-ack and the surface stays
//! blank. So we DEFAULT to Annex-B and keep length-prefixed one env var away
//! (`MACRDP_H264_LENGTH_PREFIXED=1`) for ironrdp-decoder interop testing.

#![cfg(target_os = "macos")]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use ironrdp_dvc::encode_dvc_messages;
use ironrdp_egfx::pdu::{
    Avc420Region, CacheImportOfferPdu, CapabilitiesAdvertisePdu, CapabilitiesV103Flags,
    CapabilitiesV104Flags, CapabilitiesV107Flags, CapabilitiesV10Flags, CapabilitiesV81Flags,
    CapabilitySet, PixelFormat,
};
use ironrdp_egfx::server::{GraphicsPipelineHandler, GraphicsPipelineServer, QoeMetrics, Surface};
use ironrdp_pdu::gcc::{Monitor, MonitorFlags};
use ironrdp_server::{
    EgfxServerMessage, GfxDvcBridge, GfxServerFactory, GfxServerHandle, ServerEvent,
    ServerEventSender,
};
use ironrdp_svc::ChannelFlags;
use tokio::sync::mpsc;
use tracing::{debug, info, trace, warn};

use crate::videotoolbox::{EncodedFrame, Encoder};

/// Minimum spacing between "trickle" frames let through the EGFX-on-UDP
/// backpressure gate while the client's frame-ack lag is over the threshold.
/// ~10 fps: enough trailing frames for mstsc to keep presenting + acking (so
/// the window reopens and lag recovers) while still throttling well below the
/// full 60 fps so the client's decode queue drains net. See the gate in
/// `submit_bgra` and `ConnectionContext::last_throttle_ship`.
const UDP_THROTTLE_FLOOR: Duration = Duration::from_millis(100);

/// P3 cold-start guard: how long after the first frame-ack the adaptive controller
/// ignores the ack-lag congestion signal. At connect the encoder ships an initial
/// burst (keyframe + first frames) before the client starts acking, so `shipped −
/// acked` spikes (~25) for ~1.5 s — that's startup backlog, not congestion, and
/// honoring it dips the bitrate right as the session opens. The retransmit signal
/// stays active during warmup (it's acks-independent).
const ADAPTIVE_WARMUP: Duration = Duration::from_secs(2);

/// How the H.264 NAL units are framed inside the AVC420 wire payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WireFormat {
    /// 4-byte big-endian length prefix per NAL (VideoToolbox's native AVCC).
    /// ironrdp's decoder documents this as the expected format.
    LengthPrefixed,
    /// `00 00 00 01` start codes (historical Windows/FreeRDP convention).
    AnnexB,
}

impl WireFormat {
    /// Annex-B is the verified-correct framing for Microsoft's decoder
    /// (mstsc renders the desktop with it; length-prefixed AVCC gets ZERO
    /// frame-acks and a blank surface — confirmed empirically 2026-05-20).
    /// Default to Annex-B; keep length-prefixed one env var away
    /// (`MACRDP_H264_LENGTH_PREFIXED=1`) for ironrdp-decoder interop testing.
    /// The legacy `MACRDP_H264_ANNEXB=1` is still accepted (now a no-op since
    /// Annex-B is the default).
    fn from_env() -> Self {
        match std::env::var("MACRDP_H264_LENGTH_PREFIXED") {
            Ok(v) if v == "1" || v.eq_ignore_ascii_case("true") => Self::LengthPrefixed,
            _ => Self::AnnexB,
        }
    }
}

/// Per-connection state, shared between the `Gfx` factory/handle (capture
/// side) and the `GfxHandler` callbacks (protocol side) via `Arc<Mutex<>>`.
struct ConnectionContext {
    server_handle: GfxServerHandle,
    encoder: Option<Encoder>,
    surface_id: Option<u16>,
    is_ready: bool,
    epoch: Instant,
    /// True once the next shipped frame must be a forced keyframe (IDR):
    /// before the first frame, and after any backpressure-induced skip, so
    /// the client never applies P-frame deltas against frames it never got.
    need_keyframe: bool,
    /// Whether the client's advertised EGFX caps indicate AVC420 (H.264)
    /// decode support. Set in `capabilities_advertise`, read in `on_ready`:
    /// if false we leave `is_ready` false so `submit_bgra` returns `Ok(false)`
    /// and capture.rs falls back to legacy BitmapUpdate, instead of shipping
    /// AVC420 to a client that can't decode it (which it rejects with
    /// ERROR_NOT_SUPPORTED and a dead graphics channel).
    client_supports_avc: bool,
    /// Drop-to-latest throttle counters for the push pipeline. `submitted` is
    /// bumped by `submit_bgra` (capture thread) per frame handed to VT;
    /// `shipped` is bumped by the ship thread per frame pulled back out and
    /// sent. `submitted - shipped` is how many frames are in the VT/ship
    /// pipeline; when it reaches `max_in_flight` the capture thread skips
    /// (drops to latest) — an ack-INDEPENDENT throttle, since clients commonly
    /// suspend frame acks (queue_depth=0xFFFFFFFF) which disables the EGFX
    /// ack-based backpressure entirely. Per-connection (fresh each context) so
    /// counts don't leak across reconnects; `Arc` so the ship thread shares
    /// `shipped`.
    submitted: Arc<AtomicU64>,
    shipped: Arc<AtomicU64>,
    /// Dimensions the surface + encoder were created with (in
    /// `setup_locked`, from the live `SharedDesktopSize`). `ship_frames`
    /// builds its AVC420 regions from these — not from a fresh
    /// `SharedDesktopSize` read — so a size adoption between setup and ship
    /// can't tear the region away from the surface.
    dims: (u16, u16),
    /// Ack-driven IDR recovery state (EGFX-on-lossy). Wall-clock; per-connection
    /// (reset on reconnect), all init to `now` so warmup doesn't false-trigger.
    /// `last_ack_at` + `acks_suspended` set in `on_frame_ack`; `last_ship_at` set
    /// in `ship_frames`; `last_recovery_at` set when a recovery IDR is armed in
    /// `submit_bgra`. See [`should_force_recovery_idr`].
    last_ack_at: Instant,
    acks_suspended: bool,
    last_ship_at: Instant,
    last_recovery_at: Instant,
    /// EGFX-on-UDP frame-ack backpressure. `last_shipped_frame_id` is the id of
    /// the most recent frame handed to `send_avc420_frame` (bumped by the ship
    /// thread); `last_acked_frame_id` is the most recent frame the client
    /// reported *decoded* via RDPGFX_FRAME_ACKNOWLEDGE (bumped in `on_frame_ack`).
    /// Their difference is the client's decode backlog. On TCP the socket's own
    /// backpressure paces the server to the client; on the UDP tunnel nothing
    /// does, so without this the server floods frames and the client's decode
    /// queue runs away → frozen video. `submit_bgra` drops captures (to latest)
    /// when the lag exceeds the threshold — but only once `egfx_acks_seen` (avoid
    /// a cold-start false drop before the first ack) and only while EGFX is on the
    /// UDP tunnel (`Gfx::egfx_on_udp`) and acks aren't suspended. `Arc` so the
    /// ship thread can bump `last_shipped_frame_id` without taking the ctx lock
    /// (the ship-path lock-order invariant: never hold ctx under server_handle).
    last_shipped_frame_id: Arc<AtomicU64>,
    last_acked_frame_id: Arc<AtomicU64>,
    egfx_acks_seen: bool,
    /// When the EGFX-on-UDP backpressure gate last let a "trickle" frame
    /// through while lag was over the threshold. The gate drops MOST captures
    /// when the client is behind, but NOT all of them: mstsc only *presents*
    /// (and thus frame-acks) an H.264 frame once a couple more arrive behind
    /// it, so dropping to zero starves that — the acks never advance, lag never
    /// recovers, and video freezes permanently. This timestamp paces a low-rate
    /// floor (`UDP_THROTTLE_FLOOR`) so the client always has trailing frames to
    /// drain its presentation buffer and the window reopens.
    last_throttle_ship: Instant,
    /// EGFX-over-UDP → TCP watchdog latch. Set true once the watchdog has
    /// de-migrated EGFX off a wedged RELIABLE UDP tunnel back onto TCP (see
    /// [`should_demigrate_to_tcp`]). One-way per connection — once de-migrated we
    /// never re-migrate to UDP in-session (no flapping); UDP is retried only on the
    /// next connection. Reset with the fresh context on reconnect.
    demigrated: bool,
    /// Adaptive-bitrate (congestion-responsive rate control) per-connection state.
    /// `adaptive_target_bps` is the controller's current target (starts at the
    /// configured `--bitrate` ceiling); `adaptive_last_control` rate-limits the AIMD
    /// step to one per `Gfx::adaptive_interval`; `adaptive_last_retransmits` is the
    /// last sampled value of the shared cumulative-retransmit loss counter, so the
    /// controller works on per-interval deltas. See [`Gfx::adaptive_bitrate_step`].
    adaptive_target_bps: u32,
    adaptive_last_control: Instant,
    adaptive_last_retransmits: u64,
    /// P2a IDR-backoff state: true while the periodic keyframe is suppressed
    /// (stretched) because the tunnel is congested. Restored (+ one recovery IDR)
    /// when the link fully recovers. See [`idr_backoff_decision`].
    idr_backed_off: bool,
    /// P3: which transport EGFX was on at the previous controller step, so the
    /// step can detect the UDP→TCP edge (watchdog de-migrate) and snap the bitrate
    /// back to the ceiling once before the TCP controller takes over. Mirrors
    /// `Gfx::egfx_on_udp`. See [`Gfx::adaptive_bitrate_step`].
    adaptive_on_udp: bool,
    /// P3 cold-start guard: set to `now + ADAPTIVE_WARMUP` the first time the
    /// controller sees acks flowing; while `now < this`, the ack-lag congestion
    /// signal is ignored (the connect-time startup backlog isn't congestion).
    /// `None` until the first ack. See [`Gfx::adaptive_bitrate_step`].
    adaptive_warmup_until: Option<Instant>,
    /// EWMA of the ack-lag signal (only updated while acks are usable, so the
    /// connect-time backlog never enters it). Fed to [`congested_hysteresis`].
    adaptive_lag_ewma: f64,
    /// Hysteresis state: whether the controller is currently in a congestion episode
    /// (enters when the EWMA lag clears the high mark, exits below the low mark).
    adaptive_congested: bool,
    /// P2b: when the last capture was let through under the frame-rate floor (the
    /// fps cap that engages once bitrate is at the floor and the link is still
    /// congested). Throttles let-throughs to `Gfx::adaptive_min_fps_interval`, like
    /// `last_throttle_ship` does for the EGFX-on-UDP trickle. See [`frame_drop_at_floor`].
    last_floor_fps_pass: Instant,
    /// Blank-presentation detector state (the mstsc reconnect-blank; see
    /// [`should_blank_recover`]). `qoe_reports` counts `on_qoe_metrics`
    /// callbacks (reset per recovery attempt so a re-fire needs a fresh
    /// all-zero window); `qoe_render_seen` latches true on the first report
    /// with a nonzero decode+render time (`time_diff_dr > 0`) — the proof the
    /// client actually presents, which disarms the detector for the connection.
    qoe_reports: u64,
    qoe_render_seen: bool,
    /// Recovery attempts this connection + when the last one ran (init to the
    /// connection epoch, which also serves as the arm-delay baseline).
    blank_recovery_attempts: u32,
    last_blank_recovery_at: Instant,
}

/// Tunables for ack-driven IDR recovery (EGFX-on-lossy). See
/// [`should_force_recovery_idr`] and `docs/rdp-udp-multitransport-feasibility.md`
/// ("Ack-driven IDR recovery").
#[derive(Clone, Copy, Debug)]
struct RecoveryParams {
    /// We only treat silent acks as loss while we're *actively* shipping — if the
    /// last ship is older than this, the screen is static and the periodic IDR
    /// backstops. Sized to cover the flush-burst window so a loss just before the
    /// screen goes static still heals.
    active_window: Duration,
    /// How long acks must stay silent (while shipping) before we infer a lost
    /// frame. Above normal ack jitter + RTT, below the periodic keyframe interval.
    ack_stall: Duration,
    /// Minimum spacing between forced recovery IDRs — the IDR is large and itself
    /// loss-vulnerable, so don't storm them if it keeps getting lost.
    min_recovery_interval: Duration,
}

/// Decide whether to force a recovery IDR from ack-staleness. Pure (takes
/// `Duration`s, not a clock) so it's unit-testable without timing. See the spec
/// in `docs/rdp-udp-multitransport-feasibility.md` — each clause guards a distinct
/// failure mode:
/// - `egfx_on_lossy`: only on the lossy tunnel; on TCP/reliable a missing ack is
///   congestion and an IDR would *worsen* it.
/// - `!acks_suspended`: with acks off (`queueDepth==0xFFFFFFFF`) loss is uninferable.
/// - `since_ship <= active_window`: only while actively shipping (else: static screen).
/// - `since_ack >= ack_stall`: the loss signal — acks went silent.
/// - `since_recovery >= min_recovery_interval`: rate-limit IDR storms.
fn should_force_recovery_idr(
    since_ship: Duration,
    since_ack: Duration,
    since_recovery: Duration,
    acks_suspended: bool,
    egfx_on_lossy: bool,
    p: &RecoveryParams,
) -> bool {
    egfx_on_lossy
        && !acks_suspended
        && since_ship <= p.active_window
        && since_ack >= p.ack_stall
        && since_recovery >= p.min_recovery_interval
}

/// Decide whether to de-migrate EGFX from the RELIABLE UDP tunnel back onto TCP.
/// Pure (Durations, not a clock) so it's unit-testable without timing.
///
/// The reliable (UdpFecR) tunnel is ordered, so it head-of-line-blocks under loss
/// exactly like TCP (feasibility finding #4): once the client stops acking while
/// we're *actively* shipping (the #89 trickle floor guarantees we keep shipping
/// even when the ack-lag is high), the tunnel is wedged and queued frames will
/// never arrive — the video freezes with no recovery until reconnect. The fix is
/// to route EGFX back over TCP, which mstsc accepts post-Soft-Sync (Spike A,
/// verified live 2026-06-29) — the caller pairs this with a forced IDR, since the
/// last UDP frames never arrived so the client's decode reference is stale.
///
/// Each clause guards a distinct failure mode:
/// - `egfx_on_udp && !egfx_on_lossy`: only on the RELIABLE UDP tunnel. The lossy
///   tunnel uses ack-driven IDR recovery instead ([`should_force_recovery_idr`]);
///   TCP needs nothing (socket backpressure paces it).
/// - `!already_demigrated`: fire once per connection (one-way latch, no flapping).
/// - `!acks_suspended`: with acks off (`queueDepth==0xFFFFFFFF`) a wedge can't be
///   inferred from ack-staleness.
/// - `since_ship <= active_window`: only while actively shipping (else: static
///   screen, where silent acks are normal and the periodic IDR backstops).
/// - `since_ack >= wedge_timeout`: the wedge signal — acks have gone fully silent
///   long enough to rule out a transient congestion blip.
#[allow(clippy::too_many_arguments)]
fn should_demigrate_to_tcp(
    since_ship: Duration,
    since_ack: Duration,
    acks_suspended: bool,
    egfx_on_udp: bool,
    egfx_on_lossy: bool,
    already_demigrated: bool,
    active_window: Duration,
    wedge_timeout: Duration,
) -> bool {
    egfx_on_udp
        && !egfx_on_lossy
        && !already_demigrated
        && !acks_suspended
        && since_ship <= active_window
        && since_ack >= wedge_timeout
}

/// Tunables for the blank-presentation detector + recovery (the mstsc
/// reconnect-blank). See [`should_blank_recover`] and the H.264 reconnect
/// quirk note in `docs/known-quirks.md`.
#[derive(Clone, Copy, Debug)]
struct BlankRecoveryParams {
    /// QoE reports that must accumulate (all with `time_diff_dr == 0`) before
    /// the session is declared blank. Upstream delivers ~8 `on_qoe_metrics`
    /// callbacks/s during active decoding (~1 per DVC batch, NOT per wire PDU —
    /// measured live 2026-07-02: ~850 wire QoE PDUs produced exactly 120
    /// callbacks), so 40 ≈ 5 s of active decoding. Also a floor on real decode
    /// activity — a static screen accrues slowly and simply defers detection.
    /// A *rendering* session is disarmed by its very first callbacks: the
    /// initial full-screen IDR present always costs >1 ms (observed 7–14 ms
    /// within ~230 ms of connect on every healthy session).
    min_qoe_reports: u64,
    /// Don't evaluate before this much of the connection has elapsed — the
    /// connect-time surface/caps churn shouldn't race the detector.
    arm_delay: Duration,
    /// Minimum spacing between recovery attempts (the QoE-report counter also
    /// resets per attempt, so a re-fire needs a full fresh all-zero window).
    retry_interval: Duration,
    /// Total attempts per connection. All attempts but the last REMAP the
    /// output to a fresh surface (non-destructive); the LAST attempt drops the
    /// connection so the client auto-reconnects (a fresh attempt renders with
    /// high probability, and the detector re-checks the new session).
    max_attempts: u32,
}

/// Which recovery lever to pull for a given (1-based) attempt number: every
/// attempt before the last remaps to a fresh surface; the last one drops the
/// connection. Pure, unit-tested.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum BlankAction {
    /// Create a fresh surface, map it over the output, force an IDR — the
    /// non-destructive in-session heal. Deliberately sends NO DeleteSurface and
    /// NO RESET_GRAPHICS: the resize-dance variant (upstream `resize_with_
    /// monitors`, whose first PDU deletes the mapped surface) was tried live
    /// 2026-07-02 and KILLED mstsc's GFX channel outright (zero acks/QoE for
    /// the rest of the session) — the third independent confirmation that any
    /// DeleteSurface aimed at a blank mstsc is fatal. The old surface is left
    /// alive; the client leaks at most (max_attempts − 1) surfaces.
    Remap,
    /// Drop the connection (`ServerEvent::Quit` → per-connection
    /// `RunState::Disconnect`). mstsc treats the unexpected loss as an outage
    /// and auto-reconnects with its reconnect cookie; a fresh connection
    /// renders with high probability and the detector re-checks it.
    Drop,
}

fn blank_action(attempt: u32, max_attempts: u32) -> BlankAction {
    if attempt < max_attempts {
        BlankAction::Remap
    } else {
        BlankAction::Drop
    }
}

/// Decide whether to run a blank-recovery attempt. Pure (counters + Durations,
/// not a clock) so it's unit-testable without timing.
///
/// The signal (pcap-proven 2026-07-02, validated live the same day — see
/// [[h264-reconnect-blank]]): a reconnect that lands on mstsc's stale retained
/// surface DECODES every frame — FrameAcks and QoE acks flow normally — but
/// never PRESENTS, and its QoE Frame Acknowledge PDUs report `timeDiffEDR == 0`
/// on every single frame. A rendering session shows nonzero EDR on its first
/// callbacks (~100–230 ms after connect). So: QoE flowing + zero render-time
/// ever = the client is painting into a surface nobody composites (the
/// reconnect-blank). The caller then pulls the [`blank_action`] lever for the
/// attempt number: remap to a fresh surface, or drop the connection.
///
/// Each clause guards a distinct failure mode:
/// - `qoe_reports >= min_qoe_reports`: enough evidence, and implies the client
///   actually sends QoE acks at all (FreeRDP-family clients that don't are
///   simply never evaluated — no false recovery on non-QoE clients).
/// - `!qoe_render_seen`: a single nonzero EDR proves presentation and disarms
///   the detector for the connection.
/// - `egfx_acks_seen && !acks_suspended`: regular FrameAcks flowing too — the
///   blank signature is "acking normally while EDR stays zero", not a stalled
///   or suspended client (those are congestion, handled elsewhere).
/// - `since_connect >= arm_delay`: skip the connect-time churn window.
/// - `since_last_attempt >= retry_interval` + `attempts < max_attempts`:
///   rate-limit; the final attempt is the connection drop, after which the
///   fresh connection starts a fresh detector.
#[allow(clippy::too_many_arguments)]
fn should_blank_recover(
    qoe_reports: u64,
    qoe_render_seen: bool,
    egfx_acks_seen: bool,
    acks_suspended: bool,
    since_connect: Duration,
    since_last_attempt: Duration,
    attempts: u32,
    p: &BlankRecoveryParams,
) -> bool {
    qoe_reports >= p.min_qoe_reports
        && !qoe_render_seen
        && egfx_acks_seen
        && !acks_suspended
        && since_connect >= p.arm_delay
        && since_last_attempt >= p.retry_interval
        && attempts < p.max_attempts
}

/// Pure AIMD step for congestion-responsive bitrate (P1). Given the current target,
/// the reliable-tunnel loss delta observed this control interval, and the bounds/
/// params, return the new target bitrate. **Multiplicative-decrease** on any loss
/// (back off fast, clamp to `floor_bps`); **additive-increase** when clean (climb
/// slowly, clamp to `ceiling_bps`). Pure (no clock/state) so it's unit-testable.
/// See [`Gfx::adaptive_bitrate_step`].
fn aimd_bitrate(
    current: u32,
    loss_delta: u64,
    floor_bps: u32,
    ceiling_bps: u32,
    increase_bps: u32,
    decrease: f32,
) -> u32 {
    if loss_delta > 0 {
        (((current as f32) * decrease) as u32).max(floor_bps)
    } else {
        current.saturating_add(increase_bps).min(ceiling_bps)
    }
}

/// What the IDR-backoff sub-controller (P2a) should do this interval. A periodic
/// keyframe is a big intra frame — the worst thing to inject into a congested,
/// backed-up tunnel — so under congestion we **stretch** the keyframe interval
/// (effectively suppressing the periodic IDR) and **restore** it (plus force one
/// clean recovery IDR) once the link has fully recovered. Safe on the RELIABLE
/// tunnel: reliable delivery means there's no loss-corruption to heal, so the
/// periodic IDR is only a decode-glitch safety net we can defer until clear.
#[derive(Debug, PartialEq, Eq)]
enum IdrBackoff {
    /// Loss started and we're not yet backed off → suppress the periodic IDR.
    Stretch,
    /// Fully recovered (clean + back at the bitrate ceiling) while backed off →
    /// restore the normal interval and force one recovery IDR.
    Restore,
    /// No change this interval.
    Hold,
}

/// Pure IDR-backoff decision (P2a). `loss` = any reliable retransmit this interval;
/// `new_target`/`ceiling` are the post-AIMD bitrate and its ceiling; `backed_off`
/// is the current state. Unit-tested. See [`Gfx::adaptive_bitrate_step`].
fn idr_backoff_decision(loss: bool, new_target: u32, ceiling: u32, backed_off: bool) -> IdrBackoff {
    if loss && !backed_off {
        IdrBackoff::Stretch
    } else if !loss && new_target >= ceiling && backed_off {
        IdrBackoff::Restore
    } else {
        IdrBackoff::Hold
    }
}

/// Whether the reliable-tunnel retransmits observed this control interval count as
/// loss, given the per-interval `tolerance`. The tunnel retransmits on *any* packet
/// loss and a wireless link (WiFi) has near-continuous low-level loss, so a single
/// retransmit must NOT read as congestion — only `retransmit_delta > tolerance` does.
/// `tolerance == 0` restores the old "any retransmit = loss" behaviour. Pure +
/// unit-tested. See [`Gfx::adaptive_bitrate_step`] and the `adaptive_retx_tolerance` field.
fn retransmit_is_lossy(retransmit_delta: u64, tolerance: u64) -> bool {
    retransmit_delta > tolerance
}

/// Pure congestion decision with EWMA smoothing + hysteresis. `ewma_lag` is the
/// exponentially-smoothed frame-ack lag (shipped − acked); the caller smooths the raw
/// per-interval lag so a single spike doesn't trip a back-off (raw TCP ack-lag bursts
/// 0↔40 even at moderate loss, which a naive threshold turns into visible bitrate
/// pumping). **Hysteresis:** enter congestion when `ewma_lag` crosses `high`, then stay
/// congested until it falls below `low` (`low < high`) — so the bitrate doesn't
/// flip-flop while the signal straddles one threshold. `retransmit_lossy` (UDP — the
/// caller has already applied the per-interval retransmit *tolerance*, so this is
/// "loss above the wireless background", not "any retransmit") forces congested
/// immediately (a definite loss, no smoothing). With acks unusable (suspended / not yet
/// seen / cold-start warmup), the lag is uninferable so only the retransmit signal
/// counts. Unit-tested. See [`Gfx::adaptive_bitrate_step`].
fn congested_hysteresis(
    ewma_lag: f64,
    high: f64,
    low: f64,
    retransmit_lossy: bool,
    acks_usable: bool,
    currently_congested: bool,
) -> bool {
    if retransmit_lossy {
        return true;
    }
    if !acks_usable {
        return false;
    }
    if currently_congested {
        ewma_lag > low // stay congested until the smoothed lag drops below the low mark
    } else {
        ewma_lag > high // only enter once the smoothed lag clears the high mark
    }
}

/// What the controller does to the bitrate this interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RateAction {
    /// Smoothed lag is above the high mark (or a retransmit) — multiplicative-decrease.
    Decrease,
    /// In the hysteresis band (congested, lag decaying between low and high) — hold the
    /// current bitrate. Stops a single spike from cratering the bitrate as the EWMA
    /// decays back through the band (it would otherwise decrease every interval → the
    /// "video sometimes stops" deep dips).
    Hold,
    /// Cleared (below the low mark / not in an episode) — additive-increase toward ceiling.
    Increase,
}

/// Pure 3-zone bitrate action on the smoothed signal (AIMD with a hold band).
/// `congested` is the hysteresis state from [`congested_hysteresis`] this interval.
/// Decrease while genuinely congested (lag above `high`, or retransmits above the
/// tolerance); hold while
/// the episode is still latched but the smoothed lag is decaying back through the band;
/// increase once cleared. So a single spike = one step down then a plateau, while
/// *sustained* congestion (lag stays above `high`) keeps decreasing toward the floor.
/// Unit-tested. See [`Gfx::adaptive_bitrate_step`].
fn rate_action(
    ewma_lag: f64,
    high: f64,
    retransmit_lossy: bool,
    acks_usable: bool,
    congested: bool,
) -> RateAction {
    if retransmit_lossy || (acks_usable && ewma_lag > high) {
        RateAction::Decrease
    } else if !congested {
        RateAction::Increase
    } else {
        RateAction::Hold
    }
}

/// P2b — pure decision: should this capture be DROPPED to enforce a frame-rate floor?
///
/// Engages only once the bitrate controller has already cut the encoder to its floor
/// (`at_floor`) AND the link is still `congested` — i.e. lowering quality can no longer
/// help, so the next lever is shedding *frames* (fewer frames → fewer packets → less
/// load). It caps the effective frame rate to a floor by dropping any capture that
/// arrives within `min_interval` of the last one we let through (`since_last_pass`).
///
/// It never drops to zero: a capture is let through once `min_interval` has elapsed, so
/// the client always keeps receiving trailing frames to present/ack (the same reason the
/// EGFX-on-UDP trickle floor never zeroes — dropping to zero pins the lag and freezes the
/// picture). Works on BOTH transports; on TCP it's the only fps lever (there's no UDP
/// frame-ack backpressure gate). When the link recovers (`congested` clears or the
/// controller climbs off the floor) it stops dropping and the full capture rate resumes.
/// Unit-tested. See [`Gfx::submit_bgra`].
fn frame_drop_at_floor(
    at_floor: bool,
    congested: bool,
    since_last_pass: Duration,
    min_interval: Duration,
) -> bool {
    at_floor && congested && since_last_pass < min_interval
}

/// Encoder adjustments the adaptive controller wants applied this frame: the P1
/// bitrate and the P2a keyframe interval. `None` fields = no change. The controller
/// also sets `ctx.need_keyframe` directly when forcing a recovery IDR.
#[derive(Default)]
struct AdaptiveActions {
    bitrate_bps: Option<u32>,
    keyframe_frames: Option<u32>,
}

/// Read the ack-recovery config from the environment once. Returns
/// `(enabled, params)`; disabled (default) keeps the feature off and the path
/// byte-identical. Tunables: `MACRDP_UDP_EGFX_ACK_STALL_MS` (200),
/// `MACRDP_UDP_EGFX_ACK_ACTIVE_MS` (500), `MACRDP_UDP_EGFX_ACK_RECOVERY_MS` (1000).
fn recovery_config_from_env() -> (bool, RecoveryParams) {
    let ms = |name: &str, default: u64| -> Duration {
        let v = std::env::var(name)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(default);
        Duration::from_millis(v)
    };
    let enabled = crate::multitransport::env_truthy("MACRDP_UDP_EGFX_ACK_RECOVERY");
    let params = RecoveryParams {
        active_window: ms("MACRDP_UDP_EGFX_ACK_ACTIVE_MS", 500),
        ack_stall: ms("MACRDP_UDP_EGFX_ACK_STALL_MS", 200),
        min_recovery_interval: ms("MACRDP_UDP_EGFX_ACK_RECOVERY_MS", 1000),
    };
    (enabled, params)
}

/// Cloneable factory + frame-submit handle. One clone is boxed into
/// `RdpServer::builder().with_gfx_factory(...)`; another lives on the capture
/// side as the `submit_bgra` entry point.
#[derive(Clone)]
pub struct Gfx {
    sender: Arc<Mutex<Option<mpsc::UnboundedSender<ServerEvent>>>>,
    ctx: Arc<Mutex<Option<ConnectionContext>>>,
    /// Live session desktop size, shared with `CaptureDisplay` /
    /// `MacInputHandler`. Read in `setup_locked` when the per-connection
    /// surface + encoder are created, so the H.264 pipeline tracks the
    /// client-resolution auto-adopt without rebuilding the factory.
    desktop_size: crate::capture::SharedDesktopSize,
    fps: u32,
    bitrate_bps: u32,
    /// Periodic keyframe (IDR) interval in seconds (from `--keyframe-interval`).
    /// Heal-vs-smoothness knob; converted to a frame count by `Encoder::new`.
    keyframe_secs: f32,
    /// Capture-side drop-to-latest depth (from `--h264-frames-in-flight`): the
    /// max frames allowed in the VT/ship pipeline (`submitted - shipped`) before
    /// `submit_bgra` skips a capture. Bounds interactive latency under load: a
    /// deeper window buffers more (smoother video) but lets a backlog build; a
    /// shallow one drops-to-latest sooner (snappier) at the cost of more skips.
    /// Read in `submit_bgra` — NOT the EGFX send-side window (that's `u32::MAX`,
    /// so encoded frames are never dropped, which would break the H.264 chain).
    max_in_flight: u32,
    wire_format: WireFormat,
    /// Ack-driven IDR recovery (EGFX-on-lossy). `recovery_enabled` is the opt-in
    /// env gate (`MACRDP_UDP_EGFX_ACK_RECOVERY`); `egfx_on_lossy` is the *runtime*
    /// gate the vendored server flips true when it migrates EGFX onto the lossy
    /// tunnel. Both must hold for `submit_bgra` to arm a recovery IDR. Default-off
    /// → byte-identical to the pre-feature path.
    recovery_enabled: bool,
    recovery_params: RecoveryParams,
    egfx_on_lossy: Arc<AtomicBool>,
    /// Runtime gate the vendored server flips true when EGFX is migrated onto the
    /// UDP multitransport tunnel (reliable OR lossy). Enables the frame-ack-lag
    /// backpressure in `submit_bgra` — only on the UDP tunnel, since the TCP path
    /// is paced by socket backpressure and must stay byte-identical (never-drop
    /// push model). Stays false on TCP → the gate is a no-op there.
    egfx_on_udp: Arc<AtomicBool>,
    /// Max EGFX frame-ack lag (shipped − decoded, in frames) tolerated on the UDP
    /// tunnel before `submit_bgra` drops captures to let the client catch up.
    /// `MACRDP_UDP_EGFX_MAX_FRAME_LAG` (default 16 ≈ 266 ms at 60 fps). High enough
    /// that a healthy high-RTT link never trips it; low enough to cap the decode
    /// backlog so video degrades to choppy-but-live instead of freezing.
    max_frame_lag: u64,
    /// EGFX-over-UDP → TCP watchdog. On by default (disable with
    /// `MACRDP_UDP_EGFX_WATCHDOG=0`); only ever acts while EGFX is on the RELIABLE
    /// UDP tunnel (`egfx_on_udp && !egfx_on_lossy`), so it's a no-op unless
    /// `--udp-migrate-egfx` is in use. On a detected wedge `submit_bgra` forces an
    /// IDR, resets the lag baseline, and sets `demigrate_request` — the cue the
    /// vendored server reads to flip EGFX routing back to the TCP DRDYNVC channel.
    watchdog_enabled: bool,
    /// How long EGFX frame acks must stay fully silent (while actively shipping)
    /// before the reliable UDP tunnel is declared wedged.
    /// `MACRDP_UDP_EGFX_WATCHDOG_MS` (default 3000). The freeze the user sees
    /// before auto-recovery is ~this long; long enough to rule out a transient blip.
    watchdog_wedge_timeout: Duration,
    /// Companion to `watchdog_wedge_timeout`: the last ship must be within this
    /// window for the ack silence to read as a wedge (vs. a static screen).
    /// `MACRDP_UDP_EGFX_WATCHDOG_ACTIVE_MS` (default 1000).
    watchdog_active_window: Duration,
    /// Shared with the vendored server: set true here on a wedge, read there to
    /// flip `egfx_on_udp` → TCP routing; reset there on reconnect.
    demigrate_request: Arc<AtomicBool>,
    /// Adaptive bitrate (congestion-responsive rate control, P1). On by
    /// `--adaptive-bitrate` (or `MACRDP_UDP_ADAPTIVE_BITRATE`); only acts while EGFX
    /// is on a UDP tunnel, so it's a no-op on TCP. The controller (AIMD) detects
    /// congestion primarily from the client's **frame-ack lag** (`shipped − acked` >
    /// `adaptive_lag_threshold`) — the FAST signal that rises the moment the client
    /// stops acking, ~2 s before the watchdog's ack-silence trigger — and secondarily
    /// from the shared `congestion_retransmits` counter (a late RTO-based signal). It
    /// live-adjusts the VideoToolbox bitrate within `[adaptive_floor_bps, bitrate_bps]`:
    /// multiplicative-decrease `adaptive_decrease` per interval with congestion,
    /// additive-increase `adaptive_increase_bps` per interval when clean. `bitrate_bps`
    /// (the `--bitrate` value) is the ceiling. See [`Gfx::adaptive_bitrate_step`].
    adaptive_enabled: bool,
    adaptive_floor_bps: u32,
    adaptive_increase_bps: u32,
    adaptive_decrease: f32,
    adaptive_interval: Duration,
    /// Frame-ack lag (shipped − acked, in frames) above which the controller treats
    /// the link as congested — the fast pre-watchdog signal. Below `max_frame_lag`
    /// (the frame-DROP threshold) so the controller leads the backpressure gate, and
    /// well above mstsc's healthy presentation jitter (~0–2). Default `max_frame_lag
    /// / 2`; `MACRDP_UDP_ADAPTIVE_LAG_THRESHOLD` overrides.
    adaptive_lag_threshold: u64,
    /// P3: ack-lag congestion threshold for the **TCP** path. The same `shipped −
    /// acked` signal works on TCP (the EGFX `FrameAcknowledge` flows there too, and
    /// the unbounded ship channel lets `shipped` advance so the lag reflects the
    /// full encoder→socket→client backlog), but TCP's send buffer adds baseline
    /// depth, so the threshold is higher than the UDP one. Default `max_frame_lag`;
    /// `MACRDP_TCP_ADAPTIVE_LAG_THRESHOLD` overrides. Only consulted while EGFX is
    /// on TCP and `--adaptive-bitrate` is set.
    adaptive_tcp_lag_threshold: u64,
    /// EWMA weight on each new ack-lag sample in (0,1]: `ewma = α·sample + (1−α)·ewma`.
    /// Smooths the spiky raw lag so single bursts don't pump the bitrate; lower = more
    /// smoothing (slower reaction). Default 0.3; `MACRDP_ADAPTIVE_EWMA_ALPHA` overrides.
    /// The hysteresis exit threshold is half the (transport) entry threshold.
    adaptive_ewma_alpha: f64,
    /// Retransmit tolerance (per control interval) for the UDP loss signal. The
    /// reliable tunnel retransmits on *any* packet loss, and a wireless link (WiFi)
    /// has near-continuous low-level loss, so treating a single retransmit as
    /// congestion made the controller ratchet the bitrate down with no recovery
    /// (decrease on any retransmit; increase only on a *zero*-retransmit interval,
    /// which WiFi rarely gives). Instead, only `retransmit_delta > tolerance` in an
    /// interval counts as loss — so sporadic single retransmits are ignored and the
    /// bitrate can still climb under low background loss, while *sustained* loss
    /// (delta above the tolerance every interval) still backs off. Default 2;
    /// `MACRDP_UDP_ADAPTIVE_RETX_TOLERANCE` overrides (0 = the old "any retransmit"
    /// behaviour). Only the UDP path produces retransmits, so this never affects TCP.
    adaptive_retx_tolerance: u64,
    /// P2a IDR backoff: the normal periodic-keyframe interval (frames, from
    /// `--keyframe-interval`) restored on recovery, and the stretched value used to
    /// suppress the periodic IDR under congestion.
    normal_keyframe_frames: u32,
    stretched_keyframe_frames: u32,
    /// Cumulative reliable-tunnel retransmit count, bumped by the UDP listener and
    /// sampled (as deltas) by the controller. Shared `Arc` like the egfx flags.
    congestion_retransmits: Arc<AtomicU64>,
    /// P2b: minimum spacing between captures once the frame-rate floor engages (bitrate
    /// at the floor AND still congested) — i.e. `1 / floor-fps`. When P2b is active,
    /// captures arriving sooner than this are dropped, capping the effective frame rate
    /// so a congested link sheds packet load that bitrate cuts alone can't. Default 10
    /// fps (100 ms); `MACRDP_ADAPTIVE_FLOOR_FPS` overrides. See [`frame_drop_at_floor`].
    adaptive_min_fps_interval: Duration,
    /// Blank-presentation recovery (the mstsc reconnect-blank). On by default
    /// (`MACRDP_BLANK_RECOVERY=0` disables); a strict no-op unless a QoE-acking
    /// client (mstsc) decodes a whole detection window without ever presenting —
    /// see [`should_blank_recover`]. On detection: remap the output to a fresh
    /// surface + IDR ([`Gfx::perform_blank_remap`]); if the blank persists
    /// through a fresh window, drop the connection so the client auto-reconnects
    /// (see [`BlankAction`]).
    blank_recovery_enabled: bool,
    blank_params: BlankRecoveryParams,
}

impl Gfx {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        desktop_size: crate::capture::SharedDesktopSize,
        fps: u32,
        bitrate_bps: u32,
        keyframe_secs: f32,
        max_in_flight: u32,
        egfx_on_lossy: Arc<AtomicBool>,
        egfx_on_udp: Arc<AtomicBool>,
        demigrate_request: Arc<AtomicBool>,
        adaptive_bitrate: bool,
        congestion_retransmits: Arc<AtomicU64>,
    ) -> Self {
        let wire_format = WireFormat::from_env();
        let (recovery_enabled, recovery_params) = recovery_config_from_env();
        let max_frame_lag = std::env::var("MACRDP_UDP_EGFX_MAX_FRAME_LAG")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(16);
        let watchdog_enabled = match std::env::var("MACRDP_UDP_EGFX_WATCHDOG") {
            Ok(v) => !(v == "0" || v.eq_ignore_ascii_case("false")),
            Err(_) => true, // default on (no-op unless EGFX is on the reliable UDP tunnel)
        };
        let watchdog_ms = |name: &str, default: u64| -> Duration {
            Duration::from_millis(
                std::env::var(name)
                    .ok()
                    .and_then(|s| s.trim().parse::<u64>().ok())
                    .filter(|&n| n > 0)
                    .unwrap_or(default),
            )
        };
        let watchdog_wedge_timeout = watchdog_ms("MACRDP_UDP_EGFX_WATCHDOG_MS", 3000);
        let watchdog_active_window = watchdog_ms("MACRDP_UDP_EGFX_WATCHDOG_ACTIVE_MS", 1000);
        // Adaptive bitrate (P1). Enabled by the --adaptive-bitrate flag OR the env
        // fallback; the controller still only acts while EGFX is on a UDP tunnel.
        let adaptive_enabled =
            adaptive_bitrate || crate::multitransport::env_truthy("MACRDP_UDP_ADAPTIVE_BITRATE");
        let env_u32 = |name: &str, default: u32| -> u32 {
            std::env::var(name)
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok())
                .filter(|&n| n > 0)
                .unwrap_or(default)
        };
        // Floor: don't drop below this (degrade to "choppy but alive", not dead).
        // Default = 1/8 of the ceiling, clamped to ≥500 kbps and ≤ the ceiling.
        let adaptive_floor_bps = env_u32(
            "MACRDP_UDP_ADAPTIVE_FLOOR_BPS",
            (bitrate_bps / 8).max(500_000),
        )
        .min(bitrate_bps.max(1));
        // Additive-increase step per interval: ~1/16 of the ceiling (≈16 clean
        // intervals to climb the full range). Multiplicative-decrease factor on loss.
        let adaptive_increase_bps = env_u32(
            "MACRDP_UDP_ADAPTIVE_INCREASE_BPS",
            (bitrate_bps / 16).max(250_000),
        );
        let adaptive_decrease = std::env::var("MACRDP_UDP_ADAPTIVE_DECREASE")
            .ok()
            .and_then(|s| s.trim().parse::<f32>().ok())
            .filter(|&f| f > 0.0 && f < 1.0)
            .unwrap_or(0.7);
        let adaptive_interval = watchdog_ms("MACRDP_UDP_ADAPTIVE_INTERVAL_MS", 300);
        // Congestion lag threshold: react below the frame-DROP point (max_frame_lag)
        // so the controller leads the backpressure gate, and above healthy jitter.
        let adaptive_lag_threshold = std::env::var("MACRDP_UDP_ADAPTIVE_LAG_THRESHOLD")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|&n| n > 0)
            .unwrap_or((max_frame_lag / 2).max(4));
        // P3 TCP-path threshold: higher than UDP (TCP send-buffer adds baseline lag),
        // but below the frame-DROP point. Default ¾·max_frame_lag (=12 at the default
        // 16) — validated on real mstsc under clumsy TCP loss to catch genuine
        // congestion spikes (ack_lag 14–40) while ignoring the healthy 0–12 jitter.
        let adaptive_tcp_lag_threshold = std::env::var("MACRDP_TCP_ADAPTIVE_LAG_THRESHOLD")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|&n| n > 0)
            .unwrap_or((max_frame_lag * 3 / 4).max(6));
        // EWMA smoothing weight for the ack-lag signal (clamped to (0,1]); default 0.3.
        let adaptive_ewma_alpha = std::env::var("MACRDP_ADAPTIVE_EWMA_ALPHA")
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|&a| a > 0.0 && a <= 1.0)
            .unwrap_or(0.3);
        // Retransmit tolerance per control interval for the UDP loss signal (0 = the
        // old "any retransmit = loss" behaviour). Default 2 so sporadic single
        // wireless retransmits don't ratchet the bitrate down on WiFi. See the field.
        let adaptive_retx_tolerance = std::env::var("MACRDP_UDP_ADAPTIVE_RETX_TOLERANCE")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(2);
        // P2b frame-rate floor: once bitrate is pinned at the floor and the link is
        // still congested, cap the effective fps to this (drop captures arriving sooner
        // than 1/fps). Default 10 fps — matches the EGFX-on-UDP trickle floor and stays
        // well above "must keep presenting." `MACRDP_ADAPTIVE_FLOOR_FPS` overrides.
        let adaptive_floor_fps = env_u32("MACRDP_ADAPTIVE_FLOOR_FPS", 10).max(1);
        let adaptive_min_fps_interval = Duration::from_millis(1000 / u64::from(adaptive_floor_fps));
        // P2a IDR backoff: the configured periodic-keyframe interval in frames (same
        // derivation as Encoder::new), and a stretched value (~10 min) that
        // effectively suppresses the periodic IDR for the duration of a congestion
        // episode without a hard "never" (VT still honors forced keyframes).
        let normal_keyframe_frames =
            (f64::from(fps) * f64::from(keyframe_secs)).round().max(1.0) as u32;
        let stretched_keyframe_frames = fps.saturating_mul(600).max(normal_keyframe_frames + 1);
        // Blank-presentation recovery (the mstsc reconnect-blank): on by default —
        // it only ever acts on the pcap-proven blank signature (QoE acks flowing
        // with decode+render time zero on EVERY frame of a whole window), so a
        // rendering client is effectively never touched. `MACRDP_BLANK_RECOVERY=0`
        // is the kill switch; the window tunables are for experimentation.
        // min_qoe 40 ≈ 5 s of active decoding (callbacks arrive ~8/s, per DVC
        // batch — measured live). max_attempts 2 = one non-destructive remap,
        // then the connection drop.
        let blank_recovery_enabled = match std::env::var("MACRDP_BLANK_RECOVERY") {
            Ok(v) => !(v == "0" || v.eq_ignore_ascii_case("false")),
            Err(_) => true,
        };
        let blank_params = BlankRecoveryParams {
            min_qoe_reports: u64::from(env_u32("MACRDP_BLANK_RECOVERY_MIN_QOE", 40)),
            arm_delay: watchdog_ms("MACRDP_BLANK_RECOVERY_ARM_MS", 3000),
            retry_interval: watchdog_ms("MACRDP_BLANK_RECOVERY_RETRY_MS", 5000),
            max_attempts: env_u32("MACRDP_BLANK_RECOVERY_MAX_ATTEMPTS", 2),
        };
        let (width, height) = desktop_size.get();
        info!(
            ?wire_format,
            width, height, fps, keyframe_secs, max_in_flight, "EGFX/H.264 pipeline configured"
        );
        if recovery_enabled {
            info!(
                ?recovery_params,
                "EGFX ack-driven IDR recovery ENABLED (MACRDP_UDP_EGFX_ACK_RECOVERY) — \
                 active only while EGFX is on the lossy UDP tunnel"
            );
        }
        if adaptive_enabled {
            info!(
                ceiling_bps = bitrate_bps,
                floor_bps = adaptive_floor_bps,
                increase_bps = adaptive_increase_bps,
                decrease = adaptive_decrease,
                interval_ms = adaptive_interval.as_millis() as u64,
                lag_threshold = adaptive_lag_threshold,
                tcp_lag_threshold = adaptive_tcp_lag_threshold,
                ewma_alpha = adaptive_ewma_alpha,
                normal_keyframe_frames,
                stretched_keyframe_frames,
                floor_fps = adaptive_floor_fps,
                "EGFX adaptive bitrate + IDR backoff + frame-rate floor ENABLED (--adaptive-bitrate) — \
                 congestion-responsive rate control on both the UDP tunnel and the TCP path"
            );
        }
        Self {
            sender: Arc::new(Mutex::new(None)),
            ctx: Arc::new(Mutex::new(None)),
            desktop_size,
            fps,
            bitrate_bps,
            keyframe_secs,
            max_in_flight,
            wire_format,
            recovery_enabled,
            recovery_params,
            egfx_on_lossy,
            egfx_on_udp,
            max_frame_lag,
            watchdog_enabled,
            watchdog_wedge_timeout,
            watchdog_active_window,
            demigrate_request,
            adaptive_enabled,
            adaptive_floor_bps,
            adaptive_increase_bps,
            adaptive_decrease,
            adaptive_interval,
            adaptive_lag_threshold,
            adaptive_tcp_lag_threshold,
            adaptive_ewma_alpha,
            adaptive_retx_tolerance,
            normal_keyframe_frames,
            stretched_keyframe_frames,
            congestion_retransmits,
            adaptive_min_fps_interval,
            blank_recovery_enabled,
            blank_params,
        }
    }

    /// Proactively de-migrate EGFX off the reliable UDP tunnel back onto TCP when
    /// the client comes out of a minimize (SuppressOutput → restore).
    ///
    /// Once the EGFX channel has been Soft-Sync-migrated onto the reliable UDP
    /// tunnel, mstsc's surface does NOT survive a minimize/restore: on restore the
    /// picture freezes and the watchdog's *reactive* de-migrate (3–6 s later, after
    /// we've already shipped frames into the now-stale tunnel) is too late to heal
    /// it — recovery needs a fresh mstsc. The fix is to switch the transport back to
    /// TCP on the un-suppress edge, BEFORE shipping a single frame back into the
    /// post-minimize tunnel, so the forced restore-IDR lands over TCP (which mstsc
    /// renders cleanly across minimize, exactly like an EGFX-on-TCP-from-start
    /// session). One-way per connection (the `demigrated` latch), and a no-op unless
    /// EGFX is currently on the reliable UDP tunnel → the TCP/default path and the
    /// lossy path are byte-unchanged.
    pub fn demigrate_on_resume(&self) {
        let on_reliable_udp =
            self.egfx_on_udp.load(Ordering::Relaxed) && !self.egfx_on_lossy.load(Ordering::Relaxed);
        if !on_reliable_udp {
            return; // not on the reliable UDP tunnel → nothing to switch
        }
        let mut guard = self.ctx.lock().unwrap();
        let Some(ctx) = guard.as_mut() else {
            return;
        };
        if ctx.demigrated {
            return; // already on TCP for this session
        }
        ctx.need_keyframe = true;
        ctx.last_acked_frame_id.store(
            ctx.last_shipped_frame_id.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        ctx.demigrated = true;
        self.demigrate_request.store(true, Ordering::Relaxed);
        warn!(
            "client resumed from minimize while EGFX was on the reliable UDP tunnel — \
             proactively de-migrating to TCP (one-way for this session) so the restore \
             IDR lands over TCP instead of a stale tunnel"
        );
    }

    /// Feed one full-frame BGRA buffer. Never blocks on the encoder.
    ///
    /// `request_keyframe` asks for the next encoded frame to be a forced IDR —
    /// pass it when a lot of the screen just changed (a window raised to front,
    /// a scroll, an app launch). Such large updates render as a big P-frame that
    /// some clients (mstsc) only resolve cleanly at the next periodic IDR (the
    /// "takes a while to come to front" lag); a forced IDR lands them at once.
    ///
    /// Returns `Ok(true)` when EGFX is the active display path (so the caller
    /// should suppress legacy BitmapUpdates for this frame — even if this
    /// particular frame was skipped for backpressure or isn't encoded yet).
    /// Returns `Ok(false)` when EGFX hasn't negotiated (no connection, still
    /// negotiating, or a non-EGFX client), so the caller falls back to legacy.
    pub fn submit_bgra(&self, bgra: &[u8], stride: usize, request_keyframe: bool) -> Result<bool> {
        // Push pipeline: this (capture) thread only converts + submits to VT and
        // returns immediately; a dedicated ship thread (spawned in setup_locked)
        // pulls each encoded frame off VT's output channel and ships it the
        // instant it's ready. The capture thread never blocks on the encoder, so
        // it keeps pace with ScreenCaptureKit instead of falling behind under
        // heavy frames (which would queue stale frames → growing latency).
        // Blank-presentation recovery: when the detector fires inside the ctx
        // guard below, the action itself must run OUTSIDE it (the remap locks
        // `server_handle`, which is never taken while holding ctx — the ship/ack
        // lock-order invariant). The decision extracts what the action needs here.
        let mut blank_recovery: Option<(BlankAction, GfxServerHandle, u16, u16, u32)> = None;
        let force_keyframe = {
            let mut guard = self.ctx.lock().unwrap();
            let Some(ctx) = guard.as_mut() else {
                return Ok(false); // no active connection
            };
            if !ctx.is_ready {
                return Ok(false); // channel not negotiated yet (or non-EGFX client)
            }
            // Arm the keyframe BEFORE the throttle check so a large change that
            // lands on a dropped frame still forces the IDR on the next encoded
            // frame (the change is still on screen by then).
            if request_keyframe {
                ctx.need_keyframe = true;
            }
            // Ack-driven IDR recovery (opt-in, EGFX-on-lossy only): if acks have
            // gone silent while we're actively shipping, infer a lost frame and arm
            // an IDR so the client recovers without waiting for the periodic
            // keyframe. Armed BEFORE the throttle so a dropped capture still carries
            // the IDR forward (need_keyframe persists across skips). No-op unless
            // both the env gate and the runtime lossy-tunnel gate hold → default
            // path unchanged.
            if self.recovery_enabled {
                let now = Instant::now();
                let since_ack = now.saturating_duration_since(ctx.last_ack_at);
                if should_force_recovery_idr(
                    now.saturating_duration_since(ctx.last_ship_at),
                    since_ack,
                    now.saturating_duration_since(ctx.last_recovery_at),
                    ctx.acks_suspended,
                    self.egfx_on_lossy.load(Ordering::Relaxed),
                    &self.recovery_params,
                ) {
                    ctx.need_keyframe = true;
                    ctx.last_recovery_at = now;
                    info!(
                        since_ack_ms = since_ack.as_millis() as u64,
                        "EGFX ack-stall on lossy tunnel — forcing recovery IDR"
                    );
                }
            }
            // EGFX-over-UDP → TCP watchdog (default on): the RELIABLE tunnel is
            // ordered, so it head-of-line-blocks under loss (finding #4). If acks go
            // fully silent while we're still actively shipping (the #89 trickle keeps
            // frames flowing even when lag is high), the tunnel is wedged and queued
            // frames will never arrive → the video freezes with no recovery until
            // reconnect. Route EGFX back to TCP (mstsc renders it post-Soft-Sync —
            // Spike A) + force an IDR (the last UDP frames never arrived, so the
            // client's reference is stale) + reset the lag baseline so the trickle
            // gate below doesn't drop the recovery IDR before the server flips
            // routing. One-way per connection (the `demigrated` latch). No-op unless
            // EGFX is on the reliable UDP tunnel → default path unchanged.
            if self.watchdog_enabled {
                let now = Instant::now();
                let since_ack = now.saturating_duration_since(ctx.last_ack_at);
                if should_demigrate_to_tcp(
                    now.saturating_duration_since(ctx.last_ship_at),
                    since_ack,
                    ctx.acks_suspended,
                    self.egfx_on_udp.load(Ordering::Relaxed),
                    self.egfx_on_lossy.load(Ordering::Relaxed),
                    ctx.demigrated,
                    self.watchdog_active_window,
                    self.watchdog_wedge_timeout,
                ) {
                    ctx.need_keyframe = true;
                    ctx.last_acked_frame_id.store(
                        ctx.last_shipped_frame_id.load(Ordering::Relaxed),
                        Ordering::Relaxed,
                    );
                    ctx.demigrated = true;
                    self.demigrate_request.store(true, Ordering::Relaxed);
                    warn!(
                        since_ack_ms = since_ack.as_millis() as u64,
                        "EGFX-over-UDP reliable tunnel wedged (acks silent while shipping) — \
                         de-migrating to TCP + forcing IDR (one-way for this session)"
                    );
                }
            }
            // Lazy one-time setup on the first ready frame (creates the encoder
            // and spawns the ship thread).
            if ctx.surface_id.is_none() || ctx.encoder.is_none() {
                self.setup_locked(ctx)?;
            }
            // Blank-presentation detector (the mstsc reconnect-blank): the client
            // is decoding + acking every frame (QoE reports flowing) but its
            // reported decode+render time has been zero on every single one — it
            // is compositing into a stale retained surface and never presenting
            // (pcap-proven signature, 2026-07-02). Pull the recovery lever for
            // this attempt: remap the output to a fresh surface, or (last
            // attempt) drop the connection so the client auto-reconnects.
            // Decision here (under ctx), action after the guard drops.
            // `qoe_reports` resets so a re-fire needs a fresh all-zero window.
            if self.blank_recovery_enabled && ctx.surface_id.is_some() {
                let now = Instant::now();
                if should_blank_recover(
                    ctx.qoe_reports,
                    ctx.qoe_render_seen,
                    ctx.egfx_acks_seen,
                    ctx.acks_suspended,
                    now.saturating_duration_since(ctx.epoch),
                    now.saturating_duration_since(ctx.last_blank_recovery_at),
                    ctx.blank_recovery_attempts,
                    &self.blank_params,
                ) {
                    ctx.blank_recovery_attempts += 1;
                    ctx.last_blank_recovery_at = now;
                    let evidence = ctx.qoe_reports;
                    ctx.qoe_reports = 0;
                    let (w, h) = ctx.dims;
                    let action =
                        blank_action(ctx.blank_recovery_attempts, self.blank_params.max_attempts);
                    blank_recovery = Some((
                        action,
                        ctx.server_handle.clone(),
                        w,
                        h,
                        ctx.blank_recovery_attempts,
                    ));
                    warn!(
                        ?action,
                        attempt = ctx.blank_recovery_attempts,
                        max_attempts = self.blank_params.max_attempts,
                        qoe_reports_all_zero = evidence,
                        "EGFX client is decoding but not presenting (QoE decode+render \
                         time zero across the whole window — the reconnect-blank) — \
                         running blank recovery"
                    );
                }
            }
            // P2b frame-rate floor: once the adaptive controller has cut the bitrate to
            // its floor AND the link is still congested, lowering quality can't help —
            // so shed FRAMES. Cap the effective fps (drop captures arriving within
            // `adaptive_min_fps_interval`), cutting packet load on BOTH transports — it's
            // the only fps lever on TCP, which has no UDP frame-ack backpressure gate.
            // Never zero: one capture per interval gets through so the client keeps
            // trailing frames to present/ack. The controller state read here was set by
            // the previous interval's adaptive_bitrate_step (runs after submit) — fine,
            // congestion persists for seconds. need_keyframe persists across the drop
            // (consumed below), so an armed IDR lands on the next let-through. Dropping
            // before encode keeps the H.264 reference chain valid. No-op unless
            // --adaptive-bitrate AND the controller is actually at the floor under
            // congestion → default path unchanged.
            if self.adaptive_enabled {
                let at_floor = ctx.adaptive_target_bps <= self.adaptive_floor_bps;
                let now = Instant::now();
                if frame_drop_at_floor(
                    at_floor,
                    ctx.adaptive_congested,
                    now.duration_since(ctx.last_floor_fps_pass),
                    self.adaptive_min_fps_interval,
                ) {
                    trace!(
                        "EGFX at bitrate floor + congested; dropping capture (frame-rate floor)"
                    );
                    return Ok(true);
                }
                if at_floor && ctx.adaptive_congested {
                    ctx.last_floor_fps_pass = now;
                    debug!("EGFX frame-rate floor active — capping fps (let a capture through)");
                }
            }
            // Drop-to-latest throttle: if too many frames are still in the
            // VT/ship pipeline, skip this capture entirely. This bounds latency
            // under load WITHOUT relying on frame acks (clients commonly suspend
            // them, which disables the EGFX ack-based backpressure). Skipping a
            // capture before encode doesn't break the reference chain — the next
            // encoded frame is a valid P-frame against the last encoded one — and
            // an armed `need_keyframe` persists across the skip.
            let outstanding = ctx
                .submitted
                .load(Ordering::Relaxed)
                .saturating_sub(ctx.shipped.load(Ordering::Relaxed));
            if outstanding >= u64::from(self.max_in_flight) {
                trace!(
                    outstanding,
                    "EGFX pipeline full; dropping capture to latest"
                );
                return Ok(true); // still the active path; just dropped this frame
            }
            // EGFX-on-UDP frame-ack backpressure: on the UDP tunnel there's no
            // socket backpressure to pace us to the client (unlike TCP), so without
            // this the server floods frames and the client's DECODE queue runs away
            // → frozen video while audio (on TCP) keeps playing. When the client's
            // decode backlog (shipped − decoded, from FrameAcknowledge) exceeds the
            // threshold, drop this capture so the client catches up — video degrades
            // to choppy-but-live instead of freezing. Gated to the UDP tunnel
            // (TCP push path stays byte-identical), to acks actually flowing (a
            // suspended-ack client falls back to the submitted−shipped throttle
            // above), and to having seen ≥1 ack (no cold-start false drop). Dropping
            // before encode keeps the H.264 reference chain valid.
            if self.egfx_on_udp.load(Ordering::Relaxed) && ctx.egfx_acks_seen && !ctx.acks_suspended
            {
                let lag = ctx
                    .last_shipped_frame_id
                    .load(Ordering::Relaxed)
                    .saturating_sub(ctx.last_acked_frame_id.load(Ordering::Relaxed));
                if lag > self.max_frame_lag {
                    // Client is behind. Drop MOST captures so it catches up — but
                    // keep a low-rate trickle, never zero: mstsc only presents (and
                    // thus frame-acks) an H.264 frame once a couple more arrive
                    // behind it, so dropping to zero means it never acks the
                    // in-flight frames, `lag` never falls back under the threshold,
                    // and the video freezes PERMANENTLY (recovers only on
                    // reconnect). The trickle keeps trailing frames flowing so the
                    // presentation buffer drains and the window reopens. Dropping
                    // before encode keeps the H.264 reference chain valid (the
                    // encoder never sees the dropped frames, so the next encoded
                    // frame is a valid P-frame from the client's last reference).
                    let now = Instant::now();
                    if now.duration_since(ctx.last_throttle_ship) < UDP_THROTTLE_FLOOR {
                        trace!(
                            lag,
                            "EGFX-on-UDP lag high; dropping capture (trickle floor)"
                        );
                        return Ok(true);
                    }
                    ctx.last_throttle_ship = now;
                    trace!(
                        lag,
                        "EGFX-on-UDP lag high; letting a trickle frame through to drain client buffer"
                    );
                    // fall through: ship this one to keep the client presenting/acking
                }
            }
            std::mem::replace(&mut ctx.need_keyframe, false)
        };

        // Run the armed recovery action now that ctx is released (lock order).
        // This capture is dropped either way: a remap re-arms `need_keyframe`,
        // so the NEXT capture ships as the IDR into the fresh surface, cleanly
        // ordered after the queued CreateSurface/Map PDUs; a drop needs no frame
        // at all (the connection is ending).
        if let Some((action, server_handle, width, height, attempt)) = blank_recovery {
            let result = match action {
                BlankAction::Remap => self.perform_blank_remap(&server_handle, width, height),
                BlankAction::Drop => self.perform_blank_drop(),
            };
            if let Err(e) = result {
                warn!(error = ?e, ?action, attempt, "EGFX blank recovery failed");
            }
            return Ok(true);
        }

        // Submit to VideoToolbox (async). The ship thread delivers + ships the
        // output; we just count the submission for the drop-to-latest throttle.
        {
            let mut guard = self.ctx.lock().unwrap();
            let Some(ctx) = guard.as_mut() else {
                return Ok(true);
            };
            // Congestion-responsive control (P1 bitrate + P2a IDR backoff): compute
            // the adjustments (mutates ctx adaptive state, may set need_keyframe)
            // BEFORE borrowing the encoder, then apply them live. No-op unless
            // adaptive is enabled AND EGFX is on a UDP tunnel.
            let adaptive = self.adaptive_bitrate_step(ctx);
            let Some(encoder) = ctx.encoder.as_mut() else {
                return Ok(true);
            };
            if let Some(bps) = adaptive.bitrate_bps {
                if let Err(e) = encoder.set_bitrate(bps) {
                    trace!(error = ?e, bps, "adaptive set_bitrate failed");
                }
            }
            if let Some(frames) = adaptive.keyframe_frames {
                if let Err(e) = encoder.set_keyframe_interval(frames) {
                    trace!(error = ?e, frames, "adaptive set_keyframe_interval failed");
                }
            }
            encoder.encode_bgra(bgra, stride, force_keyframe)?;
            ctx.submitted.fetch_add(1, Ordering::Relaxed);
        }
        Ok(true)
    }

    /// Congestion-responsive controller (P1 bitrate AIMD + P2a IDR backoff). Called
    /// once per capture from `submit_bgra` while holding the ctx lock; rate-limited to
    /// one step per `adaptive_interval`. Detects congestion from the client's frame-ack
    /// lag (fast, leads the watchdog) and the shared retransmit counter (late), and
    /// per interval:
    /// - **bitrate:** multiplicative-decrease toward `adaptive_floor_bps` on any loss,
    ///   else additive-increase toward the `bitrate_bps` ceiling;
    /// - **IDR backoff:** stretch the periodic keyframe interval (suppress the
    ///   periodic IDR) when congestion starts, restore it + force one recovery IDR
    ///   when the link is fully recovered (clean + back at the ceiling).
    ///
    /// Returns the encoder adjustments to apply this frame; may also set
    /// `ctx.need_keyframe`. A no-op (`AdaptiveActions::default()`) unless
    /// `--adaptive-bitrate` is set — so with the feature off the path stays
    /// byte-identical. With it on, the bitrate AIMD runs on BOTH transports (P3): the
    /// `shipped − acked` ack-lag signal works on TCP too (FrameAcknowledge flows
    /// there and the unbounded ship channel lets `shipped` advance, so the lag
    /// reflects the real backlog). IDR backoff stays UDP-only — on TCP the periodic
    /// keyframe is cheap insurance and not worth the false-suppress risk.
    fn adaptive_bitrate_step(&self, ctx: &mut ConnectionContext) -> AdaptiveActions {
        let mut actions = AdaptiveActions::default();
        if !self.adaptive_enabled {
            return actions;
        }
        let on_udp = self.egfx_on_udp.load(Ordering::Relaxed);
        let ceiling = self.bitrate_bps.max(1);
        let now = Instant::now();
        // Cold-start guard: arm the warmup window the first time acks flow, then
        // ignore the ack-lag signal until it elapses (the connect-time startup
        // backlog spikes ack_lag but isn't congestion). Retransmit signal stays on.
        if ctx.egfx_acks_seen && ctx.adaptive_warmup_until.is_none() {
            ctx.adaptive_warmup_until = Some(now + ADAPTIVE_WARMUP);
        }
        let in_warmup = ctx.adaptive_warmup_until.is_some_and(|t| now < t);
        // Transport edge: EGFX moved UDP→TCP (watchdog de-migrate). Snap the bitrate
        // back to the full ceiling + restore the normal keyframe once — TCP handles
        // far more than the UDP floor and never HOL-freezes (it just slows), so we
        // want instant recovery, not a slow AIMD climb from the floor. The TCP
        // controller below then manages from the ceiling. Skip one control interval
        // afterward (reset adaptive_last_control) so the post-de-migrate IDR's
        // transient ack-lag spike doesn't immediately re-trigger a back-off.
        if ctx.adaptive_on_udp && !on_udp {
            if ctx.adaptive_target_bps < ceiling {
                ctx.adaptive_target_bps = ceiling;
                actions.bitrate_bps = Some(ceiling);
            }
            if ctx.idr_backed_off {
                ctx.idr_backed_off = false;
                actions.keyframe_frames = Some(self.normal_keyframe_frames);
            }
            if actions.bitrate_bps.is_some() || actions.keyframe_frames.is_some() {
                ctx.adaptive_last_control = now;
                info!(
                    ceiling_bps = ceiling,
                    "EGFX left the UDP tunnel — restoring full bitrate + normal keyframe for the TCP path"
                );
            }
            // Stale UDP-side smoothing state doesn't apply to the fresh TCP path.
            ctx.adaptive_lag_ewma = 0.0;
            ctx.adaptive_congested = false;
        }
        ctx.adaptive_on_udp = on_udp;

        if now.duration_since(ctx.adaptive_last_control) < self.adaptive_interval {
            return actions;
        }
        ctx.adaptive_last_control = now;
        let cur = self.congestion_retransmits.load(Ordering::Relaxed);
        let retransmit_delta = cur.saturating_sub(ctx.adaptive_last_retransmits);
        ctx.adaptive_last_retransmits = cur;
        // Apply the per-interval retransmit tolerance: a wireless link retransmits
        // continuously at a low rate, so only loss *above* the tolerance counts as
        // congestion. Without this, any single retransmit forced a multiplicative
        // decrease while an increase needed a zero-retransmit interval (rare on WiFi)
        // → the bitrate ratcheted down with no recovery.
        let retransmit_lossy = retransmit_is_lossy(retransmit_delta, self.adaptive_retx_tolerance);
        // Congestion signal: the client's frame-ack lag (shipped − acked). On UDP it
        // rises ~2 s before the watchdog's ack-silence trigger (the retransmit counter
        // only climbs after an RTO, too late); on TCP it reflects the encoder→socket→
        // client backlog growing under congestion. The raw lag is spiky (TCP bursts
        // 0↔40 even at moderate loss), so smooth it with an EWMA and decide via
        // hysteresis — otherwise single spikes pump the bitrate. Threshold is
        // transport-specific (TCP's send buffer adds baseline depth); exit at half it.
        let ack_lag = ctx
            .last_shipped_frame_id
            .load(Ordering::Relaxed)
            .saturating_sub(ctx.last_acked_frame_id.load(Ordering::Relaxed));
        let acks_usable = ctx.egfx_acks_seen && !ctx.acks_suspended && !in_warmup;
        // Only fold real samples into the EWMA — during warmup/suspend the lag is the
        // startup backlog, not congestion, and must not pre-load the average.
        if acks_usable {
            ctx.adaptive_lag_ewma = self.adaptive_ewma_alpha * ack_lag as f64
                + (1.0 - self.adaptive_ewma_alpha) * ctx.adaptive_lag_ewma;
        }
        let high = if on_udp {
            self.adaptive_lag_threshold
        } else {
            self.adaptive_tcp_lag_threshold
        } as f64;
        let congested = congested_hysteresis(
            ctx.adaptive_lag_ewma,
            high,
            high * 0.5, // exit threshold (hysteresis low mark)
            retransmit_lossy,
            acks_usable,
            ctx.adaptive_congested,
        );
        ctx.adaptive_congested = congested;
        // 3-zone action: decrease above the high mark, hold while the EWMA decays
        // through the band (so a single spike doesn't crater the bitrate), increase
        // once cleared. aimd_bitrate does the decrease/increase math (1 = decrease,
        // 0 = increase); Hold leaves the target untouched.
        let action = rate_action(
            ctx.adaptive_lag_ewma,
            high,
            retransmit_lossy,
            acks_usable,
            congested,
        );
        let new_target = match action {
            RateAction::Hold => ctx.adaptive_target_bps,
            RateAction::Decrease => aimd_bitrate(
                ctx.adaptive_target_bps,
                1,
                self.adaptive_floor_bps,
                ceiling,
                self.adaptive_increase_bps,
                self.adaptive_decrease,
            ),
            RateAction::Increase => aimd_bitrate(
                ctx.adaptive_target_bps,
                0,
                self.adaptive_floor_bps,
                ceiling,
                self.adaptive_increase_bps,
                self.adaptive_decrease,
            ),
        };
        if new_target != ctx.adaptive_target_bps {
            let prev = ctx.adaptive_target_bps;
            ctx.adaptive_target_bps = new_target;
            debug!(
                transport = if on_udp { "udp" } else { "tcp" },
                ack_lag,
                ewma_lag = ctx.adaptive_lag_ewma,
                retransmit_delta,
                ?action,
                prev_bps = prev,
                new_bps = new_target,
                "EGFX adaptive bitrate adjusted"
            );
            actions.bitrate_bps = Some(new_target);
        }
        // P2a IDR backoff: don't inject a big periodic keyframe into a congested
        // tunnel. UDP-only — on TCP (reliable, no HOL-freeze) the periodic keyframe is
        // cheap decode-glitch insurance and the false-suppress risk isn't worth it.
        if on_udp {
            match idr_backoff_decision(congested, new_target, ceiling, ctx.idr_backed_off) {
                IdrBackoff::Stretch => {
                    ctx.idr_backed_off = true;
                    actions.keyframe_frames = Some(self.stretched_keyframe_frames);
                    // info (not debug): fires at most once per congestion episode.
                    info!("EGFX IDR backoff: suppressing periodic keyframe under congestion");
                }
                IdrBackoff::Restore => {
                    ctx.idr_backed_off = false;
                    actions.keyframe_frames = Some(self.normal_keyframe_frames);
                    ctx.need_keyframe = true; // one clean recovery IDR now the link is clear
                    info!(
                        "EGFX IDR backoff: link recovered — restoring periodic keyframe + forcing recovery IDR"
                    );
                }
                IdrBackoff::Hold => {}
            }
        }
        actions
    }

    /// Ship loop for the push pipeline: owns VideoToolbox's output receiver and
    /// ships each encoded frame the instant it arrives, fully decoupled from the
    /// capture tick. Bumps `shipped` per frame so the capture thread's
    /// drop-to-latest throttle can bound the pipeline depth. Exits when the
    /// channel closes (encoder dropped on connection teardown).
    fn ship_loop(&self, rx: std::sync::mpsc::Receiver<EncodedFrame>, shipped: Arc<AtomicU64>) {
        while let Ok(frame) = rx.recv() {
            // Sweep up any others VT delivered alongside it (keeps order).
            let mut frames = vec![frame];
            while let Ok(f) = rx.try_recv() {
                frames.push(f);
            }
            let n = frames.len() as u64;
            if let Err(e) = self.ship_frames(&frames) {
                warn!(error = ?e, "EGFX ship_frames failed");
            }
            shipped.fetch_add(n, Ordering::Relaxed);
        }
        debug!("EGFX ship loop exiting (output channel closed)");
    }

    /// One-time per-connection surface + encoder setup. Caller holds `ctx`.
    fn setup_locked(&self, ctx: &mut ConnectionContext) -> Result<()> {
        // Read the live session size once and pin it for this connection's
        // surface + encoder + ship-side regions.
        let (width, height) = self.desktop_size.get();
        if ctx.surface_id.is_none() {
            ctx.dims = (width, height);
            let mut server = ctx.server_handle.lock().unwrap();
            server.set_output_dimensions(width, height);
            // Emit RESET_GRAPHICS with an explicit single-monitor layout
            // covering the full desktop, BEFORE create_surface. The auto-reset
            // path inside create_surface sends an EMPTY monitor array; mstsc
            // tolerates that on the first GFX session (it falls back to the
            // demand-active desktop region) but NOT on reconnect — with no
            // monitor defining the graphics output region, a correctly decoded
            // + acked surface has nowhere to composite and the screen stays
            // blank. resize_with_monitors sets reset_graphics_sent=true so the
            // empty-monitor reset never fires. (reconnect-blank fix 2026-05-20.)
            let monitor = Monitor {
                left: 0,
                top: 0,
                right: i32::from(width).saturating_sub(1),
                bottom: i32::from(height).saturating_sub(1),
                flags: MonitorFlags::PRIMARY,
            };
            server.resize_with_monitors(width, height, vec![monitor]);
            // Create the surface with upstream's auto-allocated id. mstsc retains
            // EGFX surfaces by id for its whole process lifetime and no-ops a
            // CreateSurface for an id it already holds, so a reconnect to the
            // same mstsc process can land on a stale surface and paint blank.
            // A fresh per-session id (the old vendored `create_surface_with_id`)
            // only mitigated this *unreliably* on mstsc — sometimes the desktop
            // drew, sometimes it didn't — for the cost of a permanent upstream
            // divergence. Since the reliable recovery is the same either way
            // (close + reopen mstsc, which clears its surface cache), we use the
            // stock API and document the quirk instead. See [[h264-reconnect-blank]].
            let sid = server
                .create_surface_with_format(width, height, PixelFormat::XRgb)
                .ok_or_else(|| anyhow!("EGFX: create_surface failed (not ready?)"))?;
            if !server.map_surface_to_output(sid, 0, 0) {
                return Err(anyhow!("EGFX: map_surface_to_output failed"));
            }
            ctx.surface_id = Some(sid);
            info!(
                surface_id = sid,
                w = width,
                h = height,
                "EGFX surface created + mapped"
            );
        }
        // Encoder dims always follow the surface's creation dims, so an
        // encoder (re)build can never disagree with an existing surface.
        let (width, height) = ctx.dims;
        if ctx.encoder.is_none() {
            // Pass actual dims; VideoToolbox pads to 16-px macroblocks
            // internally and encodes the crop in the SPS, so the client
            // decodes back to actual dims.
            let mut encoder = Encoder::new(
                width,
                height,
                self.fps,
                self.bitrate_bps,
                self.keyframe_secs,
            )?;
            // Hand VT's output channel to a dedicated ship thread (push model),
            // so encoded frames are sent the instant they're ready, off the
            // capture thread. The thread exits when the encoder is dropped (on
            // connection teardown) and its sender closes.
            let rx = encoder
                .take_receiver()
                .ok_or_else(|| anyhow!("EGFX: encoder receiver already taken"))?;
            ctx.encoder = Some(encoder);
            // Fresh throttle counters for this connection.
            ctx.submitted.store(0, Ordering::Relaxed);
            ctx.shipped.store(0, Ordering::Relaxed);
            let gfx = self.clone();
            let shipped = ctx.shipped.clone();
            std::thread::Builder::new()
                .name("egfx-ship".into())
                .spawn(move || gfx.ship_loop(rx, shipped))
                .map_err(|e| anyhow!("EGFX: failed to spawn ship thread: {e}"))?;
            info!("EGFX VideoToolbox encoder initialized + ship thread started");
        }
        Ok(())
    }

    /// Blank-recovery attempt A — remap the graphics output to a FRESH surface
    /// (see [`should_blank_recover`] / [`BlankAction::Remap`]): CreateSurface
    /// with a fresh id + MapSurfaceToOutput over the same origin + a forced IDR
    /// on the next capture. Deliberately **non-destructive**: no DeleteSurface,
    /// no RESET_GRAPHICS — the resize-dance variant (upstream
    /// `resize_with_monitors`, whose first PDU deletes the mapped surface) was
    /// tried live 2026-07-02 and killed mstsc's GFX channel outright (zero
    /// acks/QoE afterwards; visual: shrunken black + transparent). The stale
    /// surface is left alive client-side (bounded: at most `max_attempts − 1`
    /// extra surfaces per connection); MapSurfaceToOutput over the same output
    /// origin makes the fresh surface the composite source per MS-RDPEGFX.
    ///
    /// Lock order: `server_handle` is taken ALONE (never under ctx — the
    /// ship/ack invariant); ctx is re-taken afterwards to publish the new
    /// surface id, guarded by `Arc::ptr_eq` so a reconnect that swapped the
    /// context mid-remap is left untouched. Frames the ship thread sends in
    /// that window still target the old surface — which still exists (nothing
    /// was deleted), so the stream stays valid either way.
    fn perform_blank_remap(
        &self,
        server_handle: &GfxServerHandle,
        width: u16,
        height: u16,
    ) -> Result<()> {
        let (dvc_messages, egfx_channel_id, new_sid) = {
            let mut server = server_handle.lock().unwrap();
            let egfx_channel_id = server
                .channel_id()
                .ok_or_else(|| anyhow!("EGFX blank remap: channel_id not assigned"))?;
            // Fresh id: the allocator has advanced past the (stale) id 0.
            // reset_graphics_sent is true since setup, so no implicit
            // RESET_GRAPHICS rides along with the create.
            let sid = server
                .create_surface_with_format(width, height, PixelFormat::XRgb)
                .ok_or_else(|| anyhow!("EGFX blank remap: create_surface failed"))?;
            if !server.map_surface_to_output(sid, 0, 0) {
                return Err(anyhow!("EGFX blank remap: map_surface_to_output failed"));
            }
            (server.drain_output(), egfx_channel_id, sid)
        };
        if !dvc_messages.is_empty() {
            let svc_messages =
                encode_dvc_messages(egfx_channel_id, dvc_messages, ChannelFlags::SHOW_PROTOCOL)
                    .map_err(|e| anyhow!("EGFX blank remap: encode_dvc_messages failed: {e}"))?;
            let sender = self
                .sender
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| anyhow!("EGFX blank remap: server-event sender not set"))?;
            sender
                .send(ServerEvent::Egfx(EgfxServerMessage::SendMessages {
                    messages: svc_messages,
                }))
                .map_err(|_| anyhow!("EGFX blank remap: ServerEvent send failed"))?;
        }
        // Publish the fresh surface to the connection — unless a reconnect
        // swapped the context out from under the remap.
        let mut guard = self.ctx.lock().unwrap();
        match guard.as_mut() {
            Some(ctx) if Arc::ptr_eq(&ctx.server_handle, server_handle) => {
                ctx.surface_id = Some(new_sid);
                ctx.need_keyframe = true;
                info!(
                    new_surface_id = new_sid,
                    w = width,
                    h = height,
                    "EGFX blank remap complete — fresh surface mapped over the output, next \
                     frame is an IDR; a nonzero QoE decode+render time will confirm the client \
                     presents"
                );
            }
            _ => warn!("EGFX blank remap finished on a stale connection — result discarded"),
        }
        Ok(())
    }

    /// Blank-recovery attempt B — drop the connection
    /// ([`BlankAction::Drop`]). `ServerEvent::Quit` is handled by the active
    /// connection's `client_loop` as a per-connection `RunState::Disconnect`
    /// (the listener keeps accepting), and mstsc treats the unexpected loss as
    /// an outage and auto-reconnects with its reconnect cookie — a fresh
    /// connection renders with high probability, and its fresh detector
    /// re-checks. (Narrow, accepted race: if the client vanishes in the same
    /// instant, the queued Quit could reach the between-connections event loop
    /// and stop the server — under the LaunchAgent that's an auto-restart; the
    /// detector only fires on a live, actively-decoding connection, so the
    /// window is microscopic.)
    fn perform_blank_drop(&self) -> Result<()> {
        let sender = self
            .sender
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow!("EGFX blank drop: server-event sender not set"))?;
        warn!(
            "EGFX blank persisted through the fresh-surface remap — dropping the connection \
             so the client auto-reconnects (a fresh connection usually renders)"
        );
        sender
            .send(ServerEvent::Quit(
                "EGFX blank recovery: client decodes but never presents".into(),
            ))
            .map_err(|_| anyhow!("EGFX blank drop: ServerEvent send failed"))?;
        Ok(())
    }

    fn ship_frames(&self, frames: &[EncodedFrame]) -> Result<()> {
        let (dvc_messages, egfx_channel_id) = {
            // Phase 1: read what we need out of `ctx`, then DROP the ctx lock
            // before touching `server_handle`. The inbound EGFX frame-ack path
            // (`GfxDvcBridge::process` → `GraphicsPipelineServer::process` →
            // `GfxHandler::on_frame_ack`) locks `server_handle` FIRST and then
            // `ctx`. Holding `ctx` here while taking `server_handle` is the
            // opposite order — a classic lock-order inversion that deadlocks the
            // ship thread against an inbound ack. Over a long session the exact
            // interleaving eventually hits and the whole pipeline freezes (idle
            // CPU, no error, no reset — the "renders fine then freezes after a
            // few seconds" stall, far more likely once acks ride the UDP tunnel).
            // Cloning the `server_handle` Arc and releasing `ctx` first keeps the
            // lock order consistent (server_handle is never nested under ctx).
            let (surface_id, width, height, epoch, server_handle, last_shipped) = {
                let mut guard = self.ctx.lock().unwrap();
                let ctx = guard
                    .as_mut()
                    .ok_or_else(|| anyhow!("EGFX: ctx vanished mid-submit"))?;
                let surface_id = ctx
                    .surface_id
                    .ok_or_else(|| anyhow!("EGFX: no surface_id"))?;
                let (width, height) = ctx.dims;
                let epoch = ctx.epoch;
                // Liveness for ack-driven IDR recovery: we're actively shipping.
                ctx.last_ship_at = Instant::now();
                // Clone the shipped-frame-id gauge so we can bump it in Phase 2
                // (under server_handle) WITHOUT re-taking the ctx lock — preserving
                // the never-hold-ctx-under-server_handle invariant.
                (
                    surface_id,
                    width,
                    height,
                    epoch,
                    ctx.server_handle.clone(),
                    ctx.last_shipped_frame_id.clone(),
                )
            };

            // Phase 2: lock `server_handle` ALONE (ctx already released).
            let mut server = server_handle.lock().unwrap();
            let egfx_channel_id = server
                .channel_id()
                .ok_or_else(|| anyhow!("EGFX: channel_id not assigned"))?;

            for f in frames {
                // Region = full actual frame, inclusive bounds. QP 22 /
                // quality 100 are first-light defaults; tuned in M3. Rebuilt
                // per frame because `Avc420Region` isn't `Copy`.
                let region = Avc420Region {
                    left: 0,
                    top: 0,
                    right: width.saturating_sub(1),
                    bottom: height.saturating_sub(1),
                    quantization_parameter: 22,
                    quality: 100,
                };
                let payload = self.frame_payload(f);
                let ts_ms =
                    u32::try_from(epoch.elapsed().as_millis() % u128::from(u32::MAX)).unwrap_or(0);
                // Diagnostic for the reconnect-blank investigation: keyframes
                // are rare (session start + backpressure resume), so log each
                // at INFO. A correct (re)connect must emit an IDR with SPS/PPS
                // as the FIRST frame of the session; if the first shipped frame
                // after "EGFX surface created" is a P-frame (keyframe=false /
                // param_sets=0), the new client has no reference to paint and
                // the surface stays blank.
                let ps_count = f.parameter_sets.len();
                let ps_bytes: usize = f.parameter_sets.iter().map(Vec::len).sum();
                let sent = server.send_avc420_frame(surface_id, &payload, &[region], ts_ms);
                // Record the newest shipped frame id for the UDP frame-ack-lag
                // backpressure gate (`submit_bgra`). Monotonic per connection.
                if let Some(frame_id) = sent {
                    last_shipped.store(u64::from(frame_id), Ordering::Relaxed);
                }
                match sent {
                    Some(frame_id) if f.is_keyframe => debug!(
                        frame_id,
                        ?self.wire_format,
                        param_sets = ps_count,
                        param_bytes = ps_bytes,
                        payload_bytes = payload.len(),
                        "EGFX shipped keyframe (IDR)"
                    ),
                    Some(frame_id) => trace!(
                        frame_id,
                        keyframe = false,
                        payload_bytes = payload.len(),
                        "EGFX shipped frame"
                    ),
                    None => debug!(
                        keyframe = f.is_keyframe,
                        param_sets = ps_count,
                        bytes = payload.len(),
                        "send_avc420_frame returned None"
                    ),
                }
            }
            (server.drain_output(), egfx_channel_id)
        };

        if dvc_messages.is_empty() {
            return Ok(());
        }
        // DRDYNVC framing, addressed to the EGFX dynamic channel. SHOW_PROTOCOL
        // matches what upstream's Echo handler uses for DRDYNVC-wrapped data.
        let svc_messages =
            encode_dvc_messages(egfx_channel_id, dvc_messages, ChannelFlags::SHOW_PROTOCOL)
                .map_err(|e| anyhow!("encode_dvc_messages failed: {e}"))?;
        let sender = self
            .sender
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow!("EGFX: server-event sender not set"))?;
        sender
            .send(ServerEvent::Egfx(EgfxServerMessage::SendMessages {
                messages: svc_messages,
            }))
            .map_err(|_| anyhow!("EGFX: ServerEvent send failed (event loop closed)"))?;
        Ok(())
    }

    /// Frame the encoded NALs for the wire per the selected `WireFormat`,
    /// prepending SPS/PPS (from VT's format description) on keyframes.
    fn frame_payload(&self, f: &EncodedFrame) -> Vec<u8> {
        match self.wire_format {
            // VT data is already AVCC (length-prefixed); just prepend the
            // parameter sets as length-prefixed NALs on keyframes.
            WireFormat::LengthPrefixed => {
                if !f.is_keyframe || f.parameter_sets.is_empty() {
                    return f.data.clone();
                }
                let mut out = Vec::with_capacity(f.data.len() + 64);
                for ps in &f.parameter_sets {
                    out.extend_from_slice(&(ps.len() as u32).to_be_bytes());
                    out.extend_from_slice(ps);
                }
                out.extend_from_slice(&f.data);
                out
            }
            WireFormat::AnnexB => avcc_to_annex_b(&f.data, &f.parameter_sets, f.is_keyframe),
        }
    }
}

impl core::fmt::Debug for Gfx {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let (width, height) = self.desktop_size.get();
        f.debug_struct("Gfx")
            .field("w", &width)
            .field("h", &height)
            .field("fps", &self.fps)
            .field("bitrate", &self.bitrate_bps)
            .field("keyframe_secs", &self.keyframe_secs)
            .field("wire_format", &self.wire_format)
            .finish()
    }
}

impl ServerEventSender for Gfx {
    fn set_sender(&mut self, sender: mpsc::UnboundedSender<ServerEvent>) {
        *self.sender.lock().unwrap() = Some(sender);
    }
}

impl GfxServerFactory for Gfx {
    fn build_gfx_handler(&self) -> Box<dyn GraphicsPipelineHandler> {
        // We override build_server_with_handle, so this is only a safety stub.
        Box::new(StubHandler)
    }

    fn build_server_with_handle(&self) -> Option<(GfxDvcBridge, GfxServerHandle)> {
        let handler = Box::new(GfxHandler {
            ctx: self.ctx.clone(),
        });
        // A fresh `GraphicsPipelineServer` per connection — its surface-id
        // allocator resets to 0, so every (re)connect creates surface id 0. This
        // marker brackets each connection in the log; on an mstsc reconnect to a
        // still-running macrdp, the id-0 `CreateSurface` no-ops against the
        // surface mstsc retained from the prior session → the reconnect-blank.
        // See the H.264 reconnect quirk note + `on_close` instrumentation below.
        debug!("EGFX: building fresh GraphicsPipelineServer for new connection (surface-id counter resets to 0)");
        let server = GraphicsPipelineServer::new(handler);
        let handle: GfxServerHandle = Arc::new(Mutex::new(server));
        *self.ctx.lock().unwrap() = Some(ConnectionContext {
            server_handle: handle.clone(),
            encoder: None,
            surface_id: None,
            is_ready: false,
            epoch: Instant::now(),
            need_keyframe: true,
            client_supports_avc: false,
            submitted: Arc::new(AtomicU64::new(0)),
            shipped: Arc::new(AtomicU64::new(0)),
            dims: (0, 0),
            last_ack_at: Instant::now(),
            acks_suspended: false,
            last_ship_at: Instant::now(),
            last_recovery_at: Instant::now(),
            last_shipped_frame_id: Arc::new(AtomicU64::new(0)),
            last_acked_frame_id: Arc::new(AtomicU64::new(0)),
            egfx_acks_seen: false,
            last_throttle_ship: Instant::now(),
            demigrated: false,
            adaptive_target_bps: self.bitrate_bps,
            adaptive_last_control: Instant::now(),
            adaptive_last_retransmits: self.congestion_retransmits.load(Ordering::Relaxed),
            idr_backed_off: false,
            adaptive_on_udp: self.egfx_on_udp.load(Ordering::Relaxed),
            adaptive_warmup_until: None,
            adaptive_lag_ewma: 0.0,
            adaptive_congested: false,
            last_floor_fps_pass: Instant::now(),
            qoe_reports: 0,
            qoe_render_seen: false,
            blank_recovery_attempts: 0,
            last_blank_recovery_at: Instant::now(),
        });
        Some((GfxDvcBridge::new(handle.clone()), handle))
    }
}

/// Whether the client's advertised EGFX capabilities indicate AVC420 (H.264)
/// decode support.
///
/// Returns true only on a POSITIVE signal: V8.1 with `AVC420_ENABLED`, or a
/// V10+ capset whose flags lack `AVC_DISABLED`. Bare `V8` / `V10_1` carry no AVC
/// flag and are treated as no-signal (a decoder-less client advertises both of
/// those plus `AVC_DISABLED` on every flagged V10 capset, so it yields false and
/// we fall back to legacy). Verified against three real clients: decoder-less
/// FreeRDP → false; mstsc (V10 without AVC_DISABLED) → true; FreeRDP-with-H.264
/// (V8.1 + AVC420_ENABLED) → true.
fn caps_indicate_avc(caps: &[CapabilitySet]) -> bool {
    caps.iter().any(|c| match c {
        CapabilitySet::V8_1 { flags } => flags.contains(CapabilitiesV81Flags::AVC420_ENABLED),
        CapabilitySet::V10 { flags } | CapabilitySet::V10_2 { flags } => {
            !flags.contains(CapabilitiesV10Flags::AVC_DISABLED)
        }
        CapabilitySet::V10_3 { flags } => !flags.contains(CapabilitiesV103Flags::AVC_DISABLED),
        CapabilitySet::V10_4 { flags }
        | CapabilitySet::V10_5 { flags }
        | CapabilitySet::V10_6 { flags }
        | CapabilitySet::V10_6Err { flags } => !flags.contains(CapabilitiesV104Flags::AVC_DISABLED),
        CapabilitySet::V10_7 { flags } => !flags.contains(CapabilitiesV107Flags::AVC_DISABLED),
        // Bare V8 / V10_1 carry no AVC flag — no positive AVC signal.
        CapabilitySet::V8 { .. } | CapabilitySet::V10_1 => false,
    })
}

/// Per-connection EGFX state callbacks from upstream `GraphicsPipelineServer`.
/// MUST NOT lock `server_handle` from these — the server mutex is already held.
struct GfxHandler {
    ctx: Arc<Mutex<Option<ConnectionContext>>>,
}

impl GraphicsPipelineHandler for GfxHandler {
    /// Effectively unlimited, so the vendored `send_avc420_frame` NEVER drops an
    /// encoded frame on its ack-based backpressure. Dropping an encoded H.264
    /// frame — a P-frame, or worse a keyframe — breaks the decode reference
    /// chain and produces persistent artifacts on the client (observed:
    /// `send_avc420_frame returned None` dropped a 209 KB IDR mid-stream).
    /// Throttling belongs at *capture* (drop-to-latest BEFORE encode, gated by
    /// `--h264-frames-in-flight` in `submit_bgra`), never after encode.
    fn max_frames_in_flight(&self) -> u32 {
        u32::MAX
    }

    fn capabilities_advertise(&mut self, pdu: &CapabilitiesAdvertisePdu) {
        // Upstream split `Vec<CapabilitySet>` into a wire-level
        // `Vec<RawCapabilitySet>` in IronRDP#1305 — typed lookup now
        // requires `.parsed()` per entry. Decode errors or
        // unrecognized versions yield `None` and are filtered out;
        // they carry no positive AVC signal anyway.
        let typed: Vec<CapabilitySet> = pdu
            .0
            .iter()
            .filter_map(|raw| raw.parsed().ok().flatten())
            .collect();
        let supports_avc = caps_indicate_avc(&typed);
        info!(
            count = pdu.0.len(),
            parsed_count = typed.len(),
            supports_avc,
            caps = ?typed,
            "EGFX: client advertised capabilities"
        );
        if let Some(ctx) = self.ctx.lock().unwrap().as_mut() {
            ctx.client_supports_avc = supports_avc;
        }
    }

    fn on_ready(&mut self, negotiated: &CapabilitySet) {
        if let Some(ctx) = self.ctx.lock().unwrap().as_mut() {
            // Only drive the H.264 path if the client advertised AVC420 decode
            // support. Otherwise leave is_ready false → submit_bgra returns
            // Ok(false) → capture.rs uses legacy BitmapUpdate. Shipping AVC420
            // to a non-AVC client gets it rejected (ERROR_NOT_SUPPORTED) and
            // kills the graphics channel.
            if ctx.client_supports_avc {
                ctx.is_ready = true;
                ctx.need_keyframe = true;
                info!(?negotiated, "EGFX channel ready (H.264 active)");
            } else {
                ctx.is_ready = false;
                warn!(
                    ?negotiated,
                    "EGFX client advertised no AVC420 support — falling back to legacy BitmapUpdate"
                );
            }
        }
    }

    fn on_frame_ack(&mut self, frame_id: u32, queue_depth: u32) {
        trace!(frame_id, queue_depth, "EGFX frame ack");
        // Feed ack-driven IDR recovery (EGFX-on-lossy): record liveness, and note
        // whether the client suspended acks (queueDepth == SUSPEND_FRAME_
        // ACKNOWLEDGEMENT 0xFFFFFFFF) — with acks off, loss can't be inferred.
        if let Some(ctx) = self.ctx.lock().unwrap().as_mut() {
            ctx.last_ack_at = Instant::now();
            ctx.acks_suspended = queue_depth == 0xFFFF_FFFF;
            // Record decode progress for the UDP frame-ack-lag backpressure gate.
            // The client sends FrameAcknowledge after it DECODES a frame, so this
            // is the floor of its decode backlog. Only advance on a real ack (not
            // the suspend sentinel), and mark that we've seen at least one ack so
            // `submit_bgra` doesn't false-drop during cold start.
            if !ctx.acks_suspended {
                ctx.last_acked_frame_id
                    .store(u64::from(frame_id), Ordering::Relaxed);
                ctx.egfx_acks_seen = true;
            }
        }
    }

    /// Inbound `RDPGFX_CACHE_IMPORT_OFFER`. Behavior is UNCHANGED from the trait
    /// default (reject all slots → empty reply); logged only. The bitmap cache is
    /// for offscreen bitmaps, not our AVC surface, so it's irrelevant to the
    /// reconnect-blank — capturing whether mstsc even offers a cache at
    /// (re)connect is part of the "no cache-clear PDU" re-audit.
    fn on_cache_import_offer(&mut self, offer: &CacheImportOfferPdu) -> Vec<u16> {
        debug!(
            entries = offer.cache_entries.len(),
            "EGFX on_cache_import_offer (rejecting all — cache is offscreen bitmaps, not the AVC surface)"
        );
        vec![]
    }

    /// Fires when the server allocates a surface (our `create_surface`). Logged
    /// so each connection's surface id + geometry is visible alongside the
    /// `on_close` teardown marker.
    fn on_surface_created(&mut self, surface: &Surface) {
        debug!(
            id = surface.id,
            w = surface.width,
            h = surface.height,
            mapped = surface.is_mapped,
            "EGFX on_surface_created"
        );
    }

    /// Inbound client QoE frame-acknowledge. Feeds the blank-presentation
    /// detector (see [`should_blank_recover`]): `time_diff_dr` is the client's own
    /// decode+render time for the frame — a blank (stale-surface) mstsc session
    /// reports it as 0 on EVERY frame while still decoding/acking normally,
    /// whereas a rendering session shows nonzero values within ~60 ms of the
    /// first frame (pcap-proven 2026-07-02). Runs with the server mutex held
    /// (like `on_frame_ack`), so it only touches ctx — never `server_handle`.
    fn on_qoe_metrics(&mut self, metrics: QoeMetrics) {
        trace!(?metrics, "EGFX on_qoe_metrics");
        if let Some(ctx) = self.ctx.lock().unwrap().as_mut() {
            ctx.qoe_reports = ctx.qoe_reports.saturating_add(1);
            if metrics.time_diff_dr > 0 && !ctx.qoe_render_seen {
                ctx.qoe_render_seen = true;
                debug!(
                    frame_id = metrics.frame_id,
                    time_diff_dr = metrics.time_diff_dr,
                    "EGFX first nonzero QoE decode+render time — client is presenting \
                     (blank detector disarmed for this connection)"
                );
            }
        }
    }

    fn on_close(&mut self) {
        // Disconnect-side instrumentation. This is the EGFX DVC channel close;
        // whether it fires (and how promptly) on a *graceful* mstsc disconnect
        // (Disconnect menu) vs an *abrupt* window-close is the key unmeasured
        // datum for the reconnect-blank investigation — it decides whether a
        // "DeleteSurface before the channel goes away" approach is even reachable.
        // We can only log our own per-connection view here: the
        // `GraphicsPipelineServer` mutex is held while this callback runs, so we
        // must not lock `server_handle`.
        if let Some(ctx) = self.ctx.lock().unwrap().as_mut() {
            debug!(
                surface_id = ?ctx.surface_id,
                dims = ?ctx.dims,
                submitted = ctx.submitted.load(Ordering::Relaxed),
                shipped = ctx.shipped.load(Ordering::Relaxed),
                "EGFX on_close: graphics channel closed (client disconnect/teardown)"
            );
            ctx.is_ready = false;
            ctx.encoder = None;
            ctx.surface_id = None;
            ctx.need_keyframe = true;
        } else {
            debug!("EGFX on_close: graphics channel closed (no active context)");
        }
    }
}

/// Fallback handler for the default `build_gfx_handler` path, which our
/// `build_server_with_handle` override means we never actually hit.
struct StubHandler;

impl GraphicsPipelineHandler for StubHandler {
    fn capabilities_advertise(&mut self, _pdu: &CapabilitiesAdvertisePdu) {}
    fn on_ready(&mut self, _negotiated: &CapabilitySet) {
        warn!("EGFX StubHandler::on_ready — build_server_with_handle should have replaced this");
    }
}

/// Rewrite AVCC (4-byte length-prefixed NALs) to Annex-B (`00 00 00 01` start
/// codes), prepending SPS/PPS on keyframes. Only used when `MACRDP_H264_ANNEXB`
/// selects Annex-B framing.
fn avcc_to_annex_b(avcc: &[u8], parameter_sets: &[Vec<u8>], is_keyframe: bool) -> Vec<u8> {
    const START_CODE: [u8; 4] = [0, 0, 0, 1];
    let mut out = Vec::with_capacity(avcc.len() + 64);

    if is_keyframe {
        for ps in parameter_sets {
            out.extend_from_slice(&START_CODE);
            out.extend_from_slice(ps);
        }
    }

    let mut i = 0;
    while i + 4 <= avcc.len() {
        let nal_len = u32::from_be_bytes([avcc[i], avcc[i + 1], avcc[i + 2], avcc[i + 3]]) as usize;
        i += 4;
        if i + nal_len > avcc.len() {
            warn!(
                avcc_len = avcc.len(),
                offset = i,
                nal_len,
                "AVCC NAL length overflows buffer; truncating"
            );
            break;
        }
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(&avcc[i..i + nal_len]);
        i += nal_len;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recovery_params() -> RecoveryParams {
        RecoveryParams {
            active_window: Duration::from_millis(500),
            ack_stall: Duration::from_millis(200),
            min_recovery_interval: Duration::from_millis(1000),
        }
    }

    // Convenience: build a Duration in ms for the table below.
    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    #[test]
    fn recovery_fires_on_ack_stall_while_shipping_on_lossy() {
        let p = recovery_params();
        // Actively shipping (30ms), acks silent (300ms > 200), past rate-limit
        // (5s), acks not suspended, EGFX on the lossy tunnel → force a recovery IDR.
        assert!(should_force_recovery_idr(
            ms(30),
            ms(300),
            ms(5000),
            false,
            true,
            &p
        ));
    }

    #[test]
    fn recovery_suppressed_when_acks_suspended() {
        let p = recovery_params();
        // queueDepth==0xFFFFFFFF → acks_suspended: loss can't be inferred.
        assert!(!should_force_recovery_idr(
            ms(30),
            ms(300),
            ms(5000),
            true,
            true,
            &p
        ));
    }

    #[test]
    fn recovery_never_on_reliable_or_tcp() {
        let p = recovery_params();
        // egfx_on_lossy=false (TCP / reliable tunnel): a missing ack is congestion,
        // not loss — an IDR would worsen it, so never fire.
        assert!(!should_force_recovery_idr(
            ms(30),
            ms(300),
            ms(5000),
            false,
            false,
            &p
        ));
    }

    #[test]
    fn recovery_not_when_acks_fresh() {
        let p = recovery_params();
        // since_ack (50ms) below ack_stall (200ms): acks still flowing, no loss.
        assert!(!should_force_recovery_idr(
            ms(30),
            ms(50),
            ms(5000),
            false,
            true,
            &p
        ));
    }

    #[test]
    fn recovery_not_when_idle_not_shipping() {
        let p = recovery_params();
        // since_ship (2s) above active_window (500ms): static screen, nothing to
        // lose — the periodic IDR backstops; don't force.
        assert!(!should_force_recovery_idr(
            ms(2000),
            ms(300),
            ms(5000),
            false,
            true,
            &p
        ));
    }

    #[test]
    fn recovery_rate_limited() {
        let p = recovery_params();
        // since_recovery (200ms) below min_recovery_interval (1000ms): just forced
        // one; don't storm IDRs even if acks are still silent.
        assert!(!should_force_recovery_idr(
            ms(30),
            ms(300),
            ms(200),
            false,
            true,
            &p
        ));
    }

    #[test]
    fn recovery_thresholds_are_inclusive() {
        let p = recovery_params();
        // Exactly at the boundaries: since_ship == active_window (<=), since_ack ==
        // ack_stall (>=), since_recovery == min_recovery_interval (>=) → fires.
        assert!(should_force_recovery_idr(
            ms(500),
            ms(200),
            ms(1000),
            false,
            true,
            &p
        ));
    }

    // ---- EGFX-over-UDP → TCP watchdog (`should_demigrate_to_tcp`) ----
    // Arg order: since_ship, since_ack, acks_suspended, egfx_on_udp, egfx_on_lossy,
    // already_demigrated, active_window, wedge_timeout.
    const WD_ACTIVE: Duration = Duration::from_millis(1000);
    const WD_WEDGE: Duration = Duration::from_millis(3000);

    #[test]
    fn watchdog_fires_on_reliable_udp_wedge_while_shipping() {
        // Reliable UDP (on_udp && !on_lossy), actively shipping (30ms), acks silent
        // past the wedge timeout (4s > 3s), not suspended, not yet de-migrated → fire.
        assert!(should_demigrate_to_tcp(
            ms(30),
            ms(4000),
            false,
            true,
            false,
            false,
            WD_ACTIVE,
            WD_WEDGE
        ));
    }

    #[test]
    fn watchdog_never_on_tcp() {
        // egfx_on_udp=false (plain TCP): socket backpressure paces us; nothing to do.
        assert!(!should_demigrate_to_tcp(
            ms(30),
            ms(4000),
            false,
            false,
            false,
            false,
            WD_ACTIVE,
            WD_WEDGE
        ));
    }

    #[test]
    fn watchdog_never_on_lossy_tunnel() {
        // egfx_on_lossy=true: the lossy tunnel uses ack-driven IDR recovery, not
        // de-migration (a dropped frame there is real loss, not a wedge).
        assert!(!should_demigrate_to_tcp(
            ms(30),
            ms(4000),
            false,
            true,
            true,
            false,
            WD_ACTIVE,
            WD_WEDGE
        ));
    }

    #[test]
    fn watchdog_suppressed_when_acks_suspended() {
        // queueDepth==0xFFFFFFFF → a wedge can't be inferred from ack-staleness.
        assert!(!should_demigrate_to_tcp(
            ms(30),
            ms(4000),
            true,
            true,
            false,
            false,
            WD_ACTIVE,
            WD_WEDGE
        ));
    }

    #[test]
    fn watchdog_latches_one_way() {
        // already_demigrated=true: fire once per connection, never flap back.
        assert!(!should_demigrate_to_tcp(
            ms(30),
            ms(4000),
            false,
            true,
            false,
            true,
            WD_ACTIVE,
            WD_WEDGE
        ));
    }

    #[test]
    fn watchdog_not_when_acks_fresh() {
        // since_ack (500ms) below the wedge timeout (3s): acks still flowing.
        assert!(!should_demigrate_to_tcp(
            ms(30),
            ms(500),
            false,
            true,
            false,
            false,
            WD_ACTIVE,
            WD_WEDGE
        ));
    }

    #[test]
    fn watchdog_not_when_static_screen() {
        // since_ship (2s) above active_window (1s): the screen went static, so silent
        // acks are normal — not a wedge. Heals on the next activity if still wedged.
        assert!(!should_demigrate_to_tcp(
            ms(2000),
            ms(4000),
            false,
            true,
            false,
            false,
            WD_ACTIVE,
            WD_WEDGE
        ));
    }

    #[test]
    fn watchdog_thresholds_are_inclusive() {
        // since_ship == active_window (<=), since_ack == wedge_timeout (>=) → fires.
        assert!(should_demigrate_to_tcp(
            ms(1000),
            ms(3000),
            false,
            true,
            false,
            false,
            WD_ACTIVE,
            WD_WEDGE
        ));
    }

    // ---- adaptive bitrate AIMD (`aimd_bitrate`) ----
    // (current, loss_delta, floor, ceiling, increase, decrease)
    #[test]
    fn aimd_decreases_multiplicatively_on_loss() {
        // 10 Mbps, loss this interval, 0.7 factor → 7 Mbps (above the 1 Mbps floor).
        assert_eq!(
            aimd_bitrate(10_000_000, 3, 1_000_000, 10_000_000, 500_000, 0.7),
            7_000_000
        );
    }

    #[test]
    fn aimd_decrease_clamps_to_floor() {
        // Already near the floor; a further cut can't go below it.
        assert_eq!(
            aimd_bitrate(1_200_000, 5, 1_000_000, 10_000_000, 500_000, 0.7),
            1_000_000
        );
    }

    #[test]
    fn aimd_increases_additively_when_clean() {
        // No loss → climb by the step.
        assert_eq!(
            aimd_bitrate(5_000_000, 0, 1_000_000, 10_000_000, 500_000, 0.7),
            5_500_000
        );
    }

    #[test]
    fn aimd_increase_clamps_to_ceiling() {
        // Near the ceiling; the additive step can't exceed it.
        assert_eq!(
            aimd_bitrate(9_800_000, 0, 1_000_000, 10_000_000, 500_000, 0.7),
            10_000_000
        );
    }

    #[test]
    fn aimd_at_ceiling_clean_is_stable() {
        // At the ceiling on a clean interval → unchanged (caller treats == as no-op).
        assert_eq!(
            aimd_bitrate(10_000_000, 0, 1_000_000, 10_000_000, 500_000, 0.7),
            10_000_000
        );
    }

    #[test]
    fn aimd_at_floor_with_loss_is_stable() {
        // At the floor under continued loss → stays at the floor (choppy-but-alive).
        assert_eq!(
            aimd_bitrate(1_000_000, 9, 1_000_000, 10_000_000, 500_000, 0.7),
            1_000_000
        );
    }

    // ---- IDR backoff (`idr_backoff_decision`) ----
    // (loss, new_target, ceiling, backed_off)
    #[test]
    fn idr_stretches_when_congestion_starts() {
        // Loss begins, not yet backed off → suppress the periodic IDR.
        assert_eq!(
            idr_backoff_decision(true, 7_000_000, 10_000_000, false),
            IdrBackoff::Stretch
        );
    }

    #[test]
    fn idr_holds_while_already_backed_off_under_loss() {
        // Still losing, already suppressed → nothing to change.
        assert_eq!(
            idr_backoff_decision(true, 5_000_000, 10_000_000, true),
            IdrBackoff::Hold
        );
    }

    #[test]
    fn idr_holds_during_clean_climbback_below_ceiling() {
        // Recovering (no loss) but not yet at the ceiling → keep suppressed (don't
        // inject a keyframe mid-recovery).
        assert_eq!(
            idr_backoff_decision(false, 8_000_000, 10_000_000, true),
            IdrBackoff::Hold
        );
    }

    #[test]
    fn idr_restores_on_full_recovery() {
        // Clean AND back at the ceiling while backed off → restore + recovery IDR.
        assert_eq!(
            idr_backoff_decision(false, 10_000_000, 10_000_000, true),
            IdrBackoff::Restore
        );
    }

    #[test]
    fn idr_no_op_when_never_backed_off() {
        // Clean at ceiling but never suppressed → nothing to do.
        assert_eq!(
            idr_backoff_decision(false, 10_000_000, 10_000_000, false),
            IdrBackoff::Hold
        );
    }

    // retransmit_is_lossy(retransmit_delta, tolerance)
    #[test]
    fn retransmit_tolerance_ignores_sporadic_wireless_loss() {
        // Default tolerance 2: up to 2 retransmits/interval is background wireless loss,
        // NOT congestion — so a sporadic single/double retransmit no longer ratchets the
        // bitrate down, and the controller can still climb under it.
        assert!(!retransmit_is_lossy(0, 2));
        assert!(!retransmit_is_lossy(1, 2));
        assert!(!retransmit_is_lossy(2, 2)); // at tolerance → still tolerated
        assert!(retransmit_is_lossy(3, 2)); // above tolerance → real loss → back off
                                            // tolerance 0 restores the old "any retransmit = loss" behaviour.
        assert!(!retransmit_is_lossy(0, 0));
        assert!(retransmit_is_lossy(1, 0));
    }

    // congested_hysteresis(ewma_lag, high, low, retransmit_lossy, acks_usable, currently)
    #[test]
    fn hysteresis_enters_only_above_high() {
        // Not yet congested: enter only once the smoothed lag clears the HIGH mark.
        assert!(!congested_hysteresis(10.0, 12.0, 6.0, false, true, false)); // below high
        assert!(!congested_hysteresis(12.0, 12.0, 6.0, false, true, false)); // exactly at
        assert!(congested_hysteresis(13.0, 12.0, 6.0, false, true, false)); // above high
    }

    #[test]
    fn hysteresis_stays_until_below_low() {
        // Already congested: stay through the (low, high] band, exit at or below low.
        assert!(congested_hysteresis(9.0, 12.0, 6.0, false, true, true)); // in the band → stay
        assert!(congested_hysteresis(7.0, 12.0, 6.0, false, true, true)); // just above low → stay
        assert!(!congested_hysteresis(6.0, 12.0, 6.0, false, true, true)); // at low → exit
        assert!(!congested_hysteresis(5.0, 12.0, 6.0, false, true, true)); // below low → exit
    }

    #[test]
    fn hysteresis_retransmit_forces_congested() {
        // Loss above the tolerance is a definite loss → congested regardless of lag/acks.
        assert!(congested_hysteresis(0.0, 12.0, 6.0, true, true, false));
        assert!(congested_hysteresis(0.0, 12.0, 6.0, true, false, false)); // even acks-unusable
    }

    #[test]
    fn hysteresis_ignores_lag_when_acks_unusable() {
        // Acks suspended / cold-start: lag uninferable → not congested (no retransmit),
        // and a stale-high EWMA can't keep an episode latched.
        assert!(!congested_hysteresis(
            10_000.0, 12.0, 6.0, false, false, false
        ));
        assert!(!congested_hysteresis(
            10_000.0, 12.0, 6.0, false, false, true
        ));
    }

    #[test]
    fn hysteresis_transport_specific_high() {
        // Same smoothed lag enters congestion at the UDP high (8) but not the TCP high
        // (12) — TCP's send buffer adds baseline depth, so it needs a higher mark.
        assert!(congested_hysteresis(10.0, 8.0, 4.0, false, true, false)); // UDP
        assert!(!congested_hysteresis(10.0, 12.0, 6.0, false, true, false)); // TCP
    }

    // rate_action(ewma_lag, high, retransmit_lossy, acks_usable, congested)
    #[test]
    fn rate_action_decreases_above_high_or_on_retransmit() {
        assert_eq!(
            rate_action(13.0, 12.0, false, true, true),
            RateAction::Decrease
        ); // lag>high
        assert_eq!(
            rate_action(0.0, 12.0, true, true, true),
            RateAction::Decrease
        ); // loss>tolerance
        assert_eq!(
            rate_action(0.0, 12.0, true, false, false),
            RateAction::Decrease
        ); // loss>tolerance, acks unusable
    }

    #[test]
    fn rate_action_holds_in_band_while_congested() {
        // Latched congested but the smoothed lag has decayed below high (decaying through
        // the band) → hold, don't keep cratering. This is the "single spike" fix.
        assert_eq!(rate_action(9.0, 12.0, false, true, true), RateAction::Hold);
    }

    #[test]
    fn rate_action_increases_when_cleared() {
        // Not congested (cleared below low) → climb back toward the ceiling.
        assert_eq!(
            rate_action(2.0, 12.0, false, true, false),
            RateAction::Increase
        );
        // Acks unusable (warmup/suspend) with no retransmit → not congested → increase
        // (target is already at ceiling at connect, so this is a clamp-no-op there).
        assert_eq!(
            rate_action(99.0, 12.0, false, false, false),
            RateAction::Increase
        );
    }

    #[test]
    fn rate_action_climbs_under_tolerated_wireless_loss() {
        // The WiFi-ratchet fix end-to-end: low background loss (delta=1) is below the
        // default tolerance → retransmit_lossy=false → with a low smoothed lag the
        // controller still INCREASES instead of being pinned in decrease.
        let lossy = retransmit_is_lossy(1, 2);
        assert!(!lossy);
        assert_eq!(
            rate_action(1.0, 8.0, lossy, true, false),
            RateAction::Increase
        );
        // But sustained loss (delta=5) crosses the tolerance → back off.
        let lossy = retransmit_is_lossy(5, 2);
        assert!(lossy);
        assert_eq!(
            rate_action(1.0, 8.0, lossy, true, true),
            RateAction::Decrease
        );
    }

    // ---- Blank-presentation detector (reconnect-blank resize dance) ----

    fn blank_params() -> BlankRecoveryParams {
        BlankRecoveryParams {
            min_qoe_reports: 120,
            arm_delay: ms(3000),
            retry_interval: ms(5000),
            max_attempts: 2,
        }
    }

    #[test]
    fn blank_recovery_fires_on_the_pcap_signature() {
        // The captured blank session: QoE reports flowing (well past the window),
        // zero render-time ever, regular acks alive, connection well past arm.
        let p = blank_params();
        assert!(should_blank_recover(
            131,
            false,
            true,
            false,
            ms(10_000),
            ms(10_000),
            0,
            &p
        ));
    }

    #[test]
    fn blank_recovery_disarmed_by_a_single_nonzero_render_time() {
        // A rendering session shows nonzero EDR within ~60 ms of the first frame —
        // one report disarms the detector no matter what else holds.
        let p = blank_params();
        assert!(!should_blank_recover(
            10_000,
            true,
            true,
            false,
            ms(60_000),
            ms(60_000),
            0,
            &p
        ));
    }

    #[test]
    fn blank_recovery_needs_a_full_evidence_window() {
        // Too few QoE reports (also covers non-QoE clients: FreeRDP sends none,
        // so the count never reaches the window and the dance never fires).
        let p = blank_params();
        assert!(!should_blank_recover(
            119,
            false,
            true,
            false,
            ms(10_000),
            ms(10_000),
            0,
            &p
        ));
        assert!(!should_blank_recover(
            0,
            false,
            true,
            false,
            ms(10_000),
            ms(10_000),
            0,
            &p
        ));
    }

    #[test]
    fn blank_recovery_requires_live_unsuspended_acks() {
        let p = blank_params();
        // No regular FrameAcks seen → not the blank signature.
        assert!(!should_blank_recover(
            200,
            false,
            false,
            false,
            ms(10_000),
            ms(10_000),
            0,
            &p
        ));
        // Acks suspended (queueDepth sentinel) → congestion territory, not blank.
        assert!(!should_blank_recover(
            200,
            false,
            true,
            true,
            ms(10_000),
            ms(10_000),
            0,
            &p
        ));
    }

    #[test]
    fn blank_recovery_respects_arm_delay_spacing_and_attempt_cap() {
        let p = blank_params();
        // Inside the connect-time arm delay → hold.
        assert!(!should_blank_recover(
            200,
            false,
            true,
            false,
            ms(2999),
            ms(2999),
            0,
            &p
        ));
        // Too soon after the previous dance → hold.
        assert!(!should_blank_recover(
            200,
            false,
            true,
            false,
            ms(10_000),
            ms(4999),
            1,
            &p
        ));
        // Attempts exhausted → give up (client-side floor).
        assert!(!should_blank_recover(
            200,
            false,
            true,
            false,
            ms(60_000),
            ms(60_000),
            2,
            &p
        ));
        // Second attempt inside the cap, past the spacing → fires.
        assert!(should_blank_recover(
            200,
            false,
            true,
            false,
            ms(10_000),
            ms(5000),
            1,
            &p
        ));
    }

    #[test]
    fn blank_action_remaps_then_drops() {
        // Default max_attempts=2: attempt 1 = the non-destructive remap,
        // attempt 2 (last) = drop the connection.
        assert_eq!(blank_action(1, 2), BlankAction::Remap);
        assert_eq!(blank_action(2, 2), BlankAction::Drop);
        // A tuned max_attempts=3 gets two remaps before the drop.
        assert_eq!(blank_action(1, 3), BlankAction::Remap);
        assert_eq!(blank_action(2, 3), BlankAction::Remap);
        assert_eq!(blank_action(3, 3), BlankAction::Drop);
        // max_attempts=1: straight to the drop.
        assert_eq!(blank_action(1, 1), BlankAction::Drop);
    }

    // ---- P2b: frame_drop_at_floor ----

    #[test]
    fn frame_drop_at_floor_only_when_at_floor_and_congested() {
        let min = Duration::from_millis(100);
        let soon = Duration::from_millis(10); // inside the min-fps spacing
                                              // Both conditions + too-soon → drop.
        assert!(frame_drop_at_floor(true, true, soon, min));
        // Not at floor → never drops (bitrate cuts are still the right lever).
        assert!(!frame_drop_at_floor(false, true, soon, min));
        // Not congested → never drops (link is fine).
        assert!(!frame_drop_at_floor(true, false, soon, min));
        // Neither → never drops.
        assert!(!frame_drop_at_floor(false, false, soon, min));
    }

    #[test]
    fn frame_drop_at_floor_respects_min_fps_spacing() {
        let min = Duration::from_millis(100);
        // A capture that arrives after the spacing elapsed is let through (never zero).
        assert!(!frame_drop_at_floor(
            true,
            true,
            Duration::from_millis(100),
            min
        ));
        assert!(!frame_drop_at_floor(
            true,
            true,
            Duration::from_millis(250),
            min
        ));
        // One arriving sooner is dropped — capping the effective fps.
        assert!(frame_drop_at_floor(
            true,
            true,
            Duration::from_millis(99),
            min
        ));
        assert!(frame_drop_at_floor(true, true, Duration::ZERO, min));
    }

    #[test]
    fn avcc_to_annex_b_rewrites_length_prefixes() {
        let mut avcc = Vec::new();
        avcc.extend_from_slice(&3u32.to_be_bytes());
        avcc.extend_from_slice(&[0xAA, 0xAA, 0xAA]);
        avcc.extend_from_slice(&5u32.to_be_bytes());
        avcc.extend_from_slice(&[0xBB, 0xBB, 0xBB, 0xBB, 0xBB]);
        let out = avcc_to_annex_b(&avcc, &[], false);
        let expected: Vec<u8> = [
            0, 0, 0, 1, 0xAA, 0xAA, 0xAA, 0, 0, 0, 1, 0xBB, 0xBB, 0xBB, 0xBB, 0xBB,
        ]
        .into();
        assert_eq!(out, expected);
    }

    #[test]
    fn avcc_to_annex_b_prepends_parameter_sets_on_keyframe() {
        let sps = vec![0x67, 0x42, 0x00];
        let pps = vec![0x68, 0xCE, 0x06];
        let avcc = {
            let mut v = Vec::new();
            v.extend_from_slice(&2u32.to_be_bytes());
            v.extend_from_slice(&[0x65, 0x88]);
            v
        };
        let out = avcc_to_annex_b(&avcc, &[sps.clone(), pps.clone()], true);
        assert_eq!(&out[0..4], &[0, 0, 0, 1]);
        assert_eq!(&out[4..7], sps.as_slice());
        assert_eq!(&out[7..11], &[0, 0, 0, 1]);
        assert_eq!(&out[11..14], pps.as_slice());
        assert_eq!(&out[14..18], &[0, 0, 0, 1]);
        assert_eq!(&out[18..20], &[0x65, 0x88]);
    }
}
