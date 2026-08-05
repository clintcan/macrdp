use core::cell::RefCell;
use core::net::{IpAddr, SocketAddr};
use core::time::Duration;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Instant;

use anyhow::{Context as _, Result, bail};
use ironrdp_acceptor::{Acceptor, AcceptorResult, BeginResult, DesktopSize};
use ironrdp_async::Framed;
use ironrdp_cliprdr::CliprdrServer;
use ironrdp_cliprdr::backend::ClipboardMessage;
use ironrdp_core::{decode, encode_vec, impl_as_any};
use ironrdp_displaycontrol::pdu::DisplayControlMonitorLayout;
use ironrdp_displaycontrol::server::{DisplayControlHandler, DisplayControlServer};
use ironrdp_pdu::input::InputEventPdu;
use ironrdp_pdu::input::fast_path::{FastPathInput, FastPathInputEvent};
use ironrdp_pdu::mcs::{SendDataIndication, SendDataRequest};
use ironrdp_pdu::rdp::capability_sets::{BitmapCodecs, CapabilitySet, CmdFlags, CodecProperty, GeneralExtraFlags};
pub use ironrdp_pdu::rdp::client_info::Credentials;
use ironrdp_pdu::rdp::headers::{ServerDeactivateAll, ShareControlPdu};
use ironrdp_pdu::x224::X224;
use ironrdp_pdu::{Action, PduResult, decode_err, mcs, nego, rdp};
use ironrdp_svc::{ChannelFlags, StaticChannelId, StaticChannelSet, SvcProcessor, server_encode_svc_messages};
use ironrdp_tokio::{FramedRead, FramedWrite, TokioFramed, split_tokio_framed, unsplit_tokio_framed};
use rdpsnd::server::{RdpsndServer, RdpsndServerMessage};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt as _};
use tokio::net::{TcpSocket, TcpStream};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, error, info, trace, warn};
use {ironrdp_dvc as dvc, ironrdp_rdpsnd as rdpsnd};

use crate::autodetect::{AutoDetectManager, RttSnapshot};
use crate::clipboard::CliprdrServerFactory;
use crate::display::{DisplayUpdate, RdpServerDisplay};
use crate::echo::{EchoDvcBridge, EchoServerHandle, EchoServerMessage, build_echo_request};
use crate::encoder::{UpdateEncoder, UpdateEncoderCodecs};
#[cfg(feature = "egfx")]
use crate::gfx::{EgfxServerMessage, GfxServerFactory};
use crate::handler::RdpServerInputHandler;
use crate::{SoundServerFactory, builder, capabilities};

/// TCP listen backlog size for the RDP server socket.
const LISTENER_BACKLOG: u32 = 1024;

/// (vendored, divergence 23) How long an evicted session gets to send its
/// `ERRINFO_DISCONNECTED_BY_OTHERCONNECTION` and wind down on its own before
/// it's cancelled outright. Short: the preempting client is already
/// authenticated and waiting, and a half-dead peer must not stall the
/// takeover. Exceeding it is not an error — it just degrades to the hard
/// cancellation this grace period replaced.
const EVICTION_GRACE: Duration = Duration::from_millis(750);

/// (vendored, divergence 23) How long a just-evicted peer is barred from
/// preempting its way back in — see [`RdpServer::recently_evicted`]. Sized to
/// sit comfortably above a client's automatic reconnect cadence (~1 s) and
/// comfortably below the time a human takes to close a window and reconnect,
/// so it filters retry storms without blocking a deliberate retake.
const REPREEMPT_COOLDOWN: Duration = Duration::from_secs(5);

/// (vendored, divergence 22) What resolved first while a session was live: the
/// session itself ending, a new inbound connection, or the verdict on a
/// previously accepted candidate. The `select!` yields one of these and does
/// NOT mutate the probe slot — its futures still borrow it inside the select
/// expression.
// Short-lived select!-race enum — one is produced per loop iteration and consumed
// immediately, so boxing the large variant would only add an accept-path allocation.
#[allow(clippy::large_enum_variant)]
enum PreemptRace {
    Ended(Result<()>),
    Accepted(std::io::Result<(tokio::net::TcpStream, SocketAddr)>),
    /// The outcome of `negotiate_candidate` for whichever peer is currently
    /// being probed — see `probing_peer` at the call site for why the peer
    /// itself isn't carried here.
    Probed(Option<NegotiatedCandidate>),
}

/// Action to take after a client disconnects.
///
/// Returned by [`ConnectionHandler::on_disconnected`] to control whether
/// the server continues accepting new connections or shuts down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostConnectionAction {
    /// Continue accepting new connections.
    Continue,
    /// Stop the accept loop and return from [`RdpServer::run`].
    Stop,
}

/// Hooks for connection lifecycle events in [`RdpServer::run`].
///
/// Implement this trait to add pre-accept filtering (rate limiting,
/// IP allowlists) and post-disconnect logic (cleanup, session validity
/// checks, metrics).
///
/// All methods have default implementations that accept all connections
/// and continue unconditionally.
pub trait ConnectionHandler: Send {
    /// Called after `accept()` returns but before `run_connection()`.
    ///
    /// Return `false` to reject the connection (the TCP stream is dropped).
    fn on_accept(&mut self, peer: SocketAddr) -> bool {
        let _ = peer;
        true
    }

    /// Called after `run_connection()` completes (successfully or with error).
    ///
    /// `duration` is the wall-clock time the connection was active.
    /// `error` is `Some` if the connection ended with an error.
    fn on_disconnected(
        &mut self,
        peer: SocketAddr,
        duration: Duration,
        error: Option<&anyhow::Error>,
    ) -> PostConnectionAction {
        let _ = (peer, duration, error);
        PostConnectionAction::Continue
    }

    /// Called once per connection after the CredSSP/NLA exchange resolves (only
    /// under Hybrid security). `success` is whether the client's credentials
    /// validated; `reason` is a short error description when they did not (auth
    /// did not complete — dominated by bad credentials, but also a client abort
    /// or a mid-exchange transport error). Correlate with [`Self::on_accept`],
    /// which runs immediately before, for the peer. Default: no-op.
    fn on_authenticated(&mut self, success: bool, reason: Option<&str>) {
        let _ = (success, reason);
    }

    /// (vendored, divergence (20)) Called once per connection when the
    /// capability exchange completes (not on a deactivation–reactivation),
    /// carrying the client-fingerprint identity: the GCC Client Core Data
    /// hostname / raw RDP version / build number plus the General-capset
    /// platform (formatted). Informational fingerprinting — a client can
    /// claim anything. Default: no-op.
    fn on_client_fingerprint(&mut self, client_name: &str, rdp_version: u32, client_build: u32, platform: &str) {
        let _ = (client_name, rdp_version, client_build, platform);
    }
}

#[derive(Clone)]
pub struct RdpServerOptions {
    pub addr: SocketAddr,
    pub security: RdpServerSecurity,
    pub codecs: BitmapCodecs,
    pub max_request_size: u32,
}

impl RdpServerOptions {
    /// Default [MultifragmentUpdate] max reassembly buffer size (8 MB).
    ///
    /// Advertised to the client during capability exchange as the largest
    /// reassembled Fast-Path Update the server can accept.
    /// Values that are too large cause certain clients (notably mstsc)
    /// to reject the connection.
    ///
    /// [MultifragmentUpdate]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/01717954-716a-424d-af35-28fb2b86df89
    pub(crate) const DEFAULT_MAX_REQUEST_SIZE: u32 = 8 * 1024 * 1024;

    fn has_image_remote_fx(&self) -> bool {
        self.codecs
            .0
            .iter()
            .any(|codec| matches!(codec.property, CodecProperty::ImageRemoteFx(_)))
    }

    fn has_remote_fx(&self) -> bool {
        self.codecs
            .0
            .iter()
            .any(|codec| matches!(codec.property, CodecProperty::RemoteFx(_)))
    }

    #[cfg(feature = "qoi")]
    fn has_qoi(&self) -> bool {
        self.codecs
            .0
            .iter()
            .any(|codec| matches!(codec.property, CodecProperty::Qoi))
    }

    #[cfg(feature = "qoiz")]
    fn has_qoiz(&self) -> bool {
        self.codecs
            .0
            .iter()
            .any(|codec| matches!(codec.property, CodecProperty::QoiZ))
    }

    fn has_nscodec(&self) -> bool {
        self.codecs
            .0
            .iter()
            .any(|codec| matches!(codec.property, CodecProperty::NsCodec(_)))
    }
}

#[derive(Clone)]
pub enum RdpServerSecurity {
    None,
    Tls(TlsAcceptor),
    /// Used for both hybrid + hybrid-ex.
    Hybrid((TlsAcceptor, Vec<u8>)),
}

impl RdpServerSecurity {
    pub fn flag(&self) -> nego::SecurityProtocol {
        match self {
            RdpServerSecurity::None => nego::SecurityProtocol::empty(),
            RdpServerSecurity::Tls(_) => nego::SecurityProtocol::SSL,
            RdpServerSecurity::Hybrid(_) => nego::SecurityProtocol::HYBRID | nego::SecurityProtocol::HYBRID_EX,
        }
    }
}

struct AInputHandler {
    handler: Arc<Mutex<Box<dyn RdpServerInputHandler>>>,
}

impl_as_any!(AInputHandler);

impl dvc::DvcProcessor for AInputHandler {
    fn channel_name(&self) -> &str {
        ironrdp_ainput::CHANNEL_NAME
    }

    fn start(&mut self, _channel_id: u32) -> PduResult<Vec<dvc::DvcMessage>> {
        use ironrdp_ainput::{ServerPdu, VersionPdu};

        let pdu = ServerPdu::Version(VersionPdu::default());

        Ok(vec![Box::new(pdu)])
    }

    fn close(&mut self, _channel_id: u32) {}

    fn process(&mut self, _channel_id: u32, payload: &[u8]) -> PduResult<Vec<dvc::DvcMessage>> {
        use ironrdp_ainput::ClientPdu;

        match decode(payload).map_err(|e| decode_err!(e))? {
            ClientPdu::Mouse(pdu) => {
                let handler = Arc::clone(&self.handler);
                task::spawn_blocking(move || {
                    handler.blocking_lock().mouse(pdu.into());
                });
            }
        }

        Ok(Vec::new())
    }
}

impl dvc::DvcServerProcessor for AInputHandler {}

struct DisplayControlBackend {
    display: Arc<Mutex<Box<dyn RdpServerDisplay>>>,
}

impl DisplayControlBackend {
    fn new(display: Arc<Mutex<Box<dyn RdpServerDisplay>>>) -> Self {
        Self { display }
    }
}

impl DisplayControlHandler for DisplayControlBackend {
    fn monitor_layout(&self, layout: DisplayControlMonitorLayout) {
        let display = Arc::clone(&self.display);
        task::spawn_blocking(move || display.blocking_lock().request_layout(layout));
    }
}

/// RDP Server
///
/// A server is created to listen for connections.
/// After the connection sequence is finalized using the provided security mechanism, the server can:
///  - receive display updates from a [`RdpServerDisplay`] and forward them to the client
///  - receive input events from a client and forward them to an [`RdpServerInputHandler`]
///
/// # Example
///
/// ```
/// use ironrdp_server::{RdpServer, RdpServerInputHandler, RdpServerDisplay, RdpServerDisplayUpdates};
///
///# use anyhow::Result;
///# use ironrdp_server::{DisplayUpdate, DesktopSize, KeyboardEvent, MouseEvent};
///# use tokio_rustls::TlsAcceptor;
///# struct NoopInputHandler;
///# impl RdpServerInputHandler for NoopInputHandler {
///#     fn keyboard(&mut self, _: KeyboardEvent) {}
///#     fn mouse(&mut self, _: MouseEvent) {}
///# }
///# struct NoopDisplay;
///# #[async_trait::async_trait]
///# impl RdpServerDisplay for NoopDisplay {
///#     async fn size(&mut self) -> DesktopSize {
///#         todo!()
///#     }
///#     async fn updates(&mut self) -> Result<Box<dyn RdpServerDisplayUpdates>> {
///#         todo!()
///#     }
///# }
///# async fn stub() -> Result<()> {
/// fn make_tls_acceptor() -> TlsAcceptor {
///    /* snip */
///#    todo!()
/// }
///
/// fn make_input_handler() -> impl RdpServerInputHandler {
///    /* snip */
///#    NoopInputHandler
/// }
///
/// fn make_display_handler() -> impl RdpServerDisplay {
///    /* snip */
///#    NoopDisplay
/// }
///
/// let tls_acceptor = make_tls_acceptor();
/// let input_handler = make_input_handler();
/// let display_handler = make_display_handler();
///
/// let mut server = RdpServer::builder()
///     .with_addr(([127, 0, 0, 1], 3389))
///     .with_tls(tls_acceptor)
///     .with_input_handler(input_handler)
///     .with_display_handler(display_handler)
///     .build();
///
/// server.run().await;
/// Ok(())
///# }
/// ```
/// (M5c) The EGFX dynamic virtual channel's announced name (MS-RDPEGFX). Used to
/// find its DVC id so it can be named in a Soft-Sync request.
#[cfg(feature = "multitransport")]
const EGFX_DVC_CHANNEL_NAME: &str = "Microsoft::Windows::RDS::Graphics";

/// (M5c) EXPERIMENTAL: whether to actually migrate the EGFX channel onto the UDP
/// tunnel (vs. the proven safe spike that sends an empty Soft-Sync and keeps EGFX
/// on TCP). Gated on the `MACRDP_UDP_MIGRATE_EGFX` env var so the default build
/// behaves exactly as the verified M5c step-1+2. Read once and cached.
#[cfg(feature = "multitransport")]
fn migrate_egfx_enabled() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| crate::multitransport::env_truthy("MACRDP_UDP_MIGRATE_EGFX"))
}

/// (vendored, extends divergences 12+15) Link-RTT gate for the multitransport
/// offer: a connection whose kernel-measured TCP RTT at accept is at or above
/// `MACRDP_UDP_OFFER_MAX_RTT_MS` (ms; default 80; 0 disables the gate) is not
/// offered UDP at all — it runs plain TCP from the first byte. On overlay
/// links (VPN/ZeroTier/mobile) the UDP tunnel is prone to wedging, and the
/// reactive tunnel-death detection only bounds the damage (up to ~30 s of dead
/// tunnel + possibly one client-side session reset per wedge); withholding the
/// offer avoids the predictably bad case entirely, so the lossy-audio /
/// EGFX-over-UDP switches are safe to leave enabled on a roaming client —
/// LAN/WiFi sessions get UDP, distant sessions silently stay pure TCP. The
/// residual case (a link that degrades after connect) stays covered by the
/// tunnel-death detection. Threshold read once and cached; the RTT is
/// per-connection (divergence 15 cell).
#[cfg(feature = "multitransport")]
fn multitransport_offer_max_rtt_ms() -> u32 {
    use std::sync::OnceLock;
    static V: OnceLock<u32> = OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("MACRDP_UDP_OFFER_MAX_RTT_MS")
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(80)
    })
}

/// (P2.4b diagnostic) EXPERIMENTAL: migrate EGFX onto the LOSSY (UdpFecL/DTLS)
/// tunnel instead of the reliable (UdpFecR/rustls) one. This is an *isolation
/// test* for the DTLS `RDP_TUNNEL_DATA` egress path (`ship_outbound`'s DTLS
/// branch): EGFX-over-tunnel is already proven on the reliable tunnel, so if it
/// also renders over DTLS the framing is correct and any lossy-audio failure is
/// purely the MS-RDPEA channel model — not our tunnel data path. Requires
/// `MACRDP_UDP_MIGRATE_EGFX` + `MACRDP_UDP_OFFER_FECL` (so a lossy tunnel exists).
/// Read once and cached.
#[cfg(feature = "multitransport")]
fn migrate_egfx_lossy() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| crate::multitransport::env_truthy("MACRDP_UDP_MIGRATE_EGFX_LOSSY"))
}

/// (vendored, divergence 15) Read the kernel's smoothed TCP RTT for an accepted
/// connection, in milliseconds, via `getsockopt(TCP_CONNECTION_INFO)` (macOS).
/// The kernel seeds srtt from the SYN/SYN-ACK exchange, so a meaningful value is
/// available immediately at accept. Returns `None` on error or on platforms
/// without the option (Linux CI compiles the stub); a sub-ms LAN RTT maps to 1
/// so 0 stays reserved for "unknown".
#[cfg(target_os = "macos")]
pub fn tcp_srtt_ms(stream: &tokio::net::TcpStream) -> Option<u32> {
    use std::os::fd::AsRawFd;
    let fd = stream.as_raw_fd();
    let mut info: libc::tcp_connection_info = unsafe { core::mem::zeroed() };
    let mut len = core::mem::size_of::<libc::tcp_connection_info>() as libc::socklen_t;
    // SAFETY: fd is a live TCP socket owned by `stream`; `info`/`len` describe a
    // properly sized, writable out-buffer for this socket option.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_CONNECTION_INFO,
            core::ptr::from_mut(&mut info).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return None;
    }
    // tcpi_srtt is already in milliseconds on macOS.
    Some(info.tcpi_srtt.max(1))
}

#[cfg(not(target_os = "macos"))]
pub fn tcp_srtt_ms(_stream: &tokio::net::TcpStream) -> Option<u32> {
    None
}

