use core::net::SocketAddr;
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
use tokio::net::TcpSocket;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, error, trace, warn};
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
    *ENABLED.get_or_init(|| std::env::var_os("MACRDP_UDP_MIGRATE_EGFX").is_some())
}

pub struct RdpServer {
    opts: RdpServerOptions,
    // FIXME: replace with a channel and poll/process the handler?
    handler: Arc<Mutex<Box<dyn RdpServerInputHandler>>>,
    display: Arc<Mutex<Box<dyn RdpServerDisplay>>>,
    static_channels: StaticChannelSet,
    sound_factory: Option<Box<dyn SoundServerFactory>>,
    cliprdr_factory: Option<Box<dyn CliprdrServerFactory>>,
    rdpdr_factory: Option<Box<dyn crate::RdpdrServerFactory>>,
    echo_handle: EchoServerHandle,
    #[cfg(feature = "egfx")]
    gfx_factory: Option<Box<dyn GfxServerFactory>>,
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
    connection_handler: Option<Box<dyn ConnectionHandler>>,
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
    /// (vendored) Optional shared cell the server writes with the client's
    /// announced keyboard-layout identifier (KLID, from the acceptor result)
    /// when a client connects. The input backend (e.g. `macrdp`'s
    /// `MacInputHandler`) holds a clone and auto-selects a matching layout, so
    /// non-US clients type correctly with no manual configuration. 0 = unknown.
    keyboard_layout: Option<Arc<AtomicU32>>,
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
    /// when the experimental `MACRDP_UDP_MIGRATE_EGFX` env flag is on; the default
    /// path leaves EGFX on TCP (the proven empty-Soft-Sync spike).
    #[cfg(feature = "multitransport")]
    egfx_on_udp: bool,
}

