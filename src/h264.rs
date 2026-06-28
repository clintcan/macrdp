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
    /// is on a UDP tunnel, so it's a no-op on TCP. The controller (AIMD) reads the
    /// shared `congestion_retransmits` loss counter the UDP listener bumps and live-
    /// adjusts the VideoToolbox bitrate within `[adaptive_floor_bps, bitrate_bps]`:
    /// multiplicative-decrease `adaptive_decrease` per interval with loss, additive-
    /// increase `adaptive_increase_bps` per interval when clean. `bitrate_bps` (the
    /// `--bitrate` value) is the ceiling. See [`Gfx::adaptive_bitrate_step`].
    adaptive_enabled: bool,
    adaptive_floor_bps: u32,
    adaptive_increase_bps: u32,
    adaptive_decrease: f32,
    adaptive_interval: Duration,
    /// Cumulative reliable-tunnel retransmit count, bumped by the UDP listener and
    /// sampled (as deltas) by the controller. Shared `Arc` like the egfx flags.
    congestion_retransmits: Arc<AtomicU64>,
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
                "EGFX adaptive bitrate ENABLED (--adaptive-bitrate) — congestion-responsive \
                 rate control, active only while EGFX is on a UDP tunnel"
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
            congestion_retransmits,
        }
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

        // Submit to VideoToolbox (async). The ship thread delivers + ships the
        // output; we just count the submission for the drop-to-latest throttle.
        {
            let mut guard = self.ctx.lock().unwrap();
            let Some(ctx) = guard.as_mut() else {
                return Ok(true);
            };
            // Congestion-responsive bitrate (P1): compute the new target (mutates
            // ctx adaptive state) BEFORE borrowing the encoder, then apply it live.
            // No-op unless adaptive is enabled AND EGFX is on a UDP tunnel.
            let new_bitrate = self.adaptive_bitrate_step(ctx);
            let Some(encoder) = ctx.encoder.as_mut() else {
                return Ok(true);
            };
            if let Some(bps) = new_bitrate {
                if let Err(e) = encoder.set_bitrate(bps) {
                    trace!(error = ?e, bps, "adaptive set_bitrate failed");
                }
            }
            encoder.encode_bgra(bgra, stride, force_keyframe)?;
            ctx.submitted.fetch_add(1, Ordering::Relaxed);
        }
        Ok(true)
    }

    /// Congestion-responsive bitrate controller (P1, AIMD). Called once per capture
    /// from `submit_bgra` while holding the ctx lock; rate-limited to one step per
    /// `adaptive_interval`. Reads the shared cumulative-retransmit loss counter the
    /// UDP listener bumps and, per interval: **multiplicative-decrease** the target
    /// toward `adaptive_floor_bps` if any loss occurred, else **additive-increase**
    /// toward the `bitrate_bps` ceiling. Returns `Some(bps)` when the target changed
    /// (the caller live-sets it on the VT session). No-op (returns `None`) unless
    /// the feature is enabled and EGFX is on a UDP tunnel — so TCP stays byte-identical.
    fn adaptive_bitrate_step(&self, ctx: &mut ConnectionContext) -> Option<u32> {
        if !self.adaptive_enabled || !self.egfx_on_udp.load(Ordering::Relaxed) {
            return None;
        }
        let now = Instant::now();
        if now.duration_since(ctx.adaptive_last_control) < self.adaptive_interval {
            return None;
        }
        ctx.adaptive_last_control = now;
        let cur = self.congestion_retransmits.load(Ordering::Relaxed);
        let delta = cur.saturating_sub(ctx.adaptive_last_retransmits);
        ctx.adaptive_last_retransmits = cur;
        let new_target = aimd_bitrate(
            ctx.adaptive_target_bps,
            delta,
            self.adaptive_floor_bps,
            self.bitrate_bps.max(1),
            self.adaptive_increase_bps,
            self.adaptive_decrease,
        );
        if new_target != ctx.adaptive_target_bps {
            let prev = ctx.adaptive_target_bps;
            ctx.adaptive_target_bps = new_target;
            debug!(
                loss_delta = delta,
                prev_bps = prev,
                new_bps = new_target,
                "EGFX adaptive bitrate adjusted"
            );
            Some(new_target)
        } else {
            None
        }
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

    /// Inbound client QoE frame-acknowledge — a client-liveness signal. Logged at
    /// trace so a graceful-disconnect capture can show whether the client keeps
    /// sending QoE up to the channel close.
    fn on_qoe_metrics(&mut self, metrics: QoeMetrics) {
        trace!(?metrics, "EGFX on_qoe_metrics");
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