pub struct RdpServer {
    opts: RdpServerOptions,
    // FIXME: replace with a channel and poll/process the handler?
    handler: Arc<Mutex<Box<dyn RdpServerInputHandler>>>,
    display: Arc<Mutex<Box<dyn RdpServerDisplay>>>,
    static_channels: StaticChannelSet,
    // (vendored, divergence 23) `Rc`, not `Box`: a preempting candidate's
    // trial negotiation (see `negotiate_and_authenticate`) needs to build its
    // own real static/dynamic channels concurrently with the live
    // connection's `run_connection`, which holds `&mut self` for the whole
    // race — so it works from a cheaply-cloned snapshot of these factories
    // instead of going through `self`. They're set once at construction and
    // never reassigned, so sharing via `Rc` costs nothing at steady state.
    sound_factory: Option<Rc<dyn SoundServerFactory>>,
    cliprdr_factory: Option<Rc<dyn CliprdrServerFactory>>,
    rdpdr_factory: Option<Rc<dyn crate::RdpdrServerFactory>>,
    // (divergence 16) server-direction MS-RDPEUSB (URBDRC) USB redirection.
    // Ships inert: the URBDRC DVC is advertised only when this is `Some`.
    usb_factory: Option<Rc<dyn crate::UrbdrcServerFactory>>,
    // (divergence 19) server-direction MS-RDPECAM camera redirection — Phase-0
    // protocol gate. The `RDCamera_Device_Enumerator` DVC is advertised only when
    // this is `Some`; byte-identical when None.
    camera_factory: Option<Rc<dyn crate::RdCameraServerFactory>>,
    echo_handle: EchoServerHandle,
    #[cfg(feature = "egfx")]
    gfx_factory: Option<Rc<dyn GfxServerFactory>>,
    #[cfg(feature = "egfx")]
    gfx_handle: Option<crate::gfx::GfxServerHandle>,
    ev_sender: mpsc::UnboundedSender<ServerEvent>,
    ev_receiver: Arc<Mutex<mpsc::UnboundedReceiver<ServerEvent>>>,
    /// Dedicated bounded channel for outbound `Wave` PDUs. Audio
    /// dispatch (the `dispatch_audio` task spawned in `client_loop`)
    /// reads from this receiver independently of the unified
    /// `ServerEvent` stream, so inbound cliprdr/PDU pressure on the
    /// per-connection `Mutex<Self>` doesn't starve audio output. The
    /// audio backend (e.g., `MacRdpsnd`) gets the sender via
    /// `SoundServerFactory::set_audio_sender`. Bounded capacity caps
    /// the queue at ~1 s of audio so capture-side backpressure kicks
    /// in before the queue grows unbounded if dispatch ever stalls.
    audio_receiver: Arc<Mutex<mpsc::Receiver<crate::AudioWave>>>,
    creds: Option<Credentials>,
    local_addr: Option<SocketAddr>,
    autodetect: Option<AutoDetectManager>,
    /// (vendored, divergence 23) Anti-ping-pong net for session preemption:
    /// the peer most recently EVICTED by a takeover, and when it last tried to
    /// come back.
    ///
    /// `EvictedByOtherConnection` is the real fix for the eviction loop (it
    /// tells the loser not to auto-reconnect), but whether a given client
    /// honors it is client-dependent. This bounds the damage if one doesn't:
    /// a peer that was just evicted may not immediately re-preempt, and each
    /// refused attempt RE-ARMS the window. An auto-reconnect storm (~1 s
    /// cadence) therefore can never win the session back, while a human who
    /// closes and reconnects — a gap far longer than
    /// [`REPREEMPT_COOLDOWN`] — still can. Cleared once a takeover sticks.
    recently_evicted: Option<(IpAddr, Instant)>,
    // (vendored, divergence 22) `Rc<RefCell<..>>`, not a bare `Option<Box<..>>`:
    // the preemption race in `run()` needs to call `on_accept` on a candidate
    // while `self` is ALSO mutably borrowed for the whole race by the live
    // connection's `run_connection` future (which itself calls
    // `on_authenticated`/`on_client_fingerprint` through this same field
    // mid-connection). A bare `.take()` of the field for the race's duration
    // — the first cut of this fix — silently broke those two hooks for
    // EVERY connection (not just preempted ones), since macrdp's preemption
    // is unconditional: caught by the audit-log CI integration test showing
    // zero `event="auth"` records. Sharing via `Rc<RefCell<..>>` lets a
    // cheap clone reach the handler for the candidate's `on_accept` without
    // taking it away from the connection that's using it concurrently.
    connection_handler: Option<Rc<RefCell<Box<dyn ConnectionHandler>>>>,
    /// True when the client has sent `SuppressOutput { desktop_rect: None }`
    /// — the standard RDP "I am minimized / don't need display updates"
    /// signal (e.g., mstsc on window-minimize). Cleared on
    /// `SuppressOutput { Some(rect) }` or `RefreshRectangle` (sent on
    /// refocus). Exposed via [`Self::display_suppressed_handle`] so the
    /// display backend (capture / H.264 encode pipeline) can skip frame
    /// emission while it's set — without this, mstsc accumulates EGFX
    /// frames during a long minimize and the refocus chew-through locks
    /// up its input dispatch for several seconds.
    display_suppressed: Arc<AtomicBool>,
    /// (vendored) Forwarded to each connection's `Acceptor` so it adopts
    /// the desktop size the client requests in its Client Core Data
    /// before Demand Active is sent. See
    /// [`Self::set_honor_client_desktop_size`].
    honor_client_desktop_size: bool,
    /// (vendored) Optional operator ceiling for the honored client size,
    /// forwarded to each connection's `Acceptor` and clamped per-dimension
    /// there. See [`Self::set_honor_client_desktop_size_max`].
    honor_client_desktop_size_max: Option<DesktopSize>,
    /// (vendored) Optional shared cell the server writes with the client's
    /// announced keyboard-layout identifier (KLID, from the acceptor result)
    /// when a client connects. The input backend (e.g. `macrdp`'s
    /// `MacInputHandler`) holds a clone and auto-selects a matching layout, so
    /// non-US clients type correctly with no manual configuration. 0 = unknown.
    keyboard_layout: Option<Arc<AtomicU32>>,
    /// (vendored, divergence 15) Optional shared cell the server writes with the
    /// kernel's smoothed TCP round-trip time (milliseconds, from
    /// `TCP_CONNECTION_INFO`) for each accepted connection, sampled in the
    /// accept loop before the stream is wrapped (the only point the raw fd is
    /// reachable). Link-adaptive consumers (macrdp's blank-recovery gating and
    /// adaptive-bitrate seeding in `h264.rs`) hold a clone. 0 = unknown /
    /// unsupported platform; a sub-millisecond LAN RTT stores as 1.
    link_rtt_ms: Option<Arc<AtomicU32>>,
    /// (vendored, extends divergence 12) The cookie registered for the CURRENT
    /// connection's multitransport offer, so the post-connection reset can
    /// evict it from the registry no matter how the connection ended. Without
    /// this, eviction keyed off `MigrationState` (set only in `client_accepted`)
    /// — so any connection dying between the offer and activation (mstsc's
    /// cert-prompt broken pipe, CredSSP failures, port scans) leaked its
    /// registry entry forever, and a client's LATE tunnel bind (UDP handshake
    /// completing after a fast session end) could consume the stale cookie and
    /// re-raise a retired bound flag → zombie peer → spurious death + 10-min
    /// offer cooldown.
    #[cfg(feature = "multitransport")]
    current_offer_cookie: Option<[u8; 16]>,
    /// (vendored, divergence 13) Optional Server Auto-Reconnect Cookie
    /// (MS-RDPBCGR 2.2.4.3 ARC_SC_PRIVATE_PACKET). When set, the server sends a
    /// Save Session Info PDU carrying it once per TCP connection, right after
    /// activation. A client (mstsc) only enters its "Attempting to reconnect"
    /// auto-reconnect loop on an *ungraceful* disconnect if it was given this
    /// cookie during logon; without it a dropped connection just reports
    /// disconnected. `macrdp` sets it (`set_auto_reconnect_cookie`) so the
    /// EGFX blank-recovery connection drop (`src/h264.rs`) heals with a seamless
    /// auto-reconnect instead of a manual one. `None` = don't send (default);
    /// the returning ARC_CS cookie is not validated (single console session +
    /// NLA re-auth), so this only *enables the client's* auto-reconnect.
    auto_reconnect_cookie: Option<rdp::session_info::ServerAutoReconnect>,
    /// (vendored, divergence 13) Per-TCP-connection guard so the cookie is sent
    /// exactly once (after the first activation), not again on each
    /// deactivation-reactivation resize. Reset at the top of `run_connection`.
    auto_reconnect_sent: bool,
    /// (vendored) Optional UDP-multitransport provider (MS-RDPEMT). When set,
    /// the server offers an auxiliary UDP transport to clients that advertise
    /// support in their GCC MultiTransportChannelData block. M1: negotiation
    /// only (no UDP listener); see `src/multitransport/`.
    #[cfg(feature = "multitransport")]
    multitransport: Option<Box<dyn crate::multitransport::MultitransportProvider>>,
    /// (vendored) Per-connection multitransport negotiation state (the issued
    /// `request_id` + cookie) used to match the client's
    /// `MultitransportResponsePdu`. Reset every connection in `client_accepted`.
    #[cfg(feature = "multitransport")]
    multitransport_migration: Option<crate::multitransport::MigrationState>,
    /// (vendored) Shared registry of issued multitransport security cookies. When
    /// set, the offer path registers each cookie here so the process-global UDP
    /// listener can bind an inbound tunnel to a real TCP session (reject forged /
    /// replayed cookies). `None` leaves binding soft (the listener accepts any
    /// CREATEREQUEST). Set once via `set_multitransport_cookie_registry`.
    #[cfg(feature = "multitransport")]
    multitransport_cookies: Option<crate::multitransport::CookieRegistry>,
    /// (M5c) Shared per-connection flag the UDP listener sets `true` when it binds
    /// this connection's tunnel (cookie match). The TCP side reads it to know the
    /// UDP multitransport connection is up — the trigger to send the Soft-Sync
    /// request that moves EGFX onto the tunnel. mstsc signals multitransport
    /// success by *creating the tunnel*, NOT by an Initiate Response over TCP, so
    /// this listener→server flag (not a message-channel PDU) is the real gate.
    #[cfg(feature = "multitransport")]
    udp_tunnel_bound: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// (M5c) Handoff to the process-global UDP listener for shipping channel data
    /// (EGFX) over the bound tunnel. `None` = no UDP data path (EGFX stays on TCP).
    #[cfg(feature = "multitransport")]
    multitransport_tunnel_sender: Option<crate::multitransport::TunnelSender>,
    /// (M5c) Set once a Soft-Sync request has migrated the EGFX DVC channel to the
    /// UDP tunnel — from then on EGFX frames route over UDP, not TCP. Only ever set
    /// when EGFX migration is enabled (the `--udp-migrate-egfx` flag /
    /// `migrate_egfx` field, or the legacy `MACRDP_UDP_MIGRATE_EGFX` env fallback);
    /// the default path leaves EGFX on TCP (the proven empty-Soft-Sync spike).
    #[cfg(feature = "multitransport")]
    egfx_on_udp: bool,
    /// Whether to migrate the EGFX DVC onto the reliable UDP tunnel (vs. the empty
    /// safe spike that keeps EGFX on TCP). Set by the application from the
    /// `--udp-migrate-egfx` flag; OR'd with the legacy `MACRDP_UDP_MIGRATE_EGFX`
    /// env var (kept so the `..._LOSSY` isolation test still works). Default false.
    #[cfg(feature = "multitransport")]
    migrate_egfx: bool,
    /// Shared flag the macrdp-side EGFX/H.264 pipeline reads for ack-driven IDR
    /// recovery: set true once EGFX has been migrated onto the **lossy** tunnel
    /// (`MACRDP_UDP_MIGRATE_EGFX_LOSSY`), where a dropped frame is real loss the
    /// client can't retransmit away. On the reliable tunnel / TCP it stays false
    /// (a missing ack there is congestion, not loss). `None` = not wired (default).
    #[cfg(feature = "multitransport")]
    egfx_on_lossy_handle: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// Shared flag the macrdp-side EGFX/H.264 pipeline reads to enable frame-ack-lag
    /// backpressure: set true once EGFX is migrated onto **any** UDP tunnel
    /// (reliable or lossy), where there's no socket backpressure to pace the server
    /// to the client. On TCP it stays false (the socket paces it; the push path must
    /// stay byte-identical). `None` = not wired (default).
    #[cfg(feature = "multitransport")]
    egfx_on_udp_handle: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// EGFX-over-UDP → TCP watchdog request. The macrdp H.264 pipeline sets this
    /// true when it detects the reliable UDP tunnel has wedged (acks silent while
    /// shipping); the EGFX route block reads it and flips `egfx_on_udp` back to
    /// false so subsequent frames route over the TCP DRDYNVC channel (mstsc renders
    /// EGFX on TCP post-Soft-Sync — Spike A). One-way per connection; reset to false
    /// on reconnect so a fresh connection retries UDP. `None` = not wired (default).
    #[cfg(feature = "multitransport")]
    demigrate_request: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// (M5c step 3b) Receiver for inbound migrated-channel data the UDP listener
    /// forwards (each item is a bare DRDYNVC PDU — HigherLayerData of an inbound
    /// `RDP_TUNNEL_DATA`, e.g. an EGFX frame ack). Created + registered (with the
    /// matching sender in the cookie registry) per connection at the offer site;
    /// `client_loop` drains it into the drdynvc processor. `None` when no offer is
    /// in flight (TCP-only).
    #[cfg(feature = "multitransport")]
    multitransport_tunnel_inbound_rx: Option<tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>>,
    /// (P2.4b) Audio format list to advertise on the lossy-UDP `AUDIO_PLAYBACK_LOSSY_DVC`
    /// dynamic channel. `Some` registers the channel (the application supplies a
    /// PCM+AAC list matching what its wave path encodes); `None` (default) leaves
    /// the channel unregistered. Set via `set_multitransport_lossy_audio_formats`.
    #[cfg(feature = "multitransport")]
    multitransport_lossy_audio_formats: Option<Vec<ironrdp_rdpsnd::pdu::AudioFormat>>,
    /// (P2.4b dual-channel) The client-list format index negotiated on the RELIABLE
    /// `AUDIO_PLAYBACK_DVC` over TCP, shared with the lossy wave path so the `Wave2`
    /// PDUs it ships over the lossy/DTLS tunnel carry the right `wFormatNo`.
    #[cfg(feature = "multitransport")]
    lossy_audio_format: crate::multitransport::audio_dvc::NegotiatedAudioFormat,
    /// (2b-iv-B) Per-wave `block_no` sequence for the lossy `AUDIO_PLAYBACK_LOSSY_DVC`
    /// `Wave2` PDUs (independent of the static rdpsnd channel's own counter). Bumped
    /// per wave shipped over the tunnel; wraps at 255.
    #[cfg(feature = "multitransport")]
    lossy_audio_block_no: u8,
    /// (2b-iv-B) One-shot marker so the "now streaming audio over the lossy tunnel"
    /// log fires exactly once at the static-rdpsnd → lossy-DVC handover (not per wave).
    #[cfg(feature = "multitransport")]
    lossy_audio_streaming: bool,
}

#[derive(Debug)]
pub enum ServerEvent {
    Quit(String),
    /// (vendored, divergence 23) End this connection because an authenticated
    /// candidate is taking the session over — a preemption, not a plain quit.
    ///
    /// Unlike [`Self::Quit`], this sends a Server Set Error Info PDU
    /// (`ERRINFO_DISCONNECTED_BY_OTHERCONNECTION`, MS-RDPBCGR 2.2.5.1.1 — the
    /// same code real Windows RDS uses for a session takeover) before
    /// disconnecting. That distinction is LOAD-BEARING, not cosmetic: macrdp
    /// provisions the Server Auto-Reconnect Cookie by default (divergence 13,
    /// for blank-recovery), so a client dropped with no explanation silently
    /// auto-reconnects a second later. With preemption in play that produced
    /// an infinite ping-pong — each evicted client bounced straight back,
    /// authenticated, and preempted the other, forever (observed live at
    /// ~1-2 s per cycle). Telling the loser WHY it was disconnected is what
    /// makes it stay away.
    EvictedByOtherConnection,
    Clipboard(ClipboardMessage),
    /// File-copy initiation that bypasses `ClipboardMessage::SendInitiateCopy`
    /// and reaches `CliprdrServer::initiate_file_copy` directly. The only
    /// way to populate the cliprdr server's `local_file_list`, without
    /// which inbound `FileContentsRequest`s short-circuit with
    /// CB_RESPONSE_FAIL. Upstream cliprdr never exposed this through the
    /// `ClipboardMessage` enum.
    ClipboardFileCopy(Vec<ironrdp_cliprdr::pdu::FileDescriptor>),
    Rdpsnd(RdpsndServerMessage),
    /// Server-initiated RDPDR device-I/O requests, framed by [`RdpdrHandle`]
    /// (drive redirection). Written on the rdpdr static channel.
    Rdpdr(crate::RdpdrServerMessage),
    /// Server-loop actions for server-direction USB redirection (MS-RDPEUSB) that
    /// the `URBDRC` processor can't do itself — currently opening a per-device DVC.
    Urbdrc(crate::UrbdrcServerMessage),
    /// Server-loop actions for server-direction camera redirection (MS-RDPECAM):
    /// the enumerator processor asks the loop to open a per-device DVC.
    Camera(crate::CameraServerMessage),
    Echo(EchoServerMessage),
    SetCredentials(Credentials),
    GetLocalAddr(oneshot::Sender<Option<SocketAddr>>),
    #[cfg(feature = "egfx")]
    Egfx(EgfxServerMessage),
    /// Trigger an RTT measurement probe (requires auto-detect enabled).
    AutoDetectRttRequest,
}

pub trait ServerEventSender {
    fn set_sender(&mut self, sender: mpsc::UnboundedSender<ServerEvent>);
}

impl ServerEvent {
    pub fn create_channel() -> (mpsc::UnboundedSender<Self>, mpsc::UnboundedReceiver<Self>) {
        mpsc::unbounded_channel()
    }
}

#[derive(Debug, PartialEq)]
enum RunState {
    Continue,
    Disconnect,
    DeactivationReactivation { desktop_size: DesktopSize },
}

/// (vendored, divergence 23) The GFX server handle `attach_channels_impl`
/// hands back, so its caller can decide whether/when to install it on
/// `self.gfx_handle` — a type alias so the function signature doesn't need
/// its own `#[cfg(feature = "egfx")]` variant.
#[cfg(feature = "egfx")]
type AttachedGfxHandle = Option<crate::gfx::GfxServerHandle>;
#[cfg(not(feature = "egfx"))]
type AttachedGfxHandle = ();

/// (vendored, divergence 23) The channel-attaching half of connection setup,
/// factored out of `RdpServer::attach_channels` so it can also run for a
/// preempting candidate's trial negotiation (`negotiate_candidate`), which
/// races concurrently against the live connection's `&mut self` borrow and so
/// can't call a `&mut self` method. Takes borrowed/cloned factory references
/// instead of reading `self` directly; the normal path
/// (`RdpServer::attach_channels`) passes `self`'s own fields, the candidate
/// path passes a cloned `NegotiationContext`'s.
///
/// Returns the GFX handle instead of writing it to a field — the normal path
/// installs it on `self.gfx_handle` immediately (nothing else is racing);
/// the candidate path holds it in the `NegotiatedCandidate` and only installs
/// it on `self.gfx_handle` if/when that candidate actually wins (see
/// `serve_negotiated`), since only the winner should claim it.
///
/// **Multitransport is a normal-path-only channel here.** The lossy-audio DVC
/// (added when `multitransport_lossy_audio_formats` is `Some`) is
/// deliberately NOT offered to a preempting candidate: it's only useful when
/// paired with the UDP transport OFFER, which candidates also skip (see
/// `negotiate_candidate`) because that offer registers process-wide cookie/
/// tunnel state on `self` that isn't safe to touch concurrently with the live
/// connection's own multitransport bookkeeping. A candidate that wins via
/// preemption always runs plain TCP with no lossy-audio channel; a normal
/// (non-racing) connection is unaffected.
#[expect(
    clippy::too_many_arguments,
    reason = "internal helper, one call site per caller shape"
)]
fn attach_channels_impl(
    acceptor: &mut Acceptor,
    cliprdr_factory: Option<&dyn CliprdrServerFactory>,
    sound_factory: Option<&dyn SoundServerFactory>,
    rdpdr_factory: Option<&dyn crate::RdpdrServerFactory>,
    usb_factory: Option<&dyn crate::UrbdrcServerFactory>,
    camera_factory: Option<&dyn crate::RdCameraServerFactory>,
    #[cfg(feature = "egfx")] gfx_factory: Option<&dyn GfxServerFactory>,
    #[cfg(feature = "multitransport")] multitransport_lossy_audio_formats: Option<
        Vec<ironrdp_rdpsnd::pdu::AudioFormat>,
    >,
    #[cfg(feature = "multitransport")] lossy_audio_format: crate::multitransport::audio_dvc::NegotiatedAudioFormat,
    display: &Arc<Mutex<Box<dyn RdpServerDisplay>>>,
    handler: &Arc<Mutex<Box<dyn RdpServerInputHandler>>>,
    echo_handle: &EchoServerHandle,
    ev_sender: &mpsc::UnboundedSender<ServerEvent>,
) -> AttachedGfxHandle {
    if let Some(cliprdr_factory) = cliprdr_factory {
        let backend = cliprdr_factory.build_cliprdr_backend();

        let cliprdr = CliprdrServer::new(backend);

        acceptor.attach_static_channel(cliprdr);
    }

    if let Some(factory) = sound_factory {
        let backend = factory.build_backend();

        acceptor.attach_static_channel(RdpsndServer::new(backend));
    }

    // RDPDR (drive redirection). MS-RDPEFS requires it be co-advertised with
    // rdpsnd, so it's attached right after the sound channel. build_rdpdr
    // wires the backend's RdpdrHandle to this connection's event sender so it
    // can issue device-I/O requests.
    if let Some(factory) = rdpdr_factory {
        let rdpdr = crate::rdpdr::build_rdpdr(factory, ev_sender.clone());
        acceptor.attach_static_channel(rdpdr);
    }

    let dcs_backend = DisplayControlBackend::new(Arc::clone(display));
    let dvc = dvc::DrdynvcServer::new()
        .with_dynamic_channel(AInputHandler {
            handler: Arc::clone(handler),
        })
        .with_dynamic_channel(DisplayControlServer::new(Box::new(dcs_backend)));

    let dvc = {
        let echo_handle = echo_handle.clone();
        dvc.with_dynamic_channel(EchoDvcBridge::new(echo_handle))
    };

    #[cfg(feature = "egfx")]
    let (dvc, gfx_handle) = {
        let mut dvc = dvc;
        let mut gfx_handle = None;
        if let Some(gfx_factory) = gfx_factory {
            if let Some((bridge, handle)) = gfx_factory.build_server_with_handle() {
                gfx_handle = Some(handle);
                dvc = dvc.with_dynamic_channel(bridge);
            } else {
                let handler = gfx_factory.build_gfx_handler();
                let gfx_server = ironrdp_egfx::server::GraphicsPipelineServer::new(handler);
                dvc = dvc.with_dynamic_channel(gfx_server);
            }
        }
        (dvc, gfx_handle)
    };
    #[cfg(not(feature = "egfx"))]
    let gfx_handle = ();

    // (vendored, feature=multitransport, P2.4b) Dual audio output DVC for lossy
    // UDP audio. Registered only when the application supplied a format list
    // (macrdp does so when `--enable-aac` + the lossy UDP offer are both on).
    // mstsc tears down if it receives Server Audio Formats on the `_LOSSY_`
    // channel (over TCP *or* the tunnel — verified), so we open BOTH: the
    // reliable `AUDIO_PLAYBACK_DVC` runs the format/quality/training handshake
    // over TCP and publishes the negotiated format index; the lossy
    // `AUDIO_PLAYBACK_LOSSY_DVC` is Soft-Synced onto the lossy/DTLS tunnel and
    // carries only Wave2 data stamped with that index. See the "Multitransport
    // is a normal-path-only channel" note on this function's doc comment.
    #[cfg(feature = "multitransport")]
    let dvc = {
        use crate::multitransport::audio_dvc::AudioLossyDvc;
        let mut dvc = dvc;
        if let Some(formats) = multitransport_lossy_audio_formats {
            dvc = dvc.with_dynamic_channel(AudioLossyDvc::reliable(formats, lossy_audio_format.clone()));
            dvc = dvc.with_dynamic_channel(AudioLossyDvc::lossy());
        }
        dvc
    };

    // (divergence 16) server-direction MS-RDPEUSB (URBDRC) USB redirection.
    // Advertised only when a factory is installed; byte-identical when None.
    let dvc = {
        let mut dvc = dvc;
        if let Some(usb_factory) = usb_factory {
            dvc = dvc.with_dynamic_channel(usb_factory.build_processor());
        }
        dvc
    };

    // (divergence 19) server-direction MS-RDPECAM camera redirection (Phase-0
    // gate). Advertised only when a factory is installed; byte-identical when None.
    let dvc = {
        let mut dvc = dvc;
        if let Some(camera_factory) = camera_factory {
            dvc = dvc.with_dynamic_channel(camera_factory.build_processor());
        }
        dvc
    };

    acceptor.attach_static_channel(dvc);

    gfx_handle
}