#[derive(Debug)]
pub enum ServerEvent {
    Quit(String),
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

impl RdpServer {
    pub fn new(
        opts: RdpServerOptions,
        handler: Box<dyn RdpServerInputHandler>,
        display: Box<dyn RdpServerDisplay>,
        mut sound_factory: Option<Box<dyn SoundServerFactory>>,
        mut cliprdr_factory: Option<Box<dyn CliprdrServerFactory>>,
        mut rdpdr_factory: Option<Box<dyn crate::RdpdrServerFactory>>,
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
        #[cfg(feature = "egfx")]
        if let Some(gfx) = gfx_factory.as_mut() {
            gfx.set_sender(ev_sender.clone());
        }
        Self {
            opts,
            handler: Arc::new(Mutex::new(handler)),
            display: Arc::new(Mutex::new(display)),
            static_channels: StaticChannelSet::new(),
            sound_factory,
            cliprdr_factory,
            rdpdr_factory,
            echo_handle: EchoServerHandle::new(ev_sender.clone()),
            #[cfg(feature = "egfx")]
            gfx_factory,
            #[cfg(feature = "egfx")]
            gfx_handle: None,
            ev_sender,
            ev_receiver: Arc::new(Mutex::new(ev_receiver)),
            audio_receiver: Arc::new(Mutex::new(audio_receiver)),
            creds: None,
            local_addr: None,
            autodetect: None,
            connection_handler,
            display_suppressed: Arc::new(AtomicBool::new(false)),
            keyboard_layout: None,
            honor_client_desktop_size: false,
            #[cfg(feature = "multitransport")]
            multitransport: None,
            #[cfg(feature = "multitransport")]
            multitransport_migration: None,
            #[cfg(feature = "multitransport")]
            multitransport_cookies: None,
            #[cfg(feature = "multitransport")]
            udp_tunnel_bound: None,
            #[cfg(feature = "multitransport")]
            multitransport_tunnel_sender: None,
            #[cfg(feature = "multitransport")]
            egfx_on_udp: false,
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

    /// (vendored) Share a cell the server fills with the client's announced
    /// keyboard-layout identifier (KLID) when a client connects. The input
    /// backend holds a clone and can auto-select a matching layout so non-US
    /// clients type the right characters with no manual configuration. Must be
    /// called before any client connects.
    pub fn set_keyboard_layout_handle(&mut self, handle: Arc<AtomicU32>) {
        self.keyboard_layout = Some(handle);
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
    pub fn set_multitransport_cookie_registry(
        &mut self,
        registry: Option<crate::multitransport::CookieRegistry>,
    ) {
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
        if let Some(cliprdr_factory) = self.cliprdr_factory.as_deref() {
            let backend = cliprdr_factory.build_cliprdr_backend();

            let cliprdr = CliprdrServer::new(backend);

            acceptor.attach_static_channel(cliprdr);
        }

        if let Some(factory) = self.sound_factory.as_deref() {
            let backend = factory.build_backend();

            acceptor.attach_static_channel(RdpsndServer::new(backend));
        }

        // RDPDR (drive redirection). MS-RDPEFS requires it be co-advertised with
        // rdpsnd, so it's attached right after the sound channel. build_rdpdr
        // wires the backend's RdpdrHandle to this connection's event sender so it
        // can issue device-I/O requests.
        if let Some(factory) = self.rdpdr_factory.as_deref() {
            let rdpdr = crate::rdpdr::build_rdpdr(factory, self.ev_sender.clone());
            acceptor.attach_static_channel(rdpdr);
        }

        let dcs_backend = DisplayControlBackend::new(Arc::clone(&self.display));
        let dvc = dvc::DrdynvcServer::new()
            .with_dynamic_channel(AInputHandler {
                handler: Arc::clone(&self.handler),
            })
            .with_dynamic_channel(DisplayControlServer::new(Box::new(dcs_backend)));

        let dvc = {
            let echo_handle = self.echo_handle.clone();
            dvc.with_dynamic_channel(EchoDvcBridge::new(echo_handle))
        };

        #[cfg(feature = "egfx")]
        let dvc = {
            let mut dvc = dvc;
            if let Some(gfx_factory) = self.gfx_factory.as_deref() {
                if let Some((bridge, handle)) = gfx_factory.build_server_with_handle() {
                    self.gfx_handle = Some(handle);
                    dvc = dvc.with_dynamic_channel(bridge);
                } else {
                    let handler = gfx_factory.build_gfx_handler();
                    let gfx_server = ironrdp_egfx::server::GraphicsPipelineServer::new(handler);
                    dvc = dvc.with_dynamic_channel(gfx_server);
                }
            }
            dvc
        };

        acceptor.attach_static_channel(dvc);
    }

    pub async fn run_connection<S>(&mut self, stream: S) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Send + Sync + Unpin,
    {
        // Audio-lag state was previously on Self and reset here; it now
        // lives task-local to `dispatch_audio`, which is spawned fresh in
        // `client_loop` for each connection, so no reset is needed at
        // this layer anymore.

        let framed = TokioFramed::new(stream);

        let size = self.display.lock().await.size().await;
        let capabilities = capabilities::capabilities(&self.opts, size);
        let mut acceptor = Acceptor::new(self.opts.security.flag(), size, capabilities, self.creds.clone());
        // (vendored) Let the acceptor adopt the client's requested desktop
        // size from Client Core Data before Demand Active goes out.
        acceptor.set_honor_client_desktop_size(self.honor_client_desktop_size);

        // (vendored, feature=multitransport) When offering UDP multitransport:
        // (1) advertise EXTENDED_CLIENT_DATA_SUPPORTED so the client actually
        // sends its CS_MULTITRANSPORT GCC block — mstsc omits all optional GCC
        // blocks unless the server sets this X.224 flag; and (2) hand the acceptor
        // the offer so it emits the Server Initiate Multitransport Request at the
        // ONLY point clients honor it — after licensing, before Demand Active.
        // (Sending it post-finalization, once the client is ACTIVE, makes both
        // mstsc and FreeRDP misparse it as a share-control PDU and disconnect.)
        #[cfg(feature = "multitransport")]
        if let Some(provider) = self.multitransport.as_ref() {
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
                // Keep the tunnel-bound flag: the listener flips it on a cookie
                // match, and the EGFX dispatch path reads it to fire Soft-Sync.
                self.udp_tunnel_bound = Some(registry.register(offer.cookie));
            }
            acceptor.set_advertise_extended_client_data(true);
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

                if let RdpServerSecurity::Hybrid((_, pub_key)) = &self.opts.security {
                    // Generic streams don't expose peer address. Use a neutral
                    // placeholder; it's unclear whether CredSSP/NTLM actually
                    // uses this value in practice.
                    let client_name = "rdp-client".to_owned();

                    ironrdp_acceptor::accept_credssp(
                        &mut framed,
                        &mut acceptor,
                        &mut ironrdp_tokio::reqwest::ReqwestNetworkClient::new(),
                        client_name.into(),
                        pub_key.clone(),
                        None,
                    )
                    .await?;
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

        loop {
            let ev_receiver = Arc::clone(&self.ev_receiver);
            let mut ev_receiver = ev_receiver.lock().await;
            tokio::select! {
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
                },
                Ok((stream, peer)) = listener.accept() => {
                    debug!(?peer, "Received connection");
                    drop(ev_receiver);

                    let accepted = self.connection_handler
                        .as_mut()
                        .is_none_or(|h| h.on_accept(peer));

                    if !accepted {
                        debug!(?peer, "Connection rejected by handler");
                        drop(stream);
                    } else {
                        let started = tokio::time::Instant::now();
                        let result = self.run_connection(stream).await;
                        let duration = started.elapsed();

                        if let Err(ref error) = result {
                            error!(?error, "Connection error");
                        }

                        self.static_channels = StaticChannelSet::new();

                        if let Some(ref mut handler) = self.connection_handler {
                            let action = handler.on_disconnected(
                                peer,
                                duration,
                                result.as_ref().err(),
                            );
                            if action == PostConnectionAction::Stop {
                                debug!(?peer, "Handler requested stop after disconnect");
                                break;
                            }
                        }
                    }
                }
                else => break,
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
                        ServerEvent::Clipboard(_) | ServerEvent::ClipboardFileCopy(_) => {
                            clipboard.push(ev)
                        }
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
                            warn!("Wave event on unified ServerEvent channel; backend should override set_audio_sender");
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
                #[cfg(feature = "egfx")]
                ServerEvent::Egfx(msg) => match msg {
                    EgfxServerMessage::SendMessages { messages } => {
                        // (M5c) Once EGFX has been migrated onto the UDP tunnel
                        // (`egfx_on_udp`, only under MACRDP_UDP_MIGRATE_EGFX), route
                        // its frames over the tunnel instead of the TCP drdynvc
                        // channel. Default path is unchanged TCP.
                        #[cfg(feature = "multitransport")]
                        if self.egfx_on_udp {
                            self.route_egfx_over_udp(messages)?;
                            continue;
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
                        let data = encode_share_data_pdu(
                            rdp::headers::ShareDataPdu::AutoDetectReq(request),
                            io_channel_id,
                            user_channel_id,
                        )?;
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
                // rdpsnd channel id. RdpsndServer::wave() bumps a private
                // block_no and produces SVC messages — microseconds of
                // work. Lock released before the (potentially long)
                // socket write.
                let encoded = {
                    let mut this = this.lock().await;
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

        let state = tokio::select!(
            state = dispatch_pdu => state,
            state = dispatch_display => state,
            state = dispatch_events => state,
            state = dispatch_audio => state,
        );

        debug!("End of client loop: {state:?}");
        state
    }

    /// (vendored, feature=multitransport) Handle the client's Initiate
    /// Multitransport Response. M1 has no UDP transport, so this only logs the
    /// outcome and clears the in-flight request; the session stays on TCP.
    #[cfg(feature = "multitransport")]
    fn handle_multitransport_response(
        &mut self,
        resp: &ironrdp_pdu::rdp::multitransport::MultitransportResponsePdu,
    ) {
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
        self.send_soft_sync_request(writer, user_channel_id, Vec::new()).await?;
        Ok(true)
    }

    /// (M5c) Called from the EGFX dispatch path. If the UDP tunnel is bound (the
    /// listener flipped our shared flag on a cookie match) and we haven't sent the
    /// Soft-Sync yet, send it now — EGFX shipping means its DVC channel is open and
    /// the client is fully in DVC mode, the right moment to begin the migration.
    /// This (not a TCP Initiate Response) is the real success gate: mstsc signals
    /// multitransport success by creating the tunnel, never by a TCP response.
    #[cfg(feature = "multitransport")]
    async fn maybe_soft_sync_on_egfx(
        &mut self,
        writer: &mut impl FramedWrite,
        user_channel_id: u16,
    ) -> Result<()> {
        let bound = self
            .udp_tunnel_bound
            .as_ref()
            .is_some_and(|f| f.load(Ordering::Relaxed));
        if !bound {
            return Ok(());
        }
        // One-time guard (drops the &mut borrow before the get_svc_processor below).
        match self.multitransport_migration.as_mut() {
            Some(state) if !state.soft_sync_sent => state.soft_sync_sent = true,
            _ => return Ok(()),
        }

        // EXPERIMENTAL (`MACRDP_UDP_MIGRATE_EGFX`): name the EGFX DVC in the request
        // so the client actually moves it onto the UDP tunnel, and flip
        // `egfx_on_udp` so subsequent frames route over UDP. Default (flag off) is
        // the proven safe spike — an empty channel list, EGFX stays on TCP.
        let channel_ids = if migrate_egfx_enabled() {
            match self
                .get_svc_processor::<dvc::DrdynvcServer>()
                .and_then(|d| d.get_channel_id_by_name(EGFX_DVC_CHANNEL_NAME))
            {
                Some(id) => {
                    self.egfx_on_udp = true;
                    debug!(
                        gfx_channel_id = id,
                        "MACRDP_UDP_MIGRATE_EGFX: Soft-Sync will migrate the EGFX DVC onto the UDP tunnel"
                    );
                    vec![id]
                }
                None => {
                    warn!("MACRDP_UDP_MIGRATE_EGFX set but EGFX DVC channel not found; sending empty Soft-Sync");
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
        debug!("UDP tunnel bound + EGFX active — Soft-Sync gate open");
        self.send_soft_sync_request(writer, user_channel_id, channel_ids).await
    }

    /// (M5c) Send a `DYNVC_SOFT_SYNC_REQUEST` over the DRDYNVC static channel on
    /// TCP. `channel_ids` are the DVC channels to migrate onto the reliable UDP
    /// tunnel — **empty** is the safe spike (TCP_FLUSHED only, migrate nothing, so
    /// everything stays on TCP); a non-empty list (the EGFX id) commits those
    /// channels' data to the tunnel.
    #[cfg(feature = "multitransport")]
    async fn send_soft_sync_request(
        &mut self,
        writer: &mut impl FramedWrite,
        user_channel_id: u16,
        channel_ids: Vec<u32>,
    ) -> Result<()> {
        let Some(drdynvc_channel_id) = self.get_channel_id_by_type::<dvc::DrdynvcServer>() else {
            warn!("No DRDYNVC channel; cannot send Soft-Sync request");
            return Ok(());
        };

        let migrating = !channel_ids.is_empty();
        let req = dvc::pdu::SoftSyncRequestPdu::switch_to_udpfecr(channel_ids);
        let pdu = dvc::pdu::DrdynvcServerPdu::SoftSyncRequest(req);
        // Soft-Sync is a top-level DRDYNVC PDU (not sub-channel DATA), so it ships
        // as a plain SvcMessage on the drdynvc static channel.
        let msg = ironrdp_svc::SvcMessage::from(pdu).with_flags(ChannelFlags::SHOW_PROTOCOL);
        let data = server_encode_svc_messages(vec![msg], drdynvc_channel_id, user_channel_id)?;
        writer.write_all(&data).await?;
        if migrating {
            debug!("Sent DYNVC_SOFT_SYNC_REQUEST (migrating EGFX onto the UDP tunnel)");
        } else {
            debug!("Sent DYNVC_SOFT_SYNC_REQUEST (empty channel list — EGFX stays on TCP)");
        }
        Ok(())
    }

    /// (M5c) Route one EGFX frame's DVC messages over the bound UDP tunnel instead
    /// of TCP: chunk them to SVC channel-data (`CHANNEL_PDU_HEADER` + DRDYNVC PDU,
    /// the HigherLayerData the tunnel carries) and hand each chunk to the listener
    /// (keyed by this connection's cookie). Best-effort — if the handoff or cookie
    /// is missing the frame is dropped (the experimental UDP path; only reached
    /// when `egfx_on_udp` was set under `MACRDP_UDP_MIGRATE_EGFX`).
    #[cfg(feature = "multitransport")]
    fn route_egfx_over_udp(&self, messages: Vec<ironrdp_svc::SvcMessage>) -> Result<()> {
        let Some(sender) = self.multitransport_tunnel_sender.as_ref() else {
            warn!("egfx_on_udp set but no tunnel sender; dropping EGFX frame");
            return Ok(());
        };
        let Some(cookie) = self.multitransport_migration.as_ref().map(|m| m.cookie) else {
            warn!("egfx_on_udp set but no migration cookie; dropping EGFX frame");
            return Ok(());
        };
        let chunks = ironrdp_svc::StaticVirtualChannel::chunkify(messages)
            .map_err(|e| anyhow::anyhow!("chunkify EGFX for tunnel: {e}"))?;
        let n = chunks.len();
        for chunk in chunks {
            sender.send(cookie, chunk.into_inner());
        }
        trace!(chunks = n, "routed EGFX frame over the UDP tunnel");
        Ok(())
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
        if !result.reactivation {
            if let Some(offer) = result.multitransport_offered {
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
                    handler.mouse(mouse.into());
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
                if let Ok(resp) = decode::<ironrdp_pdu::rdp::multitransport::MultitransportResponsePdu>(
                    data.user_data.as_ref(),
                ) {
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

                rdp::headers::ShareDataPdu::AutoDetectRsp(response) => {
                    if let Some(ref mut ad) = self.autodetect {
                        if let Some(rtt_ms) = ad.handle_response(&response) {
                            debug!(rtt_ms, seq = response.sequence_number(), "RTT measured");
                        } else {
                            trace!(seq = response.sequence_number(), "Unmatched auto-detect response");
                        }
                    }
                }

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
                    handler.mouse(mouse.into());
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