/// (vendored, divergence 23) A cheap, `Rc`/`Arc`-cloned snapshot of everything
/// [`negotiate_candidate`] needs to run a preempting candidate's TLS+CredSSP
/// negotiation concurrently with the live connection's `run_connection`
/// (which holds `&mut self` for the whole race — see `RdpServer::run`'s
/// preemption branch). Built once per race via `RdpServer::negotiation_context`.
///
/// Deliberately does NOT carry multitransport state (`self.multitransport*`,
/// `self.link_rtt_ms`, `self.current_offer_cookie`): those fields are
/// process-wide, per-connection-mutated bookkeeping shared with the live
/// connection's own negotiation, so a candidate touching them concurrently
/// would corrupt whichever runs second. A candidate that wins via preemption
/// always negotiates plain TCP (no UDP multitransport offer, no lossy-audio
/// DVC) — see the doc comment on `attach_channels_impl`.
struct NegotiationContext {
    opts: RdpServerOptions,
    creds: Option<Credentials>,
    honor_client_desktop_size: bool,
    honor_client_desktop_size_max: Option<DesktopSize>,
    display: Arc<Mutex<Box<dyn RdpServerDisplay>>>,
    handler: Arc<Mutex<Box<dyn RdpServerInputHandler>>>,
    echo_handle: EchoServerHandle,
    ev_sender: mpsc::UnboundedSender<ServerEvent>,
    cliprdr_factory: Option<Rc<dyn CliprdrServerFactory>>,
    sound_factory: Option<Rc<dyn SoundServerFactory>>,
    rdpdr_factory: Option<Rc<dyn crate::RdpdrServerFactory>>,
    usb_factory: Option<Rc<dyn crate::UrbdrcServerFactory>>,
    camera_factory: Option<Rc<dyn crate::RdCameraServerFactory>>,
    #[cfg(feature = "egfx")]
    gfx_factory: Option<Rc<dyn GfxServerFactory>>,
    connection_handler: Option<Rc<RefCell<Box<dyn ConnectionHandler>>>>,
}

/// (vendored, divergence 23) The result of a successful [`negotiate_candidate`]
/// call: TLS is up, CredSSP authenticated (or the security mode doesn't use
/// CredSSP at all — see that function's doc comment), and static/dynamic
/// channels are attached. Everything needed to jump straight into
/// `RdpServer::accept_finalize` — [`RdpServer::serve_negotiated`] does exactly
/// that for the winner of a preemption race, skipping the negotiation this
/// struct already completed.
struct NegotiatedCandidate {
    framed: TokioFramed<tokio_rustls::server::TlsStream<TcpStream>>,
    acceptor: Acceptor,
    gfx_handle: AttachedGfxHandle,
}

/// (vendored, divergence 23) Negotiate and authenticate `stream` from `peer`
/// against a cloned `ctx`, WITHOUT touching the live connection's `self` at
/// all — this is what lets it run concurrently, inside `RdpServer::run`'s
/// preemption race, against the live connection's own `run_connection` (which
/// holds `&mut self`).
///
/// Returns `Some` only once the candidate has genuinely proven itself: TLS
/// established AND (for macrdp's always-Hybrid security — see
/// [`RdpServerSecurity`]) CredSSP/NLA authentication succeeded. On any
/// failure — TLS rejected, CredSSP rejected, a malformed/non-RDP negotiation,
/// or a security mode this fast path doesn't know how to authenticate for
/// (defensive; macrdp always configures Hybrid) — returns `None` and the live
/// connection is left completely undisturbed. This is the fix for
/// "an unauthenticated connection shouldn't be able to disconnect the live
/// session": the old TPKT-header-only probe proved a candidate was
/// *attempting* an RDP handshake, not that it would succeed, so a connection
/// that failed authentication entirely — or even one that was simply still
/// negotiating when a second, unrelated connection arrived — could still
/// evict the active session before either side finished.
///
/// This duplicates the pre-`accept_finalize` portion of `run_connection`
/// (X.224 negotiate → attach real channels → TLS → CredSSP) rather than
/// sharing code with it, because `run_connection` is generic over any
/// `AsyncRead + AsyncWrite` stream and takes `&mut self`; this is
/// `TcpStream`-specific (the only stream type `RdpServer::run`'s accept loop
/// ever sees) and takes `&NegotiationContext` instead, precisely so it can
/// run alongside a live `&mut self` borrow. Keep the two in sync by hand if
/// the negotiation sequence ever changes upstream.
async fn negotiate_candidate(
    ctx: &NegotiationContext,
    stream: TcpStream,
    peer: SocketAddr,
) -> Option<NegotiatedCandidate> {
    let framed = TokioFramed::new(stream);

    let size = ctx.display.lock().await.size().await;
    let capabilities = capabilities::capabilities(&ctx.opts, size);
    let mut acceptor = Acceptor::new(ctx.opts.security.flag(), size, capabilities, ctx.creds.clone());
    // (vendored) Reconcile macrdp's (honor: bool, max: Option<DesktopSize>) onto the
    // acceptor's unified Option<DesktopSize> API (upstream #1373+#1404): Some(max) =
    // honor + clamp to max; None = don't honor. No explicit max honors up to the
    // protocol ceiling (8192), where the clamp is a no-op.
    acceptor.set_honor_client_desktop_size(ctx.honor_client_desktop_size.then(|| {
        ctx.honor_client_desktop_size_max.unwrap_or(DesktopSize {
            width: 8192,
            height: 8192,
        })
    }));

    #[allow(
        clippy::let_unit_value,
        reason = "attach_channels_impl returns () when the egfx feature is off"
    )]
    let gfx_handle = attach_channels_impl(
        &mut acceptor,
        ctx.cliprdr_factory.as_deref(),
        ctx.sound_factory.as_deref(),
        ctx.rdpdr_factory.as_deref(),
        ctx.usb_factory.as_deref(),
        ctx.camera_factory.as_deref(),
        #[cfg(feature = "egfx")]
        ctx.gfx_factory.as_deref(),
        // No multitransport offer for a candidate (see the struct doc on
        // `NegotiationContext`), so the lossy-audio DVC — which is only
        // useful paired with that offer — is never attached here either.
        #[cfg(feature = "multitransport")]
        None,
        #[cfg(feature = "multitransport")]
        crate::multitransport::audio_dvc::NegotiatedAudioFormat::new(),
        &ctx.display,
        &ctx.handler,
        &ctx.echo_handle,
        &ctx.ev_sender,
    );

    let res = match ironrdp_acceptor::accept_begin(framed, &mut acceptor).await {
        Ok(res) => res,
        Err(error) => {
            debug!(?peer, ?error, "candidate accept_begin failed — not eligible to preempt");
            return None;
        }
    };

    let stream = match res {
        BeginResult::ShouldUpgrade(stream) => stream,
        BeginResult::Continue(_) => {
            // No TLS in this security mode, so there's nothing to
            // authenticate against — matches `RdpServerSecurity::None`,
            // which macrdp never configures. Conservatively not eligible to
            // preempt rather than guessing at a meaning for "authenticated"
            // that doesn't apply here.
            debug!(
                ?peer,
                "candidate connection did not upgrade to TLS — not eligible to preempt"
            );
            return None;
        }
    };

    let tls_acceptor = match &ctx.opts.security {
        RdpServerSecurity::Tls(acceptor) => acceptor,
        RdpServerSecurity::Hybrid((acceptor, _)) => acceptor,
        RdpServerSecurity::None => unreachable!("ShouldUpgrade implies a TLS-capable security mode"),
    };
    let accept = match tls_acceptor.accept(stream).await {
        Ok(accept) => accept,
        Err(error) => {
            debug!(?peer, ?error, "candidate TLS accept failed — not eligible to preempt");
            return None;
        }
    };
    let mut framed = TokioFramed::new(accept);

    acceptor.mark_security_upgrade_as_done();

    let hybrid_pub_key = match &ctx.opts.security {
        RdpServerSecurity::Hybrid((_, pub_key)) => Some(pub_key.clone()),
        _ => None,
    };
    if let Some(pub_key) = hybrid_pub_key {
        let client_name = "rdp-client".to_owned();

        let auth_result = ironrdp_acceptor::accept_credssp(
            &mut framed,
            &mut acceptor,
            &mut ironrdp_tokio::reqwest::ReqwestNetworkClient::new(),
            client_name.into(),
            pub_key,
            None,
        )
        .await;

        // (vendored, divergence 18) Same audit hook `run_connection` uses for
        // the live connection — reachable here via the `Rc<RefCell<..>>`
        // clone in `ctx`. Fires for a REJECTED candidate too (unlike the old
        // TPKT probe, which had no auth outcome to report at all), so a
        // failed preemption attempt against a real macrdp deployment still
        // shows up in the audit log / AuthGuardHandler lockout accounting.
        if let Some(handler) = ctx.connection_handler.as_ref() {
            match &auth_result {
                Ok(()) => handler.borrow_mut().on_authenticated(true, None),
                Err(e) => handler.borrow_mut().on_authenticated(false, Some(&e.to_string())),
            }
        }

        if let Err(error) = auth_result {
            debug!(
                ?peer,
                ?error,
                "candidate CredSSP authentication failed — not preempting the live session"
            );
            return None;
        }
    }

    debug!(?peer, "candidate authenticated — eligible to preempt the live session");
    Some(NegotiatedCandidate {
        framed,
        acceptor,
        gfx_handle,
    })
}

impl RdpServer {
    // Takes every channel factory + shared handle explicitly; RdpServerBuilder is the
    // ergonomic construction path.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        opts: RdpServerOptions,
        handler: Box<dyn RdpServerInputHandler>,
        display: Box<dyn RdpServerDisplay>,
        mut sound_factory: Option<Box<dyn SoundServerFactory>>,
        mut cliprdr_factory: Option<Box<dyn CliprdrServerFactory>>,
        mut rdpdr_factory: Option<Box<dyn crate::RdpdrServerFactory>>,
        mut usb_factory: Option<Box<dyn crate::UrbdrcServerFactory>>,
        mut camera_factory: Option<Box<dyn crate::RdCameraServerFactory>>,
        connection_handler: Option<Box<dyn ConnectionHandler>>,
        #[cfg(feature = "egfx")] mut gfx_factory: Option<Box<dyn GfxServerFactory>>,
    ) -> Self {
        let (ev_sender, ev_receiver) = ServerEvent::create_channel();
        // Bounded channel sized to ~1 s of audio at our steady-state
        // 42.78 waves/s (1029 samples/wave at 44.1 kHz). Backpressures
        // the capture loop's `send().await` rather than queuing
        // indefinitely if dispatch ever stalls — losses then happen at
        // the SCK ring buffer instead of server-side.
        let (audio_sender, audio_receiver) = mpsc::channel::<crate::AudioWave>(50);
        if let Some(cliprdr) = cliprdr_factory.as_mut() {
            cliprdr.set_sender(ev_sender.clone());
        }
        if let Some(snd) = sound_factory.as_mut() {
            snd.set_sender(ev_sender.clone());
            snd.set_audio_sender(audio_sender);
        }
        if let Some(rdpdr) = rdpdr_factory.as_mut() {
            rdpdr.set_sender(ev_sender.clone());
        }
        if let Some(usb) = usb_factory.as_mut() {
            usb.set_sender(ev_sender.clone());
        }
        if let Some(camera) = camera_factory.as_mut() {
            camera.set_sender(ev_sender.clone());
        }
        #[cfg(feature = "egfx")]
        if let Some(gfx) = gfx_factory.as_mut() {
            gfx.set_sender(ev_sender.clone());
        }
        Self {
            opts,
            handler: Arc::new(Mutex::new(handler)),
            display: Arc::new(Mutex::new(display)),
            static_channels: StaticChannelSet::new(),
            // Wrapped into `Rc` here (after the one-time `set_sender` setup
            // above, which needs `&mut` on the still-owned `Box`) so a
            // preempting candidate's trial negotiation can hold its own
            // cheap clone — see the field doc comment.
            sound_factory: sound_factory.map(Rc::from),
            cliprdr_factory: cliprdr_factory.map(Rc::from),
            rdpdr_factory: rdpdr_factory.map(Rc::from),
            usb_factory: usb_factory.map(Rc::from),
            camera_factory: camera_factory.map(Rc::from),
            echo_handle: EchoServerHandle::new(ev_sender.clone()),
            #[cfg(feature = "egfx")]
            gfx_factory: gfx_factory.map(Rc::from),
            #[cfg(feature = "egfx")]
            gfx_handle: None,
            ev_sender,
            ev_receiver: Arc::new(Mutex::new(ev_receiver)),
            audio_receiver: Arc::new(Mutex::new(audio_receiver)),
            creds: None,
            local_addr: None,
            autodetect: None,
            recently_evicted: None,
            connection_handler: connection_handler.map(|h| Rc::new(RefCell::new(h))),
            display_suppressed: Arc::new(AtomicBool::new(false)),
            keyboard_layout: None,
            link_rtt_ms: None,
            #[cfg(feature = "multitransport")]
            current_offer_cookie: None,
            auto_reconnect_cookie: None,
            auto_reconnect_sent: false,
            honor_client_desktop_size: false,
            honor_client_desktop_size_max: None,
            #[cfg(feature = "multitransport")]
            multitransport: None,
            #[cfg(feature = "multitransport")]
            multitransport_migration: None,
            #[cfg(feature = "multitransport")]
            multitransport_cookies: None,
            #[cfg(feature = "multitransport")]
            udp_tunnel_bound: None,
            #[cfg(feature = "multitransport")]
            egfx_on_lossy_handle: None,
            #[cfg(feature = "multitransport")]
            egfx_on_udp_handle: None,
            #[cfg(feature = "multitransport")]
            demigrate_request: None,
            #[cfg(feature = "multitransport")]
            multitransport_tunnel_sender: None,
            #[cfg(feature = "multitransport")]
            egfx_on_udp: false,
            #[cfg(feature = "multitransport")]
            migrate_egfx: false,
            #[cfg(feature = "multitransport")]
            multitransport_tunnel_inbound_rx: None,
            #[cfg(feature = "multitransport")]
            multitransport_lossy_audio_formats: None,
            #[cfg(feature = "multitransport")]
            lossy_audio_format: crate::multitransport::audio_dvc::NegotiatedAudioFormat::new(),
            #[cfg(feature = "multitransport")]
            lossy_audio_block_no: 0,
            #[cfg(feature = "multitransport")]
            lossy_audio_streaming: false,
        }
    }

    pub fn builder() -> builder::RdpServerBuilder<builder::WantsAddr> {
        builder::RdpServerBuilder::new()
    }

    pub fn event_sender(&self) -> &mpsc::UnboundedSender<ServerEvent> {
        &self.ev_sender
    }

    /// Returns the shared "display suppressed" flag — true while the
    /// connected client has sent `SuppressOutput { desktop_rect: None }`
    /// (e.g., mstsc minimized). Display backends should hold a clone
    /// of this `Arc` and skip frame emission while it is set, so the
    /// client doesn't accumulate a backlog of EGFX frames it can't
    /// present until refocus. Cleared on `SuppressOutput { Some(rect) }`
    /// or `RefreshRectangle`.
    pub fn display_suppressed_handle(&self) -> Arc<AtomicBool> {
        self.display_suppressed.clone()
    }

    /// Replace the internally-created display-suppressed flag with one
    /// the caller already shared with the display backend before
    /// constructing the server. Use this when the display backend
    /// (e.g., `macrdp`'s `CaptureDisplay`) needs to read the same flag
    /// that the per-connection PDU handler writes to: create one
    /// `Arc<AtomicBool>`, hand a clone to the display, then call this
    /// to swap the server's internal default for that shared instance.
    /// Must be called before any client connects.
    pub fn set_display_suppressed_handle(&mut self, handle: Arc<AtomicBool>) {
        self.display_suppressed = handle;
    }

    /// (vendored) Serve each session at the desktop size the client
    /// requests in its Client Core Data (e.g. an mstsc full-screen
    /// monitor size), instead of the size the display handler reports
    /// at connect time. The acceptor adopts the client's size before
    /// Demand Active, so no deactivation-reactivation resize is needed;
    /// the display handler observes the adopted size through its normal
    /// `request_initial_size` call (the client's Confirm Active bitmap
    /// capset echoes the Demand Active size). Must be called before any
    /// client connects.
    pub fn set_honor_client_desktop_size(&mut self, honor: bool) {
        self.honor_client_desktop_size = honor;
    }

    /// (vendored) Cap the honored client desktop size at an operator
    /// maximum, clamped per-dimension in the acceptor (defense-in-depth
    /// resource bound — an 8192\u{d7}8192 request is ~256 MB of BGRA per frame).
    /// No effect unless [`Self::set_honor_client_desktop_size`] is also set.
    /// Must be called before any client connects. Mirrors upstream PR #1404;
    /// swap to that API on the pin bump past it.
    pub fn set_honor_client_desktop_size_max(&mut self, max: Option<DesktopSize>) {
        self.honor_client_desktop_size_max = max;
    }

    /// (vendored, divergence 13) Provision the Server Auto-Reconnect Cookie
    /// (ARC_SC_PRIVATE_PACKET) sent in a Save Session Info PDU once per
    /// connection after activation. `logon_id` identifies the session and
    /// `random_bits` is the 16-byte auto-reconnect random (a CSPRNG value
    /// supplied by the caller). Enabling this lets a client (mstsc) auto-
    /// reconnect after an ungraceful drop instead of showing "disconnected" —
    /// macrdp pairs it with the EGFX blank-recovery connection drop so the
    /// recovery reconnect is seamless. Must be called before a client connects.
    pub fn set_auto_reconnect_cookie(&mut self, logon_id: u32, random_bits: [u8; 16]) {
        self.auto_reconnect_cookie = Some(rdp::session_info::ServerAutoReconnect { logon_id, random_bits });
    }

    /// (vendored) Share a cell the server fills with the client's announced
    /// keyboard-layout identifier (KLID) when a client connects. The input
    /// backend holds a clone and can auto-select a matching layout so non-US
    /// clients type the right characters with no manual configuration. Must be
    /// called before any client connects.
    pub fn set_keyboard_layout_handle(&mut self, handle: Arc<AtomicU32>) {
        self.keyboard_layout = Some(handle);
    }

    /// (vendored, divergence 15) Share a cell the server fills with the
    /// kernel's smoothed TCP RTT (ms) for each accepted connection, sampled
    /// at accept time via `TCP_CONNECTION_INFO`. Link-adaptive consumers
    /// (blank-recovery gating, adaptive-bitrate seeding) hold a clone.
    /// 0 = unknown. Must be called before any client connects.
    pub fn set_link_rtt_handle(&mut self, handle: Arc<AtomicU32>) {
        self.link_rtt_ms = Some(handle);
    }

    /// (vendored) Install a UDP-multitransport provider (MS-RDPEMT). When set,
    /// the server offers an auxiliary UDP transport to clients that advertise
    /// support in their GCC MultiTransportChannelData block (surfaced by the
    /// acceptor). M1 performs only the negotiation handshake and always
    /// continues on TCP. Must be called before any client connects.
    #[cfg(feature = "multitransport")]
    pub fn set_multitransport_provider(
        &mut self,
        provider: Option<Box<dyn crate::multitransport::MultitransportProvider>>,
    ) {
        self.multitransport = provider;
    }

    /// (vendored) Install the shared multitransport [`CookieRegistry`](crate::CookieRegistry)
    /// so issued cookies are registered for the UDP listener to bind against.
    /// Pass the **same** registry that was handed to
    /// [`UdpMultitransportListener::bind`](crate::UdpMultitransportListener::bind).
    /// Must be called before any client connects.
    #[cfg(feature = "multitransport")]
    pub fn set_multitransport_cookie_registry(&mut self, registry: Option<crate::multitransport::CookieRegistry>) {
        self.multitransport_cookies = registry;
    }

    /// (M5c) Install the handoff to the UDP listener so the server can ship channel
    /// data (EGFX) over a bound multitransport tunnel. Pair it with the matching
    /// receiver passed to
    /// [`UdpMultitransportListener::bind`](crate::multitransport::listener::UdpMultitransportListener::bind)
    /// (both from one [`tunnel_channel`](crate::multitransport::tunnel_channel)).
    #[cfg(feature = "multitransport")]
    pub fn set_multitransport_tunnel_sender(&mut self, sender: Option<crate::multitransport::TunnelSender>) {
        self.multitransport_tunnel_sender = sender;
    }

    /// Supply the shared flag that signals "EGFX is migrated onto a **lossy** UDP
    /// tunnel" (UDPFECL, where datagrams can actually be dropped). macrdp's H.264
    /// pipeline reads it to arm ack-driven IDR recovery — on a reliable transport
    /// (TCP or UDPFECR) the flag stays `false` so recovery never fires. The server
    /// flips it `true` when it Soft-Syncs the EGFX DVC onto the lossy tunnel.
    #[cfg(feature = "multitransport")]
    pub fn set_egfx_on_lossy_handle(&mut self, handle: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>) {
        self.egfx_on_lossy_handle = handle;
    }

    /// Wire the EGFX-on-UDP flag (frame-ack-lag backpressure). The server flips it
    /// `true` whenever it Soft-Syncs the EGFX DVC onto a UDP tunnel (reliable or
    /// lossy) and back to `false` on connection teardown; the macrdp H.264 pipeline
    /// reads it to enable capture-dropping when the client's decode backlog grows.
    #[cfg(feature = "multitransport")]
    pub fn set_egfx_on_udp_handle(&mut self, handle: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>) {
        self.egfx_on_udp_handle = handle;
    }

    /// Wire the EGFX-over-UDP → TCP watchdog request flag. The macrdp H.264 pipeline
    /// sets it true when the reliable UDP tunnel wedges; the EGFX route block reads
    /// it and de-migrates EGFX back to the TCP DRDYNVC channel for the rest of the
    /// session. Reset to false on reconnect so a fresh connection retries UDP.
    #[cfg(feature = "multitransport")]
    pub fn set_demigrate_request_handle(&mut self, handle: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>) {
        self.demigrate_request = handle;
    }

    /// Enable migrating the EGFX DVC onto the reliable UDP tunnel (the
    /// `--udp-migrate-egfx` flag). When `false` (default) the Soft-Sync sends an
    /// empty channel list and EGFX stays on TCP (the proven safe spike). OR'd with
    /// the legacy `MACRDP_UDP_MIGRATE_EGFX` env var at the Soft-Sync site.
    #[cfg(feature = "multitransport")]
    pub fn set_migrate_egfx(&mut self, on: bool) {
        self.migrate_egfx = on;
    }

    /// (P2.4b) Supply the audio format list for the lossy-UDP `AUDIO_PLAYBACK_LOSSY_DVC`
    /// dynamic channel. `Some(formats)` registers the channel and advertises those
    /// formats (the caller passes a PCM+AAC list matching what its wave path
    /// encodes); `None` (default) leaves it unregistered.
    #[cfg(feature = "multitransport")]
    pub fn set_multitransport_lossy_audio_formats(&mut self, formats: Option<Vec<ironrdp_rdpsnd::pdu::AudioFormat>>) {
        self.multitransport_lossy_audio_formats = formats;
    }

    /// Returns the shared ECHO server handle for runtime probe requests and RTT measurements.
    pub fn echo_handle(&self) -> &EchoServerHandle {
        &self.echo_handle
    }

    /// Enable protocol-level auto-detect ([MS-RDPBCGR 2.2.14]).
    ///
    /// Auto-detect uses lightweight Share Data PDUs on the IO channel,
    /// separate from the ECHO DVC. It supports bandwidth measurement
    /// in addition to RTT and works even when DVC is unavailable.
    ///
    /// Send probes via [`ServerEvent::AutoDetectRttRequest`] and
    /// query results with [`rtt_snapshot()`](Self::rtt_snapshot).
    pub fn enable_autodetect(&mut self) {
        self.autodetect = Some(AutoDetectManager::new());
    }

    /// Get the latest auto-detect RTT snapshot.
    ///
    /// Returns `None` if auto-detect is not enabled or no measurements
    /// have been received yet.
    pub fn rtt_snapshot(&self) -> Option<RttSnapshot> {
        self.autodetect.as_ref().and_then(|ad| ad.snapshot())
    }

    /// Returns the shared EGFX server handle for proactive frame submission.
    ///
    /// Available after `build_server_with_handle()` returns `Some` during
    /// channel setup. Display handlers use this to call
    /// `send_avc420_frame()` / `send_avc444_frame()` and then signal the
    /// event loop via `ServerEvent::Egfx`.
    #[cfg(feature = "egfx")]
    pub fn gfx_handle(&self) -> Option<&crate::gfx::GfxServerHandle> {
        self.gfx_handle.as_ref()
    }

    fn attach_channels(&mut self, acceptor: &mut Acceptor) {
        #[allow(
            clippy::let_unit_value,
            reason = "attach_channels_impl returns () when the egfx feature is off"
        )]
        let gfx_handle = attach_channels_impl(
            acceptor,
            self.cliprdr_factory.as_deref(),
            self.sound_factory.as_deref(),
            self.rdpdr_factory.as_deref(),
            self.usb_factory.as_deref(),
            self.camera_factory.as_deref(),
            #[cfg(feature = "egfx")]
            self.gfx_factory.as_deref(),
            #[cfg(feature = "multitransport")]
            self.multitransport_lossy_audio_formats.clone(),
            #[cfg(feature = "multitransport")]
            self.lossy_audio_format.clone(),
            &self.display,
            &self.handler,
            &self.echo_handle,
            &self.ev_sender,
        );
        #[cfg(feature = "egfx")]
        {
            self.gfx_handle = gfx_handle;
        }
        #[cfg(not(feature = "egfx"))]
        #[allow(clippy::let_unit_value, reason = "gfx_handle is () when the egfx feature is off")]
        let _ = gfx_handle;
    }

    /// (vendored, divergence 23) Build a cheap, `Rc`/`Arc`-cloned snapshot for
    /// a candidate's concurrent negotiation — see [`NegotiationContext`].
    fn negotiation_context(&self) -> NegotiationContext {
        NegotiationContext {
            opts: self.opts.clone(),
            creds: self.creds.clone(),
            honor_client_desktop_size: self.honor_client_desktop_size,
            honor_client_desktop_size_max: self.honor_client_desktop_size_max,
            display: Arc::clone(&self.display),
            handler: Arc::clone(&self.handler),
            echo_handle: self.echo_handle.clone(),
            ev_sender: self.ev_sender.clone(),
            cliprdr_factory: self.cliprdr_factory.clone(),
            sound_factory: self.sound_factory.clone(),
            rdpdr_factory: self.rdpdr_factory.clone(),
            usb_factory: self.usb_factory.clone(),
            camera_factory: self.camera_factory.clone(),
            #[cfg(feature = "egfx")]
            gfx_factory: self.gfx_factory.clone(),
            connection_handler: self.connection_handler.clone(),
        }
    }

    /// (vendored, divergence 23) Phase 2 for a candidate that already won the
    /// preemption race in [`negotiate_candidate`] — TLS and CredSSP are
    /// already done. Installs the winner's GFX handle (deferred until now:
    /// only the actual winner should claim it, see `attach_channels_impl`'s
    /// doc comment) and resets the auto-reconnect-cookie guard (mirroring the
    /// top of `run_connection`), then hands off to the same `accept_finalize`
    /// the normal path uses — from here on a preemption-won connection is
    /// indistinguishable from a normally-accepted one.
    async fn serve_negotiated(&mut self, candidate: NegotiatedCandidate) -> Result<()> {
        self.auto_reconnect_sent = false;
        #[cfg(feature = "egfx")]
        {
            self.gfx_handle = candidate.gfx_handle;
        }
        #[cfg(not(feature = "egfx"))]
        #[allow(clippy::let_unit_value, reason = "gfx_handle is () when the egfx feature is off")]
        let _ = candidate.gfx_handle;

        let framed = self.accept_finalize(candidate.framed, candidate.acceptor).await?;
        debug!("Shutting down TLS connection");
        let (mut tls_stream, _) = framed.into_inner();
        if let Err(e) = tls_stream.shutdown().await {
            debug!(?e, "TLS shutdown error");
        }

        Ok(())
    }

    pub async fn run_connection<S>(&mut self, stream: S) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Send + Sync + Unpin,
    {
        // Audio-lag state was previously on Self and reset here; it now
        // lives task-local to `dispatch_audio`, which is spawned fresh in
        // `client_loop` for each connection, so no reset is needed at
        // this layer anymore.

        // (vendored, divergence 13) Fresh TCP connection: re-arm the one-shot
        // auto-reconnect-cookie send (it's sent after the first activation, not
        // again on each deactivation-reactivation resize within this connection).
        self.auto_reconnect_sent = false;

        let framed = TokioFramed::new(stream);

        let size = self.display.lock().await.size().await;
        let capabilities = capabilities::capabilities(&self.opts, size);
        let mut acceptor = Acceptor::new(self.opts.security.flag(), size, capabilities, self.creds.clone());
        // (vendored) Let the acceptor adopt the client's requested desktop
        // size from Client Core Data before Demand Active goes out.
        acceptor.set_honor_client_desktop_size(self.honor_client_desktop_size.then(|| {
            self.honor_client_desktop_size_max.unwrap_or(DesktopSize {
                width: 8192,
                height: 8192,
            })
        }));

        // (vendored, feature=multitransport) When offering UDP multitransport:
        // (1) advertise EXTENDED_CLIENT_DATA_SUPPORTED so the client actually
        // sends its CS_MULTITRANSPORT GCC block — mstsc omits all optional GCC
        // blocks unless the server sets this X.224 flag; and (2) hand the acceptor
        // the offer so it emits the Server Initiate Multitransport Request at the
        // ONLY point clients honor it — after licensing, before Demand Active.
        // (Sending it post-finalization, once the client is ACTIVE, makes both
        // mstsc and FreeRDP misparse it as a share-control PDU and disconnect.)
        // Tunnel-death cooldown: after the UDP listener declares a bound tunnel
        // dead (peer inbound-silent — e.g. an overlay network like ZeroTier
        // dropping UDP), it suppresses multitransport offers for a cooldown so
        // the client's reconnect (mstsc resets the session ~60 s after its
        // tunnel dies) lands as a stable plain-TCP session instead of
        // re-establishing a doomed tunnel and repeating the reset cycle.
        #[cfg(feature = "multitransport")]
        let mt_suppressed = self
            .multitransport_cookies
            .as_ref()
            .is_some_and(|reg| reg.multitransport_suppressed());
        #[cfg(feature = "multitransport")]
        if mt_suppressed && self.multitransport.is_some() {
            tracing::info!(
                "multitransport offer SUPPRESSED (tunnel-death cooldown) — this \
                 connection runs plain TCP"
            );
        }
        // Link-RTT offer gate (see `multitransport_offer_max_rtt_ms`): don't
        // even offer UDP to a connection whose accept-time RTT marks it as an
        // overlay-class link where the tunnel predictably wedges.
        #[cfg(feature = "multitransport")]
        let mt_rtt_gated = {
            let max = multitransport_offer_max_rtt_ms();
            let rtt = self.link_rtt_ms.as_ref().map_or(0, |c| c.load(Ordering::Relaxed));
            let gated = max > 0 && rtt >= max && self.multitransport.is_some();
            if gated && !mt_suppressed {
                tracing::info!(
                    link_rtt_ms = rtt,
                    max_rtt_ms = max,
                    "multitransport offer WITHHELD (link RTT above the offer gate) — this \
                     connection runs plain TCP; UDP tunnels wedge on high-latency overlay links"
                );
            }
            gated
        };
        // No offer for this connection (cooldown or RTT gate): explicitly clear
        // the per-connection multitransport state so nothing STALE from the
        // previous connection can act. These fields are otherwise only refreshed
        // at the offer site below, so a skipped offer would inherit the prior
        // session's `udp_tunnel_bound` flag (which stays true for up to ~30 s
        // after that session's tunnel goes silent, until tunnel-death flips it)
        // and its `MigrationState` — enough for this connection to route lossy
        // audio waves into a dead tunnel or fire a Soft-Sync naming a tunnel
        // this client never created. The cooldown path was previously safe only
        // by accident (suppression follows a tunnel-death event that already
        // lowered the flag); the RTT gate has no such guarantee — e.g. a WiFi
        // session with a bound tunnel followed within seconds by the client
        // auto-reconnecting over a high-latency link.
        #[cfg(feature = "multitransport")]
        if self.multitransport.is_some() && (mt_suppressed || mt_rtt_gated) {
            if let (Some(registry), Some(prev)) = (
                self.multitransport_cookies.as_ref(),
                self.multitransport_migration.as_ref(),
            ) {
                registry.remove(&prev.cookie);
            }
            self.multitransport_migration = None;
            self.udp_tunnel_bound = None;
            self.multitransport_tunnel_inbound_rx = None;
        }
        #[cfg(feature = "multitransport")]
        if let Some(provider) = self.multitransport.as_ref().filter(|_| !mt_suppressed && !mt_rtt_gated) {
            let offer = crate::multitransport::new_offer(provider.requested_protocol());
            // Register this connection's cookie so the UDP listener can bind an
            // inbound tunnel to it. Evict the previous connection's cookie first
            // (the listener consumes a cookie on a successful bind, so a leftover
            // here is from a connection that fell back to TCP — bound to ~one
            // entry per live RdpServer instance).
            if let Some(registry) = self.multitransport_cookies.as_ref() {
                if let Some(prev) = self.multitransport_migration.as_ref() {
                    registry.remove(&prev.cookie);
                }
                // (M5c step 3b) Per-connection inbound channel: the listener
                // forwards this tunnel's client→server channel data (bare DRDYNVC
                // PDUs) here, keyed by cookie. `client_loop` drains the receiver.
                let (in_tx, in_rx) = tokio::sync::mpsc::unbounded_channel();
                self.multitransport_tunnel_inbound_rx = Some(in_rx);
                // Keep the tunnel-bound flag: the listener flips it on a cookie
                // match, and the EGFX dispatch path reads it to fire Soft-Sync.
                self.udp_tunnel_bound = Some(registry.register(offer.cookie, in_tx));
            }
            self.current_offer_cookie = Some(offer.cookie);
            // (pin bump a5d1c682) EXTENDED_CLIENT_DATA is now advertised unconditionally
            // by the acceptor, so the old set_advertise_extended_client_data(true) call
            // was retired from the re-vendored acceptor — the offer alone drives the
            // multitransport negotiation now.
            acceptor.set_multitransport_offer(Some(offer));
        }

        self.attach_channels(&mut acceptor);

        let res = ironrdp_acceptor::accept_begin(framed, &mut acceptor)
            .await
            .context("accept_begin failed")?;

        match res {
            BeginResult::ShouldUpgrade(stream) => {
                let tls_acceptor = match &self.opts.security {
                    RdpServerSecurity::Tls(acceptor) => acceptor,
                    RdpServerSecurity::Hybrid((acceptor, _)) => acceptor,
                    RdpServerSecurity::None => unreachable!(),
                };
                let accept = match tls_acceptor.accept(stream).await {
                    Ok(accept) => accept,
                    Err(e) => {
                        warn!("Failed to TLS accept: {}", e);
                        return Ok(());
                    }
                };
                let mut framed = TokioFramed::new(accept);

                acceptor.mark_security_upgrade_as_done();

                // Clone the public key out of self.opts first, so the auth-outcome
                // hook below can borrow self.connection_handler without a conflict
                // (the `if let` borrow of self.opts.security would otherwise live
                // to the end of the block).
                let hybrid_pub_key = match &self.opts.security {
                    RdpServerSecurity::Hybrid((_, pub_key)) => Some(pub_key.clone()),
                    _ => None,
                };
                if let Some(pub_key) = hybrid_pub_key {
                    // Generic streams don't expose peer address. Use a neutral
                    // placeholder; it's unclear whether CredSSP/NTLM actually
                    // uses this value in practice.
                    let client_name = "rdp-client".to_owned();

                    let auth_result = ironrdp_acceptor::accept_credssp(
                        &mut framed,
                        &mut acceptor,
                        &mut ironrdp_tokio::reqwest::ReqwestNetworkClient::new(),
                        client_name.into(),
                        pub_key,
                        None,
                    )
                    .await;

                    // (vendored, divergence 18) Report the CredSSP/NLA verdict to the
                    // connection handler once per connection: Ok = credentials
                    // validated, Err = auth did not complete (dominated by bad
                    // credentials). Reactivation reuses the connection and does not
                    // re-run accept_credssp, so this fires exactly once per login.
                    if let Some(handler) = self.connection_handler.as_ref() {
                        match &auth_result {
                            Ok(()) => handler.borrow_mut().on_authenticated(true, None),
                            Err(e) => handler.borrow_mut().on_authenticated(false, Some(&e.to_string())),
                        }
                    }

                    auth_result?;
                }

                let framed = self.accept_finalize(framed, acceptor).await?;
                debug!("Shutting down TLS connection");
                let (mut tls_stream, _) = framed.into_inner();
                if let Err(e) = tls_stream.shutdown().await {
                    debug!(?e, "TLS shutdown error");
                }
            }

            BeginResult::Continue(framed) => {
                self.accept_finalize(framed, acceptor).await?;
            }
        };

        Ok(())
    }

    pub async fn run(&mut self) -> Result<()> {
        // Create socket with control over options before binding.
        // Using TcpSocket instead of TcpListener::bind() allows setting
        // SO_REUSEADDR and IPv6 dual-stack mode.
        let socket = match self.opts.addr {
            SocketAddr::V4(_) => TcpSocket::new_v4().context("create IPv4 socket")?,
            SocketAddr::V6(_) => {
                // IPv6 socket: on Linux, dual-stack is the default
                // (net.ipv6.bindv6only=0), so IPv4 clients connect as
                // IPv4-mapped addresses (::ffff:x.x.x.x). On platforms
                // where IPV6_V6ONLY defaults to 1 (Windows, some BSDs),
                // only IPv6 clients will be accepted and a separate IPv4
                // listener would be needed.
                TcpSocket::new_v6().context("create IPv6 socket")?
            }
        };

        // SO_REUSEADDR prevents EADDRINUSE when restarting the server while
        // the previous socket is still in TIME_WAIT. Only set on Unix;
        // on Windows SO_REUSEADDR has different semantics that allow a
        // second process to bind the same port, which is a security risk.
        #[cfg(unix)]
        socket.set_reuseaddr(true).context("set SO_REUSEADDR")?;

        socket.bind(self.opts.addr).context("bind listen address")?;

        let listener = socket.listen(LISTENER_BACKLOG).context("start listener")?;
        let local_addr = listener.local_addr()?;

        debug!("Listening for connections on {local_addr}");
        self.local_addr = Some(local_addr);

        // (vendored, divergence 23) A candidate that wins a preemption race
        // (see `negotiate_candidate`) has ALREADY cleared `on_accept` and
        // fully authenticated (TLS + CredSSP) by the time it lands here — the
        // whole point of the auth-gated redesign is that nothing gets to
        // preempt the live session without proving itself first. So `pending`
        // carries a `NegotiatedCandidate`, not a raw stream: the next
        // iteration skips straight to `serve_negotiated` (Phase 2), with no
        // second `on_accept` call (would double-count a stateful handler like
        // `AuthGuardHandler`, see `NegotiationContext`'s doc comment) and no
        // renegotiation (already done).
        let mut pending: Option<(NegotiatedCandidate, SocketAddr)> = None;

        loop {
            // Transient per-iteration enum unifying the two connection sources for the
            // race below; boxing the large variants would add an allocation per accept.
            #[allow(clippy::large_enum_variant)]
            enum Entry {
                Fresh(TcpStream, SocketAddr),
                Negotiated(NegotiatedCandidate, SocketAddr),
            }

            let entry = match pending.take() {
                Some((candidate, peer)) => Entry::Negotiated(candidate, peer),
                None => {
                    let ev_receiver = Arc::clone(&self.ev_receiver);
                    let mut ev_receiver = ev_receiver.lock().await;
                    let (stream, peer) = tokio::select! {
                        Some(event) = ev_receiver.recv() => {
                            match event {
                                ServerEvent::Quit(reason) => {
                                    debug!("Got quit event {reason}");
                                    break;
                                }
                                ServerEvent::GetLocalAddr(tx) => {
                                    let _ = tx.send(self.local_addr);
                                }
                                ServerEvent::SetCredentials(creds) => {
                                    self.set_credentials(Some(creds));
                                }
                                ev => {
                                    debug!("Unexpected event {:?}", ev);
                                }
                            }
                            continue;
                        },
                        Ok((stream, peer)) = listener.accept() => {
                            drop(ev_receiver);
                            (stream, peer)
                        },
                        else => break,
                    };
                    Entry::Fresh(stream, peer)
                }
            };

            let peer = match &entry {
                Entry::Fresh(_, peer) | Entry::Negotiated(_, peer) => *peer,
            };
            debug!(?peer, "Received connection");

            // A `Negotiated` winner already ran (and passed) `on_accept` once,
            // as a candidate, inside the preemption race below — its
            // negotiation wouldn't even have started otherwise (see
            // `negotiate_candidate`'s caller). Re-running it here would be a
            // silent double-count for a STATEFUL handler — macrdp's own
            // `AuthGuardHandler::on_accept` records the accept toward its
            // per-source-IP rate-limit window and writes an audit line, so
            // calling it twice for the one physical connection would inflate
            // both without a second real attempt behind it.
            let accepted = matches!(entry, Entry::Negotiated(..))
                || self
                    .connection_handler
                    .as_ref()
                    .is_none_or(|h| h.borrow_mut().on_accept(peer));

            if !accepted {
                debug!(?peer, "Connection rejected by handler");
                if let Entry::Fresh(stream, _) = entry {
                    drop(stream);
                }
            } else {
                // (vendored, divergence 15) Sample the kernel's smoothed TCP
                // RTT here — the accept loop is the only point the concrete
                // TcpStream (hence the raw fd) is reachable; run_connection
                // takes a generic stream and wraps it immediately. The
                // handshake-seeded srtt is available right away and is what
                // link-adaptive consumers key on. 0 = unknown (kept on error).
                //
                // Not sampled for a `Negotiated` winner: its raw `TcpStream`
                // is already wrapped in TLS by the time it gets here (and its
                // negotiation ran against a `NegotiationContext` snapshot that
                // deliberately excludes `link_rtt_ms` — see that struct's doc
                // comment on why candidates don't touch shared connection
                // state). Accepted limitation, same shape as candidates
                // skipping the multitransport offer: a preemption-won
                // connection's RTT-dependent behavior (blank-recovery gating,
                // adaptive-bitrate seeding) falls back to whatever was last
                // observed rather than a fresh sample.
                if let Entry::Fresh(stream, _) = &entry
                    && let Some(cell) = &self.link_rtt_ms
                {
                    let rtt = tcp_srtt_ms(stream).unwrap_or(0);
                    cell.store(rtt, Ordering::Relaxed);
                    debug!(?peer, rtt_ms = rtt, "sampled TCP link RTT at accept");
                }
                let started = tokio::time::Instant::now();

                // (vendored, divergence 22/23) Serve this connection, but
                // keep accepting meanwhile: a NEW client must be able to take
                // over an existing session instead of hanging in the backlog
                // behind it (the accept loop used to `await` the whole
                // connection, so a second client saw a silent hang until the
                // first left). A candidate wins only once it clears
                // `on_accept` (the auth guard's rate-limit/lockout — checked
                // before it's even allowed to start negotiating, see the next
                // comment) AND fully authenticates via `negotiate_candidate`
                // (TLS + CredSSP) — an unauthenticated connection, or one that
                // merely started a handshake, can never preempt the live
                // session (Devolutions/IronRDP#1476 review + a real
                // regression this exact gap caused, see divergence 23's log
                // entry). Once a candidate wins, the in-flight connection
                // future is dropped (cancelled — its socket closes and every
                // per-connection resource unwinds via Drop, the same teardown
                // a client-side disconnect takes) and the newcomer is served
                // on the next iteration via `serve_negotiated`, which resumes
                // straight from its already-completed negotiation.
                //
                // Clone the shared `connection_handler` handle first (a cheap
                // `Rc` bump) and snapshot a `NegotiationContext`: `conn` below
                // captures `&mut self` for the whole race, so a candidate's
                // `on_accept` + negotiation — which needs to run concurrently
                // with `conn` — can't go through `self` directly.
                let handler = self.connection_handler.clone();
                let ctx = self.negotiation_context();
                // Same reason as `handler`/`ctx`: `conn` borrows `self` for the
                // whole race, so the eviction event has to be sent through a
                // clone of the sender rather than `self.ev_sender`.
                let self_ev_sender = self.ev_sender.clone();
                // Likewise moved out of `self` for the race, and written back
                // after it (the loop below both reads and re-arms it).
                let mut recently_evicted = self.recently_evicted.take();

                let outcome = {
                    let mut conn: core::pin::Pin<Box<dyn Future<Output = Result<()>> + '_>> = match entry {
                        Entry::Fresh(stream, _) => Box::pin(self.run_connection(stream)),
                        Entry::Negotiated(candidate, _) => Box::pin(self.serve_negotiated(candidate)),
                    };
                    let mut probe: core::pin::Pin<Box<dyn Future<Output = Option<NegotiatedCandidate>>>> =
                        Box::pin(core::future::pending());
                    let mut probing = false;
                    // The peer being probed — `negotiate_candidate`'s Output
                    // doesn't carry it (it's already known to the caller), so
                    // it's tracked alongside `probing` and reattached to
                    // whatever `probe` resolves to.
                    let mut probing_peer: Option<SocketAddr> = None;

                    loop {
                        // The select must only YIELD here, never mutate
                        // `probe`: its futures still borrow it inside the
                        // select expression.
                        let race = tokio::select! {
                            res = &mut conn => PreemptRace::Ended(res),
                            accepted = listener.accept(), if !probing => PreemptRace::Accepted(accepted),
                            candidate = &mut probe => PreemptRace::Probed(candidate),
                        };

                        match race {
                            // The session ended on its own. A candidate still
                            // being negotiated is NOT dropped on the floor —
                            // that would silently reset a legitimate client
                            // that happened to connect right as the old
                            // session left; finish its negotiation and serve
                            // it next if it authenticates.
                            PreemptRace::Ended(res) => {
                                if probing {
                                    // Not a preemption (nothing was taken from
                                    // anyone) — hand it straight to the next
                                    // iteration.
                                    let peer = probing_peer.expect("probing implies probing_peer is set");
                                    pending = probe.await.map(|candidate| (candidate, peer));
                                }
                                break (res, None);
                            }
                            PreemptRace::Accepted(Ok((next_stream, next_peer))) => {
                                // Gate the candidate through `on_accept`
                                // (the AuthGuardHandler's per-source-IP
                                // rate-limit/lockout) BEFORE it's allowed to
                                // start negotiating — and so before it can
                                // ever preempt anything. Without this, a
                                // candidate the guard would reject could
                                // still evict the live session just by
                                // authenticating, since `on_accept` would
                                // only run afterward once it became the next
                                // iteration's connection.
                                //
                                // Ahead of even that: a peer evicted moments
                                // ago may not bounce straight back and take
                                // the session again. Each attempt re-arms the
                                // window, so an auto-reconnect storm can never
                                // win — see `recently_evicted`. This is the
                                // net under `EvictedByOtherConnection`, which
                                // is what should normally stop the loop.
                                let bounced_back = match &mut recently_evicted {
                                    Some((ip, last_try)) if *ip == next_peer.ip() => {
                                        let within = last_try.elapsed() < REPREEMPT_COOLDOWN;
                                        if within {
                                            *last_try = Instant::now();
                                        }
                                        within
                                    }
                                    _ => false,
                                };
                                let candidate_accepted = !bounced_back
                                    && handler.as_ref().is_none_or(|h| h.borrow_mut().on_accept(next_peer));
                                if candidate_accepted {
                                    probing = true;
                                    probing_peer = Some(next_peer);
                                    probe = Box::pin(negotiate_candidate(&ctx, next_stream, next_peer));
                                } else if bounced_back {
                                    info!(
                                        ?next_peer,
                                        "ignoring a reconnect from the peer just evicted — it is auto-reconnecting \
                                         into the session that replaced it; not letting it bounce back"
                                    );
                                    drop(next_stream);
                                } else {
                                    debug!(
                                        ?next_peer,
                                        "candidate connection rejected by handler while a session was live"
                                    );
                                    drop(next_stream);
                                }
                            }
                            PreemptRace::Accepted(Err(error)) => {
                                warn!(?error, "accept failed while a session was live");
                            }
                            PreemptRace::Probed(candidate) => {
                                probing = false;
                                probe = Box::pin(core::future::pending());
                                let new_peer = probing_peer.take().expect("probing_peer set alongside probing");
                                match candidate {
                                    Some(candidate) => {
                                        // The candidate has authenticated and is
                                        // taking over. Tell the incumbent WHY
                                        // it's going away, and give it a moment
                                        // to actually send that + shut down,
                                        // instead of cancelling it outright: a
                                        // client dropped with no reason
                                        // auto-reconnects off the ARC cookie
                                        // and preempts straight back, forever
                                        // (see `EvictedByOtherConnection`).
                                        info!(
                                            old_peer = ?peer,
                                            ?new_peer,
                                            "an authenticated candidate connected — evicting the existing session in its favor"
                                        );
                                        let _ = self_ev_sender.send(ServerEvent::EvictedByOtherConnection);
                                        // Arm the net against this peer
                                        // bouncing straight back in.
                                        recently_evicted = Some((peer.ip(), Instant::now()));
                                        // Keep polling the incumbent so it can
                                        // observe the event, write the PDU and
                                        // return on its own. Bounded, because a
                                        // wedged/half-dead peer must not stall
                                        // the takeover — on timeout `conn` is
                                        // simply dropped, i.e. exactly the hard
                                        // cancellation this replaces.
                                        match tokio::time::timeout(EVICTION_GRACE, &mut conn).await {
                                            Ok(res) => break (res, Some((candidate, new_peer))),
                                            Err(_) => {
                                                debug!(
                                                    old_peer = ?peer,
                                                    "evicted session did not wind down in time — cancelling it"
                                                );
                                                break (Ok(()), Some((candidate, new_peer)));
                                            }
                                        }
                                    }
                                    None => {
                                        debug!(?new_peer, "candidate did not authenticate — live session unaffected");
                                    }
                                }
                            }
                        }
                    }
                };

                let (result, preempted_by) = outcome;
                let duration = started.elapsed();
                self.recently_evicted = recently_evicted;

                // The eviction itself is logged inside the race, where the
                // decision is made; here it's just carried to the next
                // iteration.
                if let Some((candidate, new_peer)) = preempted_by {
                    pending = Some((candidate, new_peer));
                } else {
                    // This session ended on its own terms rather than being
                    // replaced, so nobody is mid-eviction any more — let a
                    // previously-evicted peer connect again freely.
                    self.recently_evicted = None;
                }

                if let Err(ref error) = result {
                    error!(?error, "Connection error");
                }

                self.static_channels = StaticChannelSet::new();

                // (M3c) Reset per-connection UDP-multitransport state that is
                // otherwise only ever SET, never cleared — so a reconnect to
                // this same persistent RdpServer starts clean. Critically
                // `egfx_on_udp`: left true from the previous connection, the
                // next connection routes EGFX over a UDP tunnel that its OWN
                // Soft-Sync hasn't bound yet → frames are dropped and the
                // client sees a blank/black desktop on reconnect. Resetting it
                // keeps EGFX on TCP until the new connection's tunnel binds and
                // re-fires Soft-Sync (clean migration; and a correct TCP
                // fallback if the new tunnel never binds). The lossy-audio
                // counters + the on-lossy handle must likewise restart.
                // (`multitransport_migration`, `udp_tunnel_bound`, and the
                // inbound rx ARE refreshed per connection at the offer site, so
                // only these only-set-never-reset flags need clearing here.)
                #[cfg(feature = "multitransport")]
                {
                    self.egfx_on_udp = false;
                    self.lossy_audio_block_no = 0;
                    self.lossy_audio_streaming = false;
                    if let Some(handle) = &self.egfx_on_lossy_handle {
                        handle.store(false, std::sync::atomic::Ordering::Relaxed);
                    }
                    if let Some(handle) = &self.egfx_on_udp_handle {
                        handle.store(false, std::sync::atomic::Ordering::Relaxed);
                    }
                    // Clear the watchdog's de-migrate request so a fresh
                    // connection retries UDP instead of instantly routing the
                    // newly-migrated EGFX straight back to TCP.
                    if let Some(handle) = &self.demigrate_request {
                        handle.store(false, std::sync::atomic::Ordering::Relaxed);
                    }
                    // Retire this connection's tunnel: lower the shared bound
                    // flag so the listener's tunnel-death check (which skips
                    // lowered flags) treats the now-abandoned tunnel as benign
                    // teardown — it just ages out via the idle GC. Without
                    // this, every ended session's tunnel "dies" ~30 s later
                    // and starts the multitransport-offer COOLDOWN, silently
                    // downgrading the next 10 min of healthy-LAN connections
                    // to plain TCP (observed live 2026-07-06 after a
                    // blank-recovery drop). A tunnel that wedges while its
                    // session is ALIVE still declares death + cooldown exactly
                    // as before (its flag is still up when the check runs).
                    if let Some(flag) = &self.udp_tunnel_bound {
                        flag.store(false, std::sync::atomic::Ordering::Relaxed);
                    }
                    // Evict the connection's offer cookie no matter how it
                    // ended. Eviction used to be keyed off `MigrationState`
                    // (set only at activation), so a connection dying
                    // earlier — mstsc's cert-prompt broken pipe, CredSSP
                    // failures, probes — leaked one registry entry per
                    // attempt forever, and a LATE tunnel bind (the client's
                    // UDP handshake outliving a fast session end) could
                    // still consume the stale cookie and re-raise the
                    // retired flag → zombie peer → spurious death + a
                    // 10-min offer cooldown on a healthy setup.
                    if let Some(cookie) = self.current_offer_cookie.take()
                        && let Some(registry) = self.multitransport_cookies.as_ref()
                    {
                        registry.remove(&cookie);
                    }
                }

                if let Some(handler) = self.connection_handler.as_ref() {
                    let action = handler
                        .borrow_mut()
                        .on_disconnected(peer, duration, result.as_ref().err());
                    if action == PostConnectionAction::Stop {
                        debug!(?peer, "Handler requested stop after disconnect");
                        break;
                    }
                }
            }
        }

        Ok(())
    }

    pub fn get_svc_processor<T: SvcProcessor + 'static>(&mut self) -> Option<&mut T> {
        self.static_channels
            .get_by_type_mut::<T>()
            .and_then(|svc| svc.channel_processor_downcast_mut())
    }

    pub fn get_channel_id_by_type<T: SvcProcessor + 'static>(&self) -> Option<StaticChannelId> {
        self.static_channels.get_channel_id_by_type::<T>()
    }

    async fn dispatch_pdu(
        &mut self,
        action: Action,
        bytes: bytes::BytesMut,
        writer: &mut impl FramedWrite,
        io_channel_id: u16,
        user_channel_id: u16,
    ) -> Result<RunState> {
        match action {
            Action::FastPath => {
                let input = decode(&bytes)?;
                self.handle_fastpath(input).await;
            }

            Action::X224 => {
                if self
                    .handle_x224(writer, io_channel_id, user_channel_id, &bytes)
                    .await
                    .context("X224 input error")?
                {
                    debug!("Got disconnect request");
                    return Ok(RunState::Disconnect);
                }
            }
        }

        Ok(RunState::Continue)
    }

    async fn dispatch_display_update(
        update: DisplayUpdate,
        writer: &mut impl FramedWrite,
        user_channel_id: u16,
        io_channel_id: u16,
        buffer: &mut Vec<u8>,
        mut encoder: UpdateEncoder,
    ) -> Result<(RunState, UpdateEncoder)> {
        if let DisplayUpdate::Resize(desktop_size) = update {
            debug!(?desktop_size, "Display resize");
            encoder.set_desktop_size(desktop_size);
            deactivate_all(io_channel_id, user_channel_id, writer).await?;
            return Ok((RunState::DeactivationReactivation { desktop_size }, encoder));
        }

        let mut encoder_iter = encoder.update(update);
        loop {
            let Some(fragmenter) = encoder_iter.next().await else {
                break;
            };

            let mut fragmenter = fragmenter.context("error while encoding")?;
            if fragmenter.size_hint() > buffer.len() {
                buffer.resize(fragmenter.size_hint(), 0);
            }

            while let Some(len) = fragmenter.next(buffer) {
                writer
                    .write_all(&buffer[..len])
                    .await
                    .context("failed to write display update")?;
            }
        }

        Ok((RunState::Continue, encoder))
    }

    async fn dispatch_server_events(
        &mut self,
        events: &mut Vec<ServerEvent>,
        writer: &mut impl FramedWrite,
        io_channel_id: u16,
        user_channel_id: u16,
    ) -> Result<RunState> {
        // Audio dispatch (`RdpsndServerMessage::Wave`) was carved out of
        // this function into the dedicated `dispatch_audio` task in
        // `client_loop`, with task-local audio-lag tracking (resync +
        // drop-oldest cap, same MAX_LAG_MS = 200 / RESYNC_DEFICIT_MS =
        // 300 model). Wave events arrive on a separate bounded mpsc
        // channel (`AudioWave` / `SoundServerFactory::set_audio_sender`)
        // and never reach this function. If a Wave event DOES somehow
        // land in the unified `ServerEvent` queue (e.g., an audio
        // backend that hasn't overridden `set_audio_sender`), the match
        // arm below logs and drops it — the lag model isn't in this
        // function anymore so we couldn't service it correctly anyway.
        // Reorder this batch so CLIPRDR events are written BEFORE any
        // EGFX video frames queued behind them. With --enable-h264, EGFX
        // frames flow continuously and dominate the event channel, and
        // the underlying socket writer is shared across all channels;
        // without this reordering, a small CLIPRDR FileContentsResponse
        // can sit behind dozens of large video frames every batch, which
        // throttles a clipboard file copy to a crawl and freezes Windows
        // Explorer's synchronous paste read (the "large Mac→Windows file
        // copy hangs under --enable-h264" bug).
        //
        // IMPORTANT: audio is intentionally NOT moved with clipboard.
        // An earlier version of this patch lumped audio in with clipboard
        // as "non-EGFX" and shipped all of them before any video. That
        // burst-shipped each batch's worth of audio in a clump every
        // ~100–170 ms (one batch interval) instead of the natural ~21 ms
        // per-wave cadence; the client's adaptive jitter buffer extended
        // to absorb the new burstiness and added ~150–300 ms of steady-
        // state playback latency. Leaving audio in arrival order keeps
        // packets arriving at the client at a steady cadence so the
        // jitter buffer doesn't grow.
        //
        // The partition is STABLE: relative order within each group is
        // preserved, so H.264 frames stay in their original sequential
        // order (required by the inter-frame codec chain), and the
        // audio wave-drop logic below (which targets the OLDEST queued
        // waves) still sees them in arrival order. Clipboard and video
        // are independent channels on the wire, so moving clipboard
        // ahead of video within a batch does NOT violate any ordering
        // invariant on either channel.
        // (Extended) RDPDR is a third, LOWEST-priority tier. A large drive
        // transfer (a big DeviceWrite PDU when copying TO the redirected drive)
        // would otherwise be written ahead of / interleaved with EGFX frames in
        // the same batch, holding the shared socket writer and stuttering the
        // video — the same starvation shape as the clipboard case above, but
        // with video as the victim and RDPDR as the bulk hog. So order the batch
        // clipboard → {EGFX + everything else} → RDPDR. RDPDR rides its own SVC
        // channel, so reordering it within a batch breaks no on-wire ordering,
        // and the partition stays STABLE within each tier (EGFX frames keep
        // their sequential order for the inter-frame codec chain). Triggered
        // whenever video shares a batch with clipboard and/or RDPDR.
        #[cfg(feature = "egfx")]
        {
            let has_egfx = events.iter().any(|e| matches!(e, ServerEvent::Egfx(_)));
            let has_clip_or_rdpdr = events.iter().any(|e| {
                matches!(
                    e,
                    ServerEvent::Clipboard(_) | ServerEvent::ClipboardFileCopy(_) | ServerEvent::Rdpdr(_)
                )
            });
            if has_egfx && has_clip_or_rdpdr {
                let mut clipboard: Vec<ServerEvent> = Vec::new();
                let mut middle: Vec<ServerEvent> = Vec::with_capacity(events.len());
                let mut rdpdr: Vec<ServerEvent> = Vec::new();
                for ev in events.drain(..) {
                    match ev {
                        ServerEvent::Clipboard(_) | ServerEvent::ClipboardFileCopy(_) => clipboard.push(ev),
                        ServerEvent::Rdpdr(_) => rdpdr.push(ev),
                        _ => middle.push(ev),
                    }
                }
                events.extend(clipboard); // 1: not starved by video
                events.extend(middle); // 2: EGFX video + the rest
                events.extend(rdpdr); // 3: bulk drive writes yield to video
            }
        }
        for event in events.drain(..) {
            trace!(?event, "Dispatching");
            match event {
                ServerEvent::Quit(reason) => {
                    debug!("Got quit event: {reason}");
                    return Ok(RunState::Disconnect);
                }
                // (vendored, divergence 23) Session takeover: tell the client
                // WHY it's being disconnected before dropping it, so it does
                // not treat this as an unexpected drop and auto-reconnect off
                // the ARC cookie (which ping-pongs forever against the
                // preempting client — see the variant's doc comment).
                ServerEvent::EvictedByOtherConnection => {
                    debug!("evicting this connection — another client took the session over");
                    let pdu =
                        rdp::headers::ShareDataPdu::ServerSetErrorInfo(rdp::server_error_info::ServerSetErrorInfoPdu(
                            rdp::server_error_info::ErrorInfo::ProtocolIndependentCode(
                                rdp::server_error_info::ProtocolIndependentCode::DisconnectedByOtherconnection,
                            ),
                        ));
                    // Best-effort: if the evicted peer's socket is already
                    // half-dead the write fails, which is fine — it's leaving
                    // either way, and the caller falls back to cancelling it.
                    match encode_share_data_pdu(pdu, io_channel_id, user_channel_id) {
                        Ok(bytes) => {
                            if let Err(error) = writer.write_all(&bytes).await {
                                debug!(?error, "could not send the eviction reason; disconnecting anyway");
                            }
                        }
                        Err(error) => {
                            warn!(?error, "could not encode the eviction reason; disconnecting anyway");
                        }
                    }
                    return Ok(RunState::Disconnect);
                }
                ServerEvent::GetLocalAddr(tx) => {
                    let _ = tx.send(self.local_addr);
                }
                ServerEvent::SetCredentials(creds) => {
                    self.set_credentials(Some(creds));
                }
                ServerEvent::Rdpsnd(s) => {
                    let Some(rdpsnd) = self.get_svc_processor::<RdpsndServer>() else {
                        warn!("No rdpsnd channel, dropping event");
                        continue;
                    };
                    let msgs = match s {
                        RdpsndServerMessage::Wave(_, _) => {
                            // Wave events should reach `dispatch_audio` via
                            // the dedicated audio channel, not this unified
                            // path. If we got one here, the audio backend
                            // hasn't overridden `set_audio_sender` — drop
                            // gracefully and warn once so the maintainer
                            // notices.
                            warn!(
                                "Wave event on unified ServerEvent channel; backend should override set_audio_sender"
                            );
                            continue;
                        }
                        RdpsndServerMessage::SetVolume { left, right } => rdpsnd.set_volume(left, right),
                        RdpsndServerMessage::Close => rdpsnd.close(),
                        RdpsndServerMessage::Error(error) => {
                            error!(?error, "Handling rdpsnd event");
                            continue;
                        }
                    }
                    .context("failed to send rdpsnd event")?;
                    let channel_id = self
                        .get_channel_id_by_type::<RdpsndServer>()
                        .context("SVC channel not found")?;
                    let data = server_encode_svc_messages(msgs.into(), channel_id, user_channel_id)?;
                    writer.write_all(&data).await?;
                }
                ServerEvent::Clipboard(c) => {
                    let Some(cliprdr) = self.get_svc_processor::<CliprdrServer>() else {
                        warn!("No clipboard channel, dropping event");
                        continue;
                    };
                    let msgs = match c {
                        ClipboardMessage::SendInitiateCopy(formats) => cliprdr.initiate_copy(&formats),
                        ClipboardMessage::SendFormatData(data) => cliprdr.submit_format_data(data),
                        ClipboardMessage::SendInitiatePaste(format) => cliprdr.initiate_paste(format),
                        ClipboardMessage::SendFileContentsRequest(request) => cliprdr.request_file_contents(request),
                        ClipboardMessage::SendFileContentsResponse(response) => cliprdr.submit_file_contents(response),
                        // (pin-bump a5d1c682) upstream added this variant. macrdp drives
                        // file copy via its own ServerEvent::ClipboardFileCopy, but the
                        // match must cover it; same call the upstream arm makes.
                        ClipboardMessage::SendInitiateFileCopy(files) => cliprdr.initiate_file_copy(files),
                        ClipboardMessage::Error(error) => {
                            error!(?error, "Handling clipboard event");
                            continue;
                        }
                    }
                    .context("failed to send clipboard event")?;
                    let channel_id = self
                        .get_channel_id_by_type::<CliprdrServer>()
                        .context("SVC channel not found")?;
                    let data = server_encode_svc_messages(msgs.into(), channel_id, user_channel_id)?;
                    writer.write_all(&data).await?;
                }
                ServerEvent::ClipboardFileCopy(files) => {
                    let Some(cliprdr) = self.get_svc_processor::<CliprdrServer>() else {
                        warn!("No clipboard channel, dropping file-copy event");
                        continue;
                    };
                    debug!(
                        file_count = files.len(),
                        "ClipboardFileCopy: calling initiate_file_copy"
                    );
                    let msgs = match cliprdr.initiate_file_copy(files) {
                        Ok(m) => m,
                        Err(e) => {
                            // Don't propagate: a failed initiate_file_copy
                            // (e.g. STREAM_FILECLIP_ENABLED not negotiated)
                            // shouldn't tear down the whole RDP session.
                            warn!(
                                ?e,
                                "initiate_file_copy failed; file paste will fail until the next copy"
                            );
                            continue;
                        }
                    };
                    let channel_id = self
                        .get_channel_id_by_type::<CliprdrServer>()
                        .context("SVC channel not found")?;
                    let data = server_encode_svc_messages(msgs.into(), channel_id, user_channel_id)?;
                    writer.write_all(&data).await?;
                }
                ServerEvent::Echo(msg) => match msg {
                    EchoServerMessage::SendRequest { payload } => {
                        let Some(drdynvc) = self.get_svc_processor::<dvc::DrdynvcServer>() else {
                            warn!("No drdynvc channel, dropping ECHO request");
                            continue;
                        };

                        let Some(echo_channel_id) = drdynvc.get_channel_id_by_type::<EchoDvcBridge>() else {
                            warn!("No ECHO dynamic channel, dropping ECHO request");
                            continue;
                        };

                        if !drdynvc.is_channel_opened(echo_channel_id) {
                            warn!("ECHO dynamic channel not yet opened, dropping ECHO request");
                            continue;
                        }

                        self.echo_handle.on_request_sent(&payload);

                        let request = build_echo_request(payload)?;
                        let messages =
                            dvc::encode_dvc_messages(echo_channel_id, vec![request], ChannelFlags::SHOW_PROTOCOL)?;

                        let drdynvc_channel_id = self
                            .get_channel_id_by_type::<dvc::DrdynvcServer>()
                            .context("DRDYNVC channel not found")?;

                        let data = server_encode_svc_messages(messages, drdynvc_channel_id, user_channel_id)?;
                        writer.write_all(&data).await?;
                    }
                },
                ServerEvent::Rdpdr(msg) => match msg {
                    crate::RdpdrServerMessage::SendMessages(messages) => {
                        let Some(channel_id) = self.get_channel_id_by_type::<crate::RdpdrServer>() else {
                            warn!("No RDPDR channel, dropping device-I/O request");
                            continue;
                        };
                        let data = server_encode_svc_messages(messages, channel_id, user_channel_id)?;
                        writer.write_all(&data).await?;
                    }
                },
                ServerEvent::Urbdrc(msg) => match msg {
                    // (divergence 16) The URBDRC processor announced a device and needs
                    // a per-device DVC. Only the event loop can reach DrdynvcServer, so
                    // open the channel here and ship the resulting CreateRequest.
                    crate::UrbdrcServerMessage::OpenDeviceChannel => {
                        // Clone the sender + presenting-side callback BEFORE the
                        // &mut self borrow below; the new device processor keeps them
                        // to ship transfers and to hand the device to the macOS side.
                        let sender = self.ev_sender.clone();
                        let device_cb = self.usb_factory.as_deref().and_then(|f| f.device_callback());
                        let create_msg = {
                            let Some(drdynvc) = self.get_svc_processor::<dvc::DrdynvcServer>() else {
                                warn!("No DRDYNVC channel, cannot open URBDRC device channel");
                                continue;
                            };
                            match drdynvc.create_channel(crate::rdpeusb::UrbdrcDeviceProcessor::new(sender, device_cb))
                            {
                                Ok(m) => m,
                                Err(e) => {
                                    warn!(error = %e, "failed to create URBDRC device channel");
                                    continue;
                                }
                            }
                        };
                        let Some(drdynvc_channel_id) = self.get_channel_id_by_type::<dvc::DrdynvcServer>() else {
                            warn!("No DRDYNVC channel id, dropping URBDRC device channel");
                            continue;
                        };
                        let data = server_encode_svc_messages(vec![create_msg], drdynvc_channel_id, user_channel_id)?;
                        writer.write_all(&data).await?;
                    }
                    // A UsbHandle wants to ship a transfer request on an open device
                    // channel. DVC-frame the messages and write them (mirrors Echo).
                    crate::UrbdrcServerMessage::SendMessages { channel_id, messages } => {
                        // Tolerate an encode error instead of tearing the session down: a
                        // malformed/unsupported URB (e.g. a control request the codec
                        // rejects) must never kill an opt-in feature's whole connection —
                        // same principle as the URBDRC process() decode-tolerance. Drop the
                        // offending transfer and keep the session alive. (A real socket
                        // error from write_all below stays fatal — that IS a dead link.)
                        let dvc_messages = match dvc::encode_dvc_messages(channel_id, messages, ChannelFlags::empty()) {
                            Ok(m) => m,
                            Err(e) => {
                                warn!(error = %e, "URBDRC: failed to encode a transfer request — dropping it (session kept alive)");
                                continue;
                            }
                        };
                        let Some(drdynvc_channel_id) = self.get_channel_id_by_type::<dvc::DrdynvcServer>() else {
                            warn!("No DRDYNVC channel, dropping URBDRC transfer");
                            continue;
                        };
                        let data = match server_encode_svc_messages(dvc_messages, drdynvc_channel_id, user_channel_id) {
                            Ok(d) => d,
                            Err(e) => {
                                warn!(error = %e, "URBDRC: failed to SVC-encode a transfer request — dropping it (session kept alive)");
                                continue;
                            }
                        };
                        writer.write_all(&data).await?;
                    }
                },
                ServerEvent::Camera(msg) => match msg {
                    // (divergence 19, Phase 1) The MS-RDPECAM enumerator announced a
                    // camera and needs a per-device DVC opened with the CLIENT-given
                    // channel name. Only the event loop reaches DrdynvcServer. The
                    // device processor drives the whole negotiation + sample pull loop
                    // from its own start()/process() return values, so no sender is
                    // threaded into it. Mirrors the URBDRC OpenDeviceChannel arm.
                    crate::CameraServerMessage::OpenDeviceChannel { channel_name, version } => {
                        // Build the per-device decode/present sink from the camera
                        // factory (Phase 2+); None keeps Phase-1 log-and-drop.
                        let sink = self.camera_factory.as_deref().and_then(|f| f.build_sample_sink());
                        let create_msg = {
                            let Some(drdynvc) = self.get_svc_processor::<dvc::DrdynvcServer>() else {
                                warn!("No DRDYNVC channel, cannot open MS-RDPECAM device channel");
                                continue;
                            };
                            let proc = crate::RdCameraDeviceProcessor::new(channel_name, version, sink);
                            match drdynvc.create_channel(proc) {
                                Ok(m) => m,
                                Err(e) => {
                                    warn!(error = %e, "failed to create MS-RDPECAM device channel");
                                    continue;
                                }
                            }
                        };
                        let Some(drdynvc_channel_id) = self.get_channel_id_by_type::<dvc::DrdynvcServer>() else {
                            warn!("No DRDYNVC channel id, dropping MS-RDPECAM device channel");
                            continue;
                        };
                        let data = server_encode_svc_messages(vec![create_msg], drdynvc_channel_id, user_channel_id)?;
                        writer.write_all(&data).await?;
                    }
                },
                #[cfg(feature = "egfx")]
                ServerEvent::Egfx(msg) => match msg {
                    EgfxServerMessage::SendMessages { messages } => {
                        // (M5c) Once EGFX has been migrated onto the UDP tunnel
                        // (`egfx_on_udp`, only under MACRDP_UDP_MIGRATE_EGFX), route
                        // its frames over the tunnel instead of the TCP drdynvc
                        // channel. Default path is unchanged TCP.
                        #[cfg(feature = "multitransport")]
                        if self.egfx_on_udp {
                            // EGFX-over-UDP → TCP watchdog: the H.264 pipeline sets
                            // `demigrate_request` when it detects the reliable UDP
                            // tunnel has wedged (acks silent while shipping). Flip
                            // routing back to the TCP DRDYNVC channel for the rest of
                            // this session and fall through to the TCP path — mstsc
                            // renders EGFX on TCP after a Soft-Sync (Spike A). The flip
                            // also clears the H.264 #89 backpressure gate via the
                            // shared egfx_on_udp handle. One-way; reset on reconnect.
                            if self
                                .demigrate_request
                                .as_ref()
                                .is_some_and(|h| h.load(std::sync::atomic::Ordering::Relaxed))
                            {
                                self.egfx_on_udp = false;
                                if let Some(handle) = &self.egfx_on_udp_handle {
                                    handle.store(false, std::sync::atomic::Ordering::Relaxed);
                                }
                                warn!(
                                    "EGFX-over-UDP reliable tunnel wedged — watchdog de-migrating EGFX to \
                                     TCP DRDYNVC for the rest of this session"
                                );
                                // fall through to the TCP DRDYNVC path below
                            } else {
                                self.route_dvc_over_udp(messages)?;
                                continue;
                            }
                        }
                        let drdynvc_channel_id = self
                            .get_channel_id_by_type::<dvc::DrdynvcServer>()
                            .context("DRDYNVC channel not found")?;
                        let data = server_encode_svc_messages(messages, drdynvc_channel_id, user_channel_id)?;
                        writer.write_all(&data).await?;
                        // (M5c) Now that EGFX is actively shipping (its DVC channel
                        // is open) AND the UDP tunnel is bound, fire the Soft-Sync
                        // request once — the cue to migrate EGFX onto the tunnel.
                        #[cfg(feature = "multitransport")]
                        self.maybe_soft_sync_on_egfx(writer, user_channel_id).await?;
                    }
                },
                ServerEvent::AutoDetectRttRequest => {
                    if let Some(ref mut ad) = self.autodetect {
                        ad.expire_stale_probes(crate::autodetect::RTT_PROBE_MAX_AGE);
                        let request = ad.send_rtt_request();
                        // (pin-bump a5d1c682) Autodetect moved out of ShareDataPdu into
                        // rdp::autodetect and onto the MCS message channel. macrdp does
                        // NOT use RDP network-autodetect (div-15 kernel TCP RTT replaces
                        // it), so self.autodetect is never enabled and this is unreached;
                        // adapt to the new PDU type so it compiles. No message channel is
                        // plumbed here — harmless as it never runs.
                        let pdu = rdp::autodetect::AutoDetectReqPdu::new(request);
                        let mcs_pdu = SendDataIndication {
                            initiator_id: user_channel_id,
                            channel_id: io_channel_id,
                            user_data: encode_vec(&pdu)?.into(),
                        };
                        let data = encode_vec(&X224(mcs_pdu))?;
                        writer.write_all(&data).await?;
                    }
                }
            }
        }

        Ok(RunState::Continue)
    }

    async fn client_loop<R, W>(
        &mut self,
        reader: &mut Framed<R>,
        writer: &mut Framed<W>,
        io_channel_id: u16,
        user_channel_id: u16,
        mut encoder: UpdateEncoder,
    ) -> Result<RunState>
    where
        R: FramedRead,
        W: FramedWrite,
    {
        debug!("Starting client loop");
        let mut display_updates = self.display.lock().await.updates().await?;
        let mut writer = SharedWriter::new(writer);
        let mut display_writer = writer.clone();
        let mut event_writer = writer.clone();
        let mut audio_writer = writer.clone();
        let ev_receiver = Arc::clone(&self.ev_receiver);
        let audio_receiver = Arc::clone(&self.audio_receiver);
        // (M5c step 3b) Take this connection's inbound-tunnel receiver before `self`
        // is moved into the shared Rc; `dispatch_tunnel_inbound` drains it.
        #[cfg(feature = "multitransport")]
        let tunnel_inbound = self.multitransport_tunnel_inbound_rx.take();
        let s = Rc::new(Mutex::new(self));

        let this = Rc::clone(&s);
        let dispatch_pdu = async move {
            loop {
                let (action, bytes) = reader.read_pdu().await?;
                let mut this = this.lock().await;
                match this
                    .dispatch_pdu(action, bytes, &mut writer, io_channel_id, user_channel_id)
                    .await?
                {
                    RunState::Continue => continue,
                    state => break Ok(state),
                }
            }
        };

        let dispatch_display = async move {
            let mut buffer = vec![0u8; 4096];

            loop {
                match display_updates.next_update().await {
                    Ok(Some(update)) => {
                        match Self::dispatch_display_update(
                            update,
                            &mut display_writer,
                            user_channel_id,
                            io_channel_id,
                            &mut buffer,
                            encoder,
                        )
                        .await?
                        {
                            (RunState::Continue, enc) => {
                                encoder = enc;
                                continue;
                            }
                            (state, _) => {
                                break Ok(state);
                            }
                        }
                    }
                    Ok(None) => {
                        break Ok(RunState::Disconnect);
                    }
                    Err(error) => {
                        warn!(error = format!("{error:#}"), "next_updated failed");
                    }
                }
            }
        };

        // Audio dispatch carved out from `dispatch_events` so a sustained
        // inbound cliprdr stream (e.g., large `--lazy-paste` Windows→Mac
        // transfer) can't starve audio output for seconds at a time.
        //
        // The audio backend (e.g., `MacRdpsnd`) sends `Wave` PDUs over a
        // dedicated bounded `mpsc::channel` (see `SoundServerFactory::
        // set_audio_sender`) instead of the unified `ServerEvent` channel.
        // This loop consumes that channel and writes each wave to the
        // socket independently of dispatch_pdu/events.
        //
        // The Self lock is held BRIEFLY (microseconds) per wave — just
        // long enough to call `rdpsnd.wave(data, ts)` and look up the
        // SVC channel id. The actual `writer.write_all` happens outside
        // the Self lock, on the shared writer (which still serializes
        // with display/event writes, but that's a much shorter critical
        // section than dispatch_events's lock-then-drain-batch pattern).
        //
        // Audio-lag tracking (resync + drop-oldest) is now task-local —
        // moved out of `RdpServer::{audio_shipped_ms, audio_clock_start}`
        // which become dead state. Per-wave duration is derived from the
        // byte count, not a hardcoded constant, so the model stays
        // accurate regardless of resampler chunk size.
        let this = Rc::clone(&s);
        let mut audio_receiver = audio_receiver.lock().await;
        let dispatch_audio = async move {
            // Task-local audio-lag state. Reset per connection because the
            // task is spawned fresh inside client_loop.
            let mut audio_shipped_ms = 0.0_f64;
            let mut audio_clock_start: Option<Instant> = None;

            // Same constants as the prior on-Self model. See the
            // "Cross-batch audio-lag control" comment block (formerly in
            // dispatch_server_events) for the rationale.
            const MAX_LAG_MS: f64 = 200.0;
            const RESYNC_DEFICIT_MS: f64 = 300.0;
            // 16-bit stereo PCM at 44.1 kHz = 4 bytes/frame × 44 100 = 176 400 B/s,
            // i.e. 176.4 bytes per ms of audio. Matches `src/audio.rs::our_format()`
            // in the macrdp tree. Update if the audio backend's format changes.
            const BYTES_PER_MS: f64 = 176.4;

            loop {
                let (data, ts, duration_ms) = match audio_receiver.recv().await {
                    Some(wave) => wave,
                    None => {
                        debug!("audio channel closed; stopping audio dispatch");
                        break Ok(RunState::Disconnect);
                    }
                };

                // PCM waves leave `duration_ms` None and we derive the
                // playback time from byte length (BYTES_PER_MS). A compressed
                // codec (AAC) carries its own duration because its byte length
                // is unrelated to playback time — a ~120-byte AU is ~23 ms, so
                // the bytes-to-ms assumption would otherwise read the buffer as
                // near-empty and disable the drop/resync lag control entirely.
                let wave_ms = duration_ms.unwrap_or_else(|| data.len() as f64 / BYTES_PER_MS);

                // Cross-batch audio-lag control. Identical model to the
                // formerly-on-Self version. Drop stale waves and resync
                // the clock if the writer fell behind.
                let now = Instant::now();
                let start = *audio_clock_start.get_or_insert(now);
                let real_elapsed_ms = (now - start).as_secs_f64() * 1000.0;
                let deficit_ms = real_elapsed_ms - audio_shipped_ms;
                if deficit_ms > RESYNC_DEFICIT_MS {
                    debug!(
                        target: "audio_backlog",
                        deficit_ms,
                        "writer stalled; resyncing audio clock to live"
                    );
                    audio_shipped_ms = real_elapsed_ms;
                }
                let projected_buffer_ms = audio_shipped_ms + wave_ms - real_elapsed_ms;
                if projected_buffer_ms > MAX_LAG_MS {
                    debug!(
                        target: "audio_backlog",
                        projected_buffer_ms,
                        wave_ms,
                        "dropping wave (projected buffer over MAX_LAG_MS)"
                    );
                    continue;
                }

                // Briefly lock Self to build the Wave PDU and look up the
                // channel id. RdpsndServer::wave() bumps a private block_no and
                // produces SVC messages — microseconds of work. Lock released
                // before the (potentially long) socket write.
                let mut this = this.lock().await;

                // (2b-iv-B) Once the lossy-audio-over-UDP path is live (reliable
                // handshake negotiated + tunnel bound + lossy DVC open), ship the
                // wave as a Wave2 PDU on AUDIO_PLAYBACK_LOSSY_DVC over the lossy/DTLS
                // tunnel and SKIP the static rdpsnd write — exactly one playback path
                // at a time (no double-play). The tunnel send is best-effort and
                // non-blocking, so no socket-write await here.
                #[cfg(feature = "multitransport")]
                if let Some((format_no, lossy_id)) = this.lossy_audio_target() {
                    let block_no = this.lossy_audio_block_no;
                    this.lossy_audio_block_no = this.lossy_audio_block_no.wrapping_add(1);
                    let res = this.route_lossy_audio_wave(block_no, ts, format_no, lossy_id, data);
                    drop(this);
                    if let Err(e) = res {
                        warn!(error = %e, "lossy audio wave route over UDP tunnel failed");
                    }
                    audio_shipped_ms += wave_ms;
                    continue;
                }

                // Static rdpsnd over TCP (default; also the pre-negotiation path
                // before the lossy DVC is live).
                let encoded = {
                    let Some(rdpsnd) = this.get_svc_processor::<RdpsndServer>() else {
                        warn!("No rdpsnd channel, dropping wave");
                        continue;
                    };
                    let msgs = match rdpsnd.wave(data, ts) {
                        Ok(m) => m,
                        Err(err) => {
                            warn!(?err, "rdpsnd.wave failed");
                            continue;
                        }
                    };
                    let Some(channel_id) = this.get_channel_id_by_type::<RdpsndServer>() else {
                        warn!("rdpsnd SVC channel id missing");
                        continue;
                    };
                    server_encode_svc_messages(msgs.into(), channel_id, user_channel_id)?
                };
                drop(this);

                audio_writer.write_all(&encoded).await?;
                audio_shipped_ms += wave_ms;
            }
        };

        let this = Rc::clone(&s);
        let mut ev_receiver = ev_receiver.lock().await;
        let dispatch_events = async move {
            let mut events = Vec::with_capacity(100);
            loop {
                let nevents = ev_receiver.recv_many(&mut events, 100).await;
                if nevents == 0 {
                    debug!("No sever events.. stopping");
                    break Ok(RunState::Disconnect);
                }
                while let Ok(ev) = ev_receiver.try_recv() {
                    events.push(ev);
                }
                let mut this = this.lock().await;
                match this
                    .dispatch_server_events(&mut events, &mut event_writer, io_channel_id, user_channel_id)
                    .await?
                {
                    RunState::Continue => continue,
                    state => break Ok(state),
                }
            }
        };

        // (M5c step 3b) Drain inbound migrated-channel data (EGFX over the UDP
        // tunnel) into the drdynvc processor. Idle forever (never resolves) when
        // there's no inbound tunnel — feature off, no migration, or the channel
        // closed — so the other arms drive the session.
        let this_tunnel = Rc::clone(&s);
        let dispatch_tunnel_inbound = async move {
            let _ = &this_tunnel;
            #[cfg(feature = "multitransport")]
            if let Some(mut rx) = tunnel_inbound {
                while let Some(pdu) = rx.recv().await {
                    let mut this = this_tunnel.lock().await;
                    this.process_tunnel_inbound(pdu);
                }
            }
            std::future::pending::<Result<RunState>>().await
        };

        let state = tokio::select!(
            state = dispatch_pdu => state,
            state = dispatch_display => state,
            state = dispatch_events => state,
            state = dispatch_audio => state,
            state = dispatch_tunnel_inbound => state,
        );

        debug!("End of client loop: {state:?}");
        state
    }

    /// (vendored, feature=multitransport) Handle the client's Initiate
    /// Multitransport Response. M1 has no UDP transport, so this only logs the
    /// outcome and clears the in-flight request; the session stays on TCP.
    #[cfg(feature = "multitransport")]
    fn handle_multitransport_response(&mut self, resp: &ironrdp_pdu::rdp::multitransport::MultitransportResponsePdu) {
        match self.multitransport_migration.take() {
            Some(state) if state.request_id == resp.request_id => {
                if resp.is_success() {
                    debug!(
                        request_id = resp.request_id,
                        protocol = ?state.protocol,
                        "multitransport response S_OK (unexpected in M1 with no listener); staying on TCP"
                    );
                } else {
                    debug!(
                        request_id = resp.request_id,
                        protocol = ?state.protocol,
                        hr = format_args!("{:#010x}", resp.hr_response),
                        "multitransport response: client could not establish UDP; continuing on TCP (expected in M1)"
                    );
                }
            }
            Some(other) => {
                let in_flight = other.request_id;
                self.multitransport_migration = Some(other);
                warn!(
                    got = resp.request_id,
                    in_flight, "multitransport response with mismatched request_id; ignoring"
                );
            }
            None => warn!(
                request_id = resp.request_id,
                "multitransport response with no request in flight; ignoring"
            ),
        }
    }

    /// (M5c) Try to interpret a message-channel PDU as the client's Initiate
    /// Multitransport Response. Returns `true` if it WAS one (handled — caller
    /// should not warn "Unexpected channel"). On `S_OK` — the UDP multitransport
    /// connection (incl. our MS-RDPEMT tunnel) is established — this opens the
    /// Soft-Sync gate and sends the `DYNVC_SOFT_SYNC_REQUEST` exactly once.
    #[cfg(feature = "multitransport")]
    async fn maybe_handle_multitransport_response(
        &mut self,
        writer: &mut impl FramedWrite,
        user_data: &[u8],
        user_channel_id: u16,
    ) -> Result<bool> {
        use ironrdp_pdu::rdp::multitransport::MultitransportResponsePdu;

        // The response is a BasicSecurityHeader-wrapped PDU (decode re-validates
        // the TRANSPORT_RSP flag). If it doesn't decode, this wasn't a response —
        // let the caller fall through to its "Unexpected channel" warning.
        let Ok(resp) = decode::<MultitransportResponsePdu>(user_data) else {
            return Ok(false);
        };

        let Some(state) = self.multitransport_migration.as_mut() else {
            warn!(
                request_id = resp.request_id,
                "Initiate Multitransport Response with no request in flight; ignoring"
            );
            return Ok(true);
        };
        if state.request_id != resp.request_id {
            warn!(
                got = resp.request_id,
                in_flight = state.request_id,
                "Initiate Multitransport Response request_id mismatch; ignoring"
            );
            return Ok(true);
        }
        if !resp.is_success() {
            debug!(
                request_id = resp.request_id,
                hr = format_args!("{:#010x}", resp.hr_response),
                "Initiate Multitransport Response: client could not establish UDP; staying on TCP"
            );
            self.multitransport_migration = None;
            return Ok(true);
        }
        if state.soft_sync_sent {
            debug!(
                request_id = resp.request_id,
                "Initiate Multitransport Response S_OK (Soft-Sync already sent); ignoring retransmit"
            );
            return Ok(true);
        }
        state.soft_sync_sent = true;
        debug!(
            request_id = resp.request_id,
            "Initiate Multitransport Response S_OK — Soft-Sync gate open"
        );
        // Secondary TCP gate (a client that *does* send an Initiate Response S_OK
        // — mstsc never does). Keep it an empty spike: EGFX migration is driven by
        // the listener-bound gate (`maybe_soft_sync_on_egfx`), not this path.
        self.send_soft_sync_request(writer, user_channel_id, dvc::pdu::TUNNELTYPE_UDPFECR, Vec::new())
            .await?;
        Ok(true)
    }

    /// (M5c) Called from the EGFX dispatch path. If the UDP tunnel is bound (the
    /// listener flipped our shared flag on a cookie match) and we haven't sent the
    /// Soft-Sync yet, send it now — EGFX shipping means its DVC channel is open and
    /// the client is fully in DVC mode, the right moment to begin the migration.
    /// This (not a TCP Initiate Response) is the real success gate: mstsc signals
    /// multitransport success by creating the tunnel, never by a TCP response.
    #[cfg(feature = "multitransport")]
    async fn maybe_soft_sync_on_egfx(&mut self, writer: &mut impl FramedWrite, user_channel_id: u16) -> Result<()> {
        let bound = self
            .udp_tunnel_bound
            .as_ref()
            .is_some_and(|f| f.load(Ordering::Relaxed));
        if !bound {
            return Ok(());
        }

        // P2.4b (dual-channel): if the lossy audio DVCs are registered, Soft-Sync the
        // LOSSY `AUDIO_PLAYBACK_LOSSY_DVC` onto the LOSSY tunnel (`TUNNELTYPE_UDPFECL`)
        // — the inverse of EGFX, which migrates onto the RELIABLE tunnel. The lossy
        // channel carries Wave2 data only; the format/quality/training handshake runs
        // on the reliable `AUDIO_PLAYBACK_DVC` over TCP (mstsc tears the socket down if
        // formats arrive on the lossy name — over TCP *or* the tunnel, both verified).
        // Look the channel up BEFORE consuming the one-time guard, so if its DVC Create
        // Response hasn't arrived yet we just retry on the next EGFX frame (guard intact).
        if self.multitransport_lossy_audio_formats.is_some() {
            let Some(audio_id) = self
                .get_svc_processor::<dvc::DrdynvcServer>()
                .and_then(|d| d.get_channel_id_by_name(crate::multitransport::audio_dvc::AUDIO_PLAYBACK_LOSSY_DVC))
            else {
                return Ok(());
            };
            match self.multitransport_migration.as_mut() {
                Some(state) if !state.soft_sync_sent => state.soft_sync_sent = true,
                _ => return Ok(()),
            }
            debug!(
                audio_channel_id = audio_id,
                "UDP lossy tunnel bound — Soft-Sync migrating AUDIO_PLAYBACK_LOSSY_DVC onto the LOSSY (UdpFecL) \
                 tunnel (waves only; the format handshake runs on the reliable AUDIO_PLAYBACK_DVC over TCP)"
            );
            return self
                .send_soft_sync_request(writer, user_channel_id, dvc::pdu::TUNNELTYPE_UDPFECL, vec![audio_id])
                .await;
        }

        // One-time guard (drops the &mut borrow before the get_svc_processor below).
        match self.multitransport_migration.as_mut() {
            Some(state) if !state.soft_sync_sent => state.soft_sync_sent = true,
            _ => return Ok(()),
        }

        // EXPERIMENTAL (`--udp-migrate-egfx`, or the legacy `MACRDP_UDP_MIGRATE_EGFX`
        // env fallback): name the EGFX DVC in the request so the client actually
        // moves it onto the UDP tunnel, and flip `egfx_on_udp` so subsequent frames
        // route over UDP. Default (off) is the proven safe spike — an empty channel
        // list, EGFX stays on TCP.
        let channel_ids = if self.migrate_egfx || migrate_egfx_enabled() {
            match self
                .get_svc_processor::<dvc::DrdynvcServer>()
                .and_then(|d| d.get_channel_id_by_name(EGFX_DVC_CHANNEL_NAME))
            {
                Some(id) => {
                    self.egfx_on_udp = true;
                    // Tell the H.264 pipeline EGFX is now on the UDP tunnel so it
                    // enables frame-ack-lag backpressure (no socket pacing here).
                    if let Some(handle) = &self.egfx_on_udp_handle {
                        handle.store(true, Ordering::Relaxed);
                    }
                    debug!(
                        gfx_channel_id = id,
                        "--udp-migrate-egfx: Soft-Sync will migrate the EGFX DVC onto the UDP tunnel"
                    );
                    vec![id]
                }
                None => {
                    warn!("--udp-migrate-egfx set but EGFX DVC channel not found; sending empty Soft-Sync");
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
        // EGFX normally migrates onto the RELIABLE tunnel. The P2.4b isolation test
        // (`MACRDP_UDP_MIGRATE_EGFX_LOSSY`) instead targets the LOSSY/DTLS tunnel to
        // exercise the DTLS `RDP_TUNNEL_DATA` egress path with a proven payload.
        let egfx_tunnel_type = if migrate_egfx_lossy() {
            dvc::pdu::TUNNELTYPE_UDPFECL
        } else {
            dvc::pdu::TUNNELTYPE_UDPFECR
        };
        // Signal macrdp's H.264 pipeline that EGFX is now riding a LOSSY tunnel
        // (UDPFECL, where datagrams can be dropped) so it can arm ack-driven IDR
        // recovery. Only when EGFX is actually migrated (non-empty channel list)
        // AND the target is the lossy tunnel; the reliable tunnel never drops, so
        // the flag stays false there.
        if egfx_tunnel_type == dvc::pdu::TUNNELTYPE_UDPFECL
            && !channel_ids.is_empty()
            && let Some(handle) = &self.egfx_on_lossy_handle
        {
            handle.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        debug!("UDP tunnel bound + EGFX active — Soft-Sync gate open");
        self.send_soft_sync_request(writer, user_channel_id, egfx_tunnel_type, channel_ids)
            .await
    }

    /// (M5c) Send a `DYNVC_SOFT_SYNC_REQUEST` over the DRDYNVC static channel on
    /// TCP. `tunnel_type` selects the target tunnel (`TUNNELTYPE_UDPFECR` reliable /
    /// `TUNNELTYPE_UDPFECL` lossy); `channel_ids` are the DVC channels to migrate onto
    /// it — **empty** is the safe spike (TCP_FLUSHED only, migrate nothing, so
    /// everything stays on TCP); a non-empty list commits those channels' data to the
    /// tunnel (the EGFX id for the reliable tunnel, the lossy audio DVC id for FECL).
    #[cfg(feature = "multitransport")]
    async fn send_soft_sync_request(
        &mut self,
        writer: &mut impl FramedWrite,
        user_channel_id: u16,
        tunnel_type: u32,
        channel_ids: Vec<u32>,
    ) -> Result<()> {
        let Some(drdynvc_channel_id) = self.get_channel_id_by_type::<dvc::DrdynvcServer>() else {
            warn!("No DRDYNVC channel; cannot send Soft-Sync request");
            return Ok(());
        };

        let migrating = !channel_ids.is_empty();
        let req = dvc::pdu::SoftSyncRequestPdu::switch_to_tunnel(tunnel_type, channel_ids);
        let pdu = dvc::pdu::DrdynvcServerPdu::SoftSyncRequest(req);
        // Soft-Sync is a top-level DRDYNVC PDU (not sub-channel DATA), so it ships
        // as a plain SvcMessage on the drdynvc static channel.
        let msg = ironrdp_svc::SvcMessage::from(pdu).with_flags(ChannelFlags::SHOW_PROTOCOL);
        let data = server_encode_svc_messages(vec![msg], drdynvc_channel_id, user_channel_id)?;
        writer.write_all(&data).await?;
        let tunnel_name = match tunnel_type {
            t if t == dvc::pdu::TUNNELTYPE_UDPFECL => "lossy (UdpFecL)",
            t if t == dvc::pdu::TUNNELTYPE_UDPFECR => "reliable (UdpFecR)",
            _ => "unknown",
        };
        if migrating {
            debug!(
                tunnel_type,
                tunnel = tunnel_name,
                "Sent DYNVC_SOFT_SYNC_REQUEST (migrating channels onto the UDP tunnel)"
            );
        } else {
            debug!(
                tunnel_type,
                tunnel = tunnel_name,
                "Sent DYNVC_SOFT_SYNC_REQUEST (empty channel list — nothing migrated, stays on TCP)"
            );
        }
        Ok(())
    }

    /// (M5c) Route already-encoded DVC messages (EGFX frames, or the lossy audio
    /// DVC's handshake/wave PDUs) over the bound UDP tunnel instead of TCP: hand each
    /// to the listener (keyed by this connection's cookie), which wraps it in
    /// `RDP_TUNNEL_DATA` and encrypts via the peer's transport (rustls for the
    /// reliable tunnel, DTLS for the lossy one). Best-effort — if the handoff or
    /// cookie is missing the data is dropped (the experimental UDP path; only reached
    /// for a migrated channel: EGFX under `MACRDP_UDP_MIGRATE_EGFX`, or lossy audio
    /// under `MACRDP_UDP_LOSSY_AUDIO`).
    #[cfg(feature = "multitransport")]
    fn route_dvc_over_udp(&self, messages: Vec<ironrdp_svc::SvcMessage>) -> Result<()> {
        let Some(sender) = self.multitransport_tunnel_sender.as_ref() else {
            warn!("migrated channel data but no tunnel sender; dropping");
            return Ok(());
        };
        let Some(cookie) = self.multitransport_migration.as_ref().map(|m| m.cookie) else {
            warn!("migrated channel data but no migration cookie; dropping");
            return Ok(());
        };
        // The MS-RDPEMT tunnel carries the BARE DRDYNVC PDU (no static-channel
        // CHANNEL_PDU_HEADER) — RDP_TUNNEL_DATA provides the framing, so encode
        // each message unframed (NOT `chunkify`, which prepends CHANNEL_PDU_HEADER
        // and would make the client misparse the stream). `encode_dvc_messages`
        // already DVC-chunked these to fit, so one tunnel PDU per message is fine.
        let mut n = 0usize;
        for msg in messages {
            let pdu = msg
                .encode_unframed_pdu()
                .map_err(|e| anyhow::anyhow!("encode DVC PDU for tunnel: {e}"))?;
            sender.send(cookie, pdu);
            n += 1;
        }
        trace!(pdus = n, "routed DVC PDUs over the UDP tunnel");
        Ok(())
    }

    /// (2b-iv-B) Is the lossy-audio-over-UDP path live right now? Returns
    /// `(format_no, lossy_channel_id)` only when every precondition holds, so a
    /// `Some` result guarantees [`route_lossy_audio_wave`](Self::route_lossy_audio_wave)
    /// can ship: the lossy audio DVCs are registered, the RELIABLE
    /// `AUDIO_PLAYBACK_DVC` handshake has published a negotiated format index, a UDP
    /// tunnel sender + migration cookie exist, the tunnel is bound, and the lossy
    /// `AUDIO_PLAYBACK_LOSSY_DVC` channel is open. Until all of that is true (e.g. at
    /// connect, before the handshake completes / the tunnel binds) audio stays on the
    /// static rdpsnd TCP channel, so there is exactly one playback path at any time
    /// (no double-play) with a clean handover once UDP is ready.
    #[cfg(feature = "multitransport")]
    fn lossy_audio_target(&mut self) -> Option<(u16, u32)> {
        use core::sync::atomic::Ordering;
        self.multitransport_lossy_audio_formats.as_ref()?;
        let format_no = self.lossy_audio_format.get()?;
        self.multitransport_tunnel_sender.as_ref()?;
        self.multitransport_migration.as_ref()?;
        if !self
            .udp_tunnel_bound
            .as_ref()
            .is_some_and(|f| f.load(Ordering::Relaxed))
        {
            return None;
        }
        let lossy_id = self
            .get_svc_processor::<dvc::DrdynvcServer>()?
            .get_channel_id_by_name(crate::multitransport::audio_dvc::AUDIO_PLAYBACK_LOSSY_DVC)?;
        Some((format_no, lossy_id))
    }

    /// (2b-iv-B) Ship one audio wave as a `Wave2` PDU on the lossy
    /// `AUDIO_PLAYBACK_LOSSY_DVC` (channel 5) over the bound UDP/DTLS tunnel instead
    /// of the static rdpsnd TCP channel. Stamps the wave with `format_no` (the index
    /// the reliable `AUDIO_PLAYBACK_DVC` negotiated) and `block_no`, DVC-frames it for
    /// `lossy_channel_id`, then hands it to [`route_dvc_over_udp`](Self::route_dvc_over_udp)
    /// (which sends the bare DRDYNVC PDU as `RDP_TUNNEL_DATA`, DTLS-encrypted).
    #[cfg(feature = "multitransport")]
    fn route_lossy_audio_wave(
        &mut self,
        block_no: u8,
        audio_timestamp: u32,
        format_no: u16,
        lossy_channel_id: u32,
        data: Vec<u8>,
    ) -> Result<()> {
        if !self.lossy_audio_streaming {
            self.lossy_audio_streaming = true;
            warn!(
                format_no,
                lossy_channel_id,
                "P2.4b 2b-iv-B: streaming Wave2 audio over the LOSSY UDP/DTLS tunnel \
                 (AUDIO_PLAYBACK_LOSSY_DVC) — static rdpsnd now silent"
            );
        }
        let wave = crate::multitransport::audio_dvc::lossy_wave_dvc_message(block_no, audio_timestamp, format_no, data);
        let msgs = dvc::encode_dvc_messages(lossy_channel_id, vec![wave], ChannelFlags::empty())
            .map_err(|e| anyhow::anyhow!("encode lossy audio DVC wave: {e}"))?;
        self.route_dvc_over_udp(msgs)
    }

    /// (M5c step 3b) Process one inbound migrated-channel PDU the UDP listener
    /// forwarded: a bare DRDYNVC PDU (HigherLayerData of an inbound
    /// `RDP_TUNNEL_DATA`, e.g. an EGFX frame acknowledgement). Feed it straight to
    /// the drdynvc processor (the tunnel replaces the static-channel framing, so no
    /// `CHANNEL_PDU_HEADER` to strip) and ship any reply PDUs back over the tunnel.
    /// Errors are logged, not propagated — the optional UDP fast-path must never
    /// tear down the TCP-authoritative session.
    #[cfg(feature = "multitransport")]
    fn process_tunnel_inbound(&mut self, pdu: Vec<u8>) {
        use ironrdp_svc::SvcProcessor as _;
        let Some(drdynvc) = self.get_svc_processor::<dvc::DrdynvcServer>() else {
            warn!("inbound tunnel data but no DRDYNVC channel; dropping");
            return;
        };
        let replies = match drdynvc.process(&pdu) {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "failed to process inbound tunnel DRDYNVC PDU");
                return;
            }
        };
        if !replies.is_empty()
            && let Err(e) = self.route_dvc_over_udp(replies)
        {
            warn!(error = %e, "failed to ship tunnel reply PDUs");
        }
    }

    async fn client_accepted<R, W>(
        &mut self,
        reader: &mut Framed<R>,
        writer: &mut Framed<W>,
        result: AcceptorResult,
    ) -> Result<RunState>
    where
        R: FramedRead,
        W: FramedWrite,
    {
        debug!("Client accepted");

        // (vendored) Publish the client's announced keyboard layout so the
        // input backend can auto-select a matching layout for non-US clients.
        if let Some(handle) = &self.keyboard_layout {
            handle.store(result.keyboard_layout, Ordering::Relaxed);
            debug!(klid = result.keyboard_layout, "client keyboard layout announced");
        }

        // (vendored, divergence (20)) Client-fingerprint log: the identity
        // fields the acceptor captured from the GCC Client Core Data
        // (acceptor divergence (4)) + the platform from the General capset.
        // Heuristics for "which client is this": mstsc sends the real Windows
        // build (e.g. 22621) + platform WINDOWS; FreeRDP hardcodes build 2600;
        // the Windows Apps announce their host platform. Informational
        // fingerprinting only — a client can claim anything. Skipped on
        // reactivation (same connection, already logged).
        if !result.reactivation {
            let platform = result
                .capabilities
                .iter()
                .find_map(|c| match c {
                    CapabilitySet::General(g) => {
                        Some(format!("{:?}/{:?}", g.major_platform_type, g.minor_platform_type))
                    }
                    _ => None,
                })
                .unwrap_or_else(|| "unknown".to_owned());
            info!(
                client_name = %result.client_name,
                rdp_version = format_args!("{:#x}", result.client_version),
                client_build = result.client_build,
                platform = %platform,
                "client fingerprint"
            );
            // Surface it to the application's ConnectionHandler (macrdp's
            // AuthGuardHandler emits an `event="fingerprint"` audit record for
            // the SIEM JSON stream).
            if let Some(handler) = self.connection_handler.as_ref() {
                handler.borrow_mut().on_client_fingerprint(
                    &result.client_name,
                    result.client_version,
                    result.client_build,
                    &platform,
                );
            }
        }

        if !result.input_events.is_empty() {
            debug!("Handling input event backlog from acceptor sequence");
            self.handle_input_backlog(
                writer,
                result.io_channel_id,
                result.user_channel_id,
                result.input_events,
            )
            .await?;
        }

        self.static_channels = result.static_channels;
        if !result.reactivation {
            for (_type_id, channel, channel_id) in self.static_channels.iter_mut() {
                debug!(?channel, ?channel_id, "Start");
                let Some(channel_id) = channel_id else {
                    continue;
                };
                let svc_responses = channel.start()?;
                let response = server_encode_svc_messages(svc_responses, channel_id, result.user_channel_id)?;
                writer.write_all(&response).await?;
            }
        }

        // (vendored, feature=multitransport) The acceptor has already emitted the
        // Server Initiate Multitransport Request (after licensing, before Demand
        // Active — the only window clients honor it). Here we just record what it
        // sent so the client's Multitransport Response can be matched and the
        // inbound UDP flow bound to this session. Initial accept only.
        #[cfg(feature = "multitransport")]
        if !result.reactivation
            && let Some(offer) = result.multitransport_offered
        {
            self.multitransport_migration = Some(crate::multitransport::MigrationState {
                request_id: offer.request_id,
                cookie: offer.cookie,
                protocol: offer.protocol,
                soft_sync_sent: false,
            });
            debug!(
                request_id = offer.request_id,
                protocol = ?offer.protocol,
                soft_sync = result
                    .multitransport_flags
                    .contains(ironrdp_pdu::gcc::MultiTransportFlags::SOFT_SYNC_TCP_TO_UDP),
                cookie = %offer.cookie.iter().map(|b| format!("{b:02x}")).collect::<String>(),
                "Server Initiate Multitransport Request was sent by the acceptor"
            );
        }

        let mut update_codecs = UpdateEncoderCodecs::new();
        let mut surface_flags = CmdFlags::empty();
        for c in result.capabilities {
            match c {
                CapabilitySet::General(c) => {
                    let fastpath = c.extra_flags.contains(GeneralExtraFlags::FASTPATH_OUTPUT_SUPPORTED);
                    if !fastpath {
                        bail!("Fastpath output not supported!");
                    }
                }
                CapabilitySet::Bitmap(b) => {
                    if !b.desktop_resize_flag {
                        debug!("Desktop resize is not supported by the client");
                        continue;
                    }

                    let client_size = DesktopSize {
                        width: b.desktop_width,
                        height: b.desktop_height,
                    };
                    let display_size = self.display.lock().await.request_initial_size(client_size).await;

                    // It's problematic when the client didn't resize, as we send bitmap updates that don't fit.
                    // The client will likely drop the connection.
                    if client_size.width < display_size.width || client_size.height < display_size.height {
                        // TODO: we may have different behaviour instead, such as clipping or scaling?
                        warn!(
                            "Client size doesn't fit the server size: {:?} < {:?}",
                            client_size, display_size
                        );
                    }
                }
                CapabilitySet::SurfaceCommands(c) => {
                    surface_flags = c.flags;
                }
                CapabilitySet::BitmapCodecs(BitmapCodecs(codecs)) => {
                    for codec in codecs {
                        match codec.property {
                            // FIXME: The encoder operates in image mode only.
                            //
                            // See [MS-RDPRFX] 3.1.1.1 "State Machine" for
                            // implementation of the video mode. which allows to
                            // skip sending Header for each image.
                            //
                            // We should distinguish parameters for both modes,
                            // and somehow choose the "best", instead of picking
                            // the last parsed here.
                            CodecProperty::RemoteFx(rdp::capability_sets::RemoteFxContainer::ClientContainer(c))
                                if self.opts.has_remote_fx() =>
                            {
                                for caps in c.caps_data.0.0 {
                                    update_codecs.set_remotefx(Some((caps.entropy_bits, codec.id)));
                                }
                            }
                            CodecProperty::ImageRemoteFx(rdp::capability_sets::RemoteFxContainer::ClientContainer(
                                c,
                            )) if self.opts.has_image_remote_fx() => {
                                for caps in c.caps_data.0.0 {
                                    update_codecs.set_remotefx(Some((caps.entropy_bits, codec.id)));
                                }
                            }
                            CodecProperty::NsCodec(client_ns) if self.opts.has_nscodec() => {
                                // Re-use the client's confirmed color-loss
                                // level so we encode at the same shift the
                                // client decodes against.
                                update_codecs.set_nscodec(Some((codec.id, client_ns.color_loss_level)));
                            }
                            CodecProperty::NsCodec(_) => (),
                            #[cfg(feature = "qoi")]
                            CodecProperty::Qoi if self.opts.has_qoi() => {
                                update_codecs.set_qoi(Some(codec.id));
                            }
                            #[cfg(feature = "qoiz")]
                            CodecProperty::QoiZ if self.opts.has_qoiz() => {
                                update_codecs.set_qoiz(Some(codec.id));
                            }
                            _ => (),
                        }
                    }
                }
                _ => {}
            }
        }

        let desktop_size = self.display.lock().await.size().await;
        let encoder = UpdateEncoder::new(desktop_size, surface_flags, update_codecs, self.opts.max_request_size)
            .context("failed to initialize update encoder")?;

        // (vendored, divergence 13) Provision the Server Auto-Reconnect Cookie
        // once per TCP connection, now that activation is complete (Confirm
        // Active processed, encoder built). A Save Session Info PDU carrying the
        // ARC_SC cookie is what makes mstsc auto-reconnect on an ungraceful drop
        // (e.g. the EGFX blank-recovery connection drop) instead of showing
        // "disconnected". Not re-sent on deactivation-reactivation resizes
        // (`auto_reconnect_sent` guard, reset per connection in run_connection).
        if !self.auto_reconnect_sent
            && let Some(cookie) = self.auto_reconnect_cookie.clone()
        {
            let pdu = rdp::headers::ShareDataPdu::SaveSessionInfo(rdp::session_info::SaveSessionInfoPdu {
                info_type: rdp::session_info::InfoType::LogonExtended,
                info_data: rdp::session_info::InfoData::LogonExtended(rdp::session_info::LogonInfoExtended {
                    present_fields_flags: rdp::session_info::LogonExFlags::AUTO_RECONNECT_COOKIE,
                    auto_reconnect: Some(cookie),
                    errors_info: None,
                }),
            });
            let data = encode_share_data_pdu(pdu, result.io_channel_id, result.user_channel_id)?;
            writer.write_all(&data).await.context("send auto-reconnect cookie")?;
            self.auto_reconnect_sent = true;
            debug!("Sent Server Auto-Reconnect Cookie (Save Session Info PDU)");
        }

        let state = self
            .client_loop(reader, writer, result.io_channel_id, result.user_channel_id, encoder)
            .await
            .context("client loop failure")?;

        Ok(state)
    }

    async fn handle_input_backlog(
        &mut self,
        writer: &mut impl FramedWrite,
        io_channel_id: u16,
        user_channel_id: u16,
        frames: Vec<Vec<u8>>,
    ) -> Result<()> {
        for frame in frames {
            match Action::from_fp_output_header(frame[0]) {
                Ok(Action::FastPath) => {
                    let input = decode(&frame)?;
                    self.handle_fastpath(input).await;
                }

                Ok(Action::X224) => {
                    let _ = self.handle_x224(writer, io_channel_id, user_channel_id, &frame).await;
                }

                // the frame here is always valid, because otherwise it would
                // have failed during the acceptor loop
                Err(_) => unreachable!(),
            }
        }

        Ok(())
    }

    async fn handle_fastpath(&mut self, input: FastPathInput) {
        for event in input.input_events().iter().copied() {
            let mut handler = self.handler.lock().await;
            match event {
                FastPathInputEvent::KeyboardEvent(flags, key) => {
                    handler.keyboard((key, flags).into());
                }

                FastPathInputEvent::UnicodeKeyboardEvent(flags, key) => {
                    handler.keyboard((key, flags).into());
                }

                FastPathInputEvent::SyncEvent(flags) => {
                    handler.keyboard(flags.into());
                }

                FastPathInputEvent::MouseEvent(mouse) => {
                    // A button PDU carries its position; emit that as a Move
                    // first so the click lands there (fixes the iOS Windows App
                    // touch-mode tap, which sends no preceding move). No-op for
                    // clients that already move-then-click.
                    for ev in crate::handler::mouse_events_from_pdu(mouse) {
                        handler.mouse(ev);
                    }
                }

                FastPathInputEvent::MouseEventEx(mouse) => {
                    handler.mouse(mouse.into());
                }

                FastPathInputEvent::MouseEventRel(mouse) => {
                    handler.mouse(mouse.into());
                }

                FastPathInputEvent::QoeEvent(quality) => {
                    warn!("Received QoE: {}", quality);
                }
            }
        }
    }

    async fn handle_io_channel_data(&mut self, data: SendDataRequest<'_>) -> Result<bool> {
        #[cfg(not(feature = "multitransport"))]
        let control: rdp::headers::ShareControlHeader = decode(data.user_data.as_ref())?;
        // (vendored, feature=multitransport) The client's Initiate Multitransport
        // Response rides the IO channel as a BasicSecurityHeader PDU (not a
        // ShareControl PDU). Try ShareControl first (the common case, and it
        // validates its pduType so a security-header PDU fails it); only on
        // failure attempt the response, which re-checks the TRANSPORT_RSP flag.
        #[cfg(feature = "multitransport")]
        let control: rdp::headers::ShareControlHeader = match decode(data.user_data.as_ref()) {
            Ok(control) => control,
            Err(e) => {
                if let Ok(resp) =
                    decode::<ironrdp_pdu::rdp::multitransport::MultitransportResponsePdu>(data.user_data.as_ref())
                {
                    self.handle_multitransport_response(&resp);
                    return Ok(false);
                }
                return Err(e.into());
            }
        };

        match control.share_control_pdu {
            ShareControlPdu::Data(header) => match header.share_data_pdu {
                rdp::headers::ShareDataPdu::Input(pdu) => {
                    self.handle_input_event(pdu).await;
                }

                rdp::headers::ShareDataPdu::ShutdownRequest => {
                    return Ok(true);
                }

                // (pin-bump a5d1c682) The auto-detect RESPONSE is no longer a
                // ShareDataPdu variant — it rides the MCS message channel
                // (handle_message_channel_data upstream). macrdp doesn't use RDP
                // network-autodetect (div-15 kernel TCP RTT), so the response is never
                // received; the arm is removed rather than re-plumbed onto the message
                // channel.

                // Client requests the server stop or resume sending display
                // updates. mstsc sends `desktop_rect: None` on minimize and
                // `desktop_rect: Some(rect)` on refocus. Without honoring
                // this, the server keeps streaming H.264/EGFX frames into a
                // minimized client; on refocus the client must chew through
                // the accumulated backlog before it can present the current
                // frame, locking up its input dispatch for seconds. Flagging
                // the shared `display_suppressed` lets the display backend
                // skip frame emission while it's set.
                rdp::headers::ShareDataPdu::SuppressOutput(pdu) => {
                    let suppress = pdu.desktop_rect.is_none();
                    self.display_suppressed.store(suppress, Ordering::Relaxed);
                    debug!(suppress, "client suppress-output state changed");
                }

                // Client asks the server to redraw a rectangle — typical on
                // refocus after a minimize. Clear the suppress flag so the
                // backend resumes emission and treat this as "client wants
                // updates again." (The flag would also be cleared by the
                // `SuppressOutput { Some(rect) }` that usually accompanies
                // this; clearing here is belt-and-braces against clients
                // that send only one of the two.)
                rdp::headers::ShareDataPdu::RefreshRectangle(_) => {
                    if self.display_suppressed.swap(false, Ordering::Relaxed) {
                        debug!("client RefreshRectangle cleared suppress-output state");
                    }
                }

                unexpected => {
                    warn!(?unexpected, "Unexpected share data pdu");
                }
            },

            unexpected => {
                warn!(?unexpected, "Unexpected share control");
            }
        }

        Ok(false)
    }

    async fn handle_x224(
        &mut self,
        writer: &mut impl FramedWrite,
        io_channel_id: u16,
        user_channel_id: u16,
        frame: &[u8],
    ) -> Result<bool> {
        let message = decode::<X224<mcs::McsMessage<'_>>>(frame)?;
        match message.0 {
            mcs::McsMessage::SendDataRequest(data) => {
                debug!(?data, "McsMessage::SendDataRequest");
                if data.channel_id == io_channel_id {
                    return self.handle_io_channel_data(data).await;
                }

                if let Some(svc) = self.static_channels.get_by_channel_id_mut(data.channel_id) {
                    let response_pdus = svc.process(&data.user_data)?;
                    let response = server_encode_svc_messages(response_pdus, data.channel_id, user_channel_id)?;
                    writer.write_all(&response).await?;
                } else {
                    // (vendored, feature=multitransport) The client's Initiate
                    // Multitransport Response rides the MCS message channel (granted
                    // by the acceptor when an offer is active, M3c), so it lands here
                    // rather than on the IO channel or a static channel. On S_OK it's
                    // the gate to begin Soft-Sync (move EGFX onto the UDP tunnel).
                    #[cfg(feature = "multitransport")]
                    if self
                        .maybe_handle_multitransport_response(writer, data.user_data.as_ref(), user_channel_id)
                        .await?
                    {
                        return Ok(false);
                    }
                    warn!(channel_id = data.channel_id, "Unexpected channel received: ID",);
                }
            }

            mcs::McsMessage::DisconnectProviderUltimatum(disconnect) => {
                if disconnect.reason == mcs::DisconnectReason::UserRequested {
                    return Ok(true);
                }
            }

            _ => {
                warn!(name = ironrdp_core::name(&message), "Unexpected mcs message");
            }
        }

        Ok(false)
    }

    async fn handle_input_event(&mut self, input: InputEventPdu) {
        for event in input.0 {
            let mut handler = self.handler.lock().await;
            match event {
                ironrdp_pdu::input::InputEvent::ScanCode(key) => {
                    handler.keyboard((key.key_code, key.flags).into());
                }

                ironrdp_pdu::input::InputEvent::Unicode(key) => {
                    handler.keyboard((key.unicode_code, key.flags).into());
                }

                ironrdp_pdu::input::InputEvent::Sync(sync) => {
                    handler.keyboard(sync.flags.into());
                }

                ironrdp_pdu::input::InputEvent::Mouse(mouse) => {
                    // Emit the button PDU's position as a Move first — see the
                    // fast-path arm above (iOS Windows App touch-mode fix).
                    for ev in crate::handler::mouse_events_from_pdu(mouse) {
                        handler.mouse(ev);
                    }
                }

                ironrdp_pdu::input::InputEvent::MouseX(mouse) => {
                    handler.mouse(mouse.into());
                }

                ironrdp_pdu::input::InputEvent::MouseRel(mouse) => {
                    handler.mouse(mouse.into());
                }

                ironrdp_pdu::input::InputEvent::Unused(_) => {}
            }
        }
    }

    async fn accept_finalize<S>(&mut self, mut framed: TokioFramed<S>, mut acceptor: Acceptor) -> Result<TokioFramed<S>>
    where
        S: AsyncRead + AsyncWrite + Sync + Send + Unpin,
    {
        loop {
            let (new_framed, result) = ironrdp_acceptor::accept_finalize(framed, &mut acceptor)
                .await
                .context("failed to accept client during finalize")?;

            let (mut reader, mut writer) = split_tokio_framed(new_framed);

            match self.client_accepted(&mut reader, &mut writer, result).await? {
                RunState::Continue => {
                    unreachable!();
                }
                RunState::DeactivationReactivation { desktop_size } => {
                    // No description of such behavior was found in the
                    // specification, but apparently, we must keep the channel
                    // state as they were during reactivation. This fixes
                    // various state issues during client resize.
                    acceptor = Acceptor::new_deactivation_reactivation(
                        acceptor,
                        core::mem::take(&mut self.static_channels),
                        desktop_size,
                    )?;
                    framed = unsplit_tokio_framed(reader, writer);
                    continue;
                }
                RunState::Disconnect => {
                    let final_framed = unsplit_tokio_framed(reader, writer);
                    return Ok(final_framed);
                }
            }
        }
    }

    pub fn set_credentials(&mut self, creds: Option<Credentials>) {
        debug!(?creds, "Changing credentials");
        self.creds = creds
    }
}

/// Encode a server-initiated Share Data PDU for the IO channel.
///
/// `share_id` is hard-coded to 0, matching the existing convention in
/// `deactivate_all()`. In practice, RDP clients do not validate `share_id`
/// on server-initiated PDUs, but a future refactor could thread the
/// negotiated value from the Demand Active exchange if needed.
fn encode_share_data_pdu(
    share_data_pdu: rdp::headers::ShareDataPdu,
    io_channel_id: u16,
    user_channel_id: u16,
) -> Result<Vec<u8>> {
    let header = rdp::headers::ShareDataHeader {
        share_data_pdu,
        stream_priority: rdp::headers::StreamPriority::Medium,
        compression_flags: rdp::headers::CompressionFlags::empty(),
        compression_type: rdp::client_info::CompressionType::K8,
    };
    let pdu = rdp::headers::ShareControlHeader {
        share_id: 0,
        pdu_source: user_channel_id,
        share_control_pdu: ShareControlPdu::Data(header),
    };
    let user_data = encode_vec(&pdu)?.into();
    let mcs_pdu = SendDataIndication {
        initiator_id: user_channel_id,
        channel_id: io_channel_id,
        user_data,
    };
    Ok(encode_vec(&X224(mcs_pdu))?)
}

async fn deactivate_all(
    io_channel_id: u16,
    user_channel_id: u16,
    writer: &mut impl FramedWrite,
) -> Result<(), anyhow::Error> {
    let pdu = ShareControlPdu::ServerDeactivateAll(ServerDeactivateAll);
    let pdu = rdp::headers::ShareControlHeader {
        share_id: 0,
        pdu_source: io_channel_id,
        share_control_pdu: pdu,
    };
    let user_data = encode_vec(&pdu)?.into();
    let pdu = SendDataIndication {
        initiator_id: user_channel_id,
        channel_id: io_channel_id,
        user_data,
    };
    let msg = encode_vec(&X224(pdu))?;
    writer.write_all(&msg).await?;
    Ok(())
}

struct SharedWriter<'w, W: FramedWrite> {
    writer: Rc<Mutex<&'w mut W>>,
}

impl<W: FramedWrite> Clone for SharedWriter<'_, W> {
    fn clone(&self) -> Self {
        Self {
            writer: Rc::clone(&self.writer),
        }
    }
}

impl<W> FramedWrite for SharedWriter<'_, W>
where
    W: FramedWrite,
{
    type WriteAllFut<'write>
        = core::pin::Pin<Box<dyn Future<Output = std::io::Result<()>> + 'write>>
    where
        Self: 'write;

    fn write_all<'a>(&'a mut self, buf: &'a [u8]) -> Self::WriteAllFut<'a> {
        Box::pin(async {
            let mut writer = self.writer.lock().await;

            writer.write_all(buf).await?;
            Ok(())
        })
    }
}

impl<'a, W: FramedWrite> SharedWriter<'a, W> {
    fn new(writer: &'a mut W) -> Self {
        Self {
            writer: Rc::new(Mutex::new(writer)),
        }
    }
}
