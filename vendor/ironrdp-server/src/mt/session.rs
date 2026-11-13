//! (macrdp divergence 12) The `RdpServer` side of macrdp's UDP multitransport.
//!
//! Declared from `server.rs` (`#[path]`) as a child of the `server` module so it can
//! extend `RdpServer` and read its private fields; `server.rs` itself only carries
//! one-line hook calls. Upstream's own multitransport (`with_udp_transport`) is a
//! separate implementation that stays inert: macrdp never sets `udp_bind_addr`.
//!
//! Lifecycle per connection: [`RdpServer::mt_offer`] (before negotiation) →
//! [`RdpServer::mt_on_finalized`] (after the acceptor sent the Initiate Multitransport
//! Request) → Soft-Sync and routing while the session runs → [`RdpServer::mt_teardown`]
//! once it ends. See `vendor/ironrdp-server/CLAUDE.md` (12) for the history behind
//! each rule.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};

use ironrdp_acceptor::{Acceptor, MultitransportSecurityRng};
use ironrdp_dvc as dvc;
use ironrdp_dvc::pdu::SoftSyncTunnelType;
use ironrdp_pdu::gcc::MultiTransportFlags;
use ironrdp_pdu::rdp::multitransport::{MultitransportResponsePdu, RequestedProtocol};
use ironrdp_svc::{ChannelFlags, SvcMessage, server_encode_svc_messages};
use ironrdp_tokio::FramedWrite;
use tokio::sync::mpsc;
use tracing::{debug, info, trace, warn};

use super::RdpServer;
#[cfg(feature = "egfx")]
use crate::GfxDvcBridge;
use crate::error::{ServerError, ServerErrorExt as _, ServerResult};
use crate::mt::audio_dvc::{AudioLossyDvc, AudioReliableDvc, NegotiatedAudioFormat};
#[cfg(feature = "egfx")]
use crate::mt::env_truthy;
use crate::mt::{CookieRegistry, MigrationState, MultitransportProvider, TunnelSender};

/// Configuration, set once at startup by macrdp, plus the current connection's state.
#[derive(Default)]
pub(super) struct MtState {
    provider: Option<Box<dyn MultitransportProvider>>,
    cookies: Option<CookieRegistry>,
    tunnel_sender: Option<TunnelSender>,
    egfx_on_lossy_handle: Option<Arc<AtomicBool>>,
    egfx_on_udp_handle: Option<Arc<AtomicBool>>,
    demigrate_request: Option<Arc<AtomicBool>>,
    migrate_egfx: bool,
    lossy_audio_formats: Option<Vec<ironrdp_rdpsnd::pdu::AudioFormat>>,
    lossy_audio_format: NegotiatedAudioFormat,
    conn: MtConnection,
}

/// One connection's multitransport state. Reset by [`RdpServer::mt_teardown`].
#[derive(Default)]
struct MtConnection {
    /// What the RNG adapter registered for this connection's offer.
    offer: Option<Arc<StdMutex<OfferRegistration>>>,
    /// The in-flight request, once the acceptor reported sending it.
    migration: Option<MigrationState>,
    /// Raised by the listener when it binds this connection's tunnel.
    tunnel_bound: Option<Arc<AtomicBool>>,
    /// The tunnel's client-to-server data; `client_loop` drains it.
    tunnel_inbound_rx: Option<mpsc::UnboundedReceiver<Vec<u8>>>,
    /// The client sent a successful Initiate Multitransport Response; an empty
    /// Soft-Sync is owed (see [`RdpServer::mt_on_response`]).
    soft_sync_owed: bool,
    #[cfg(feature = "egfx")]
    egfx_on_udp: bool,
    lossy_audio_block_no: u8,
    lossy_audio_streaming: bool,
}

/// The cookie and bound flag the RNG adapter registered, read back by the server.
#[derive(Default)]
struct OfferRegistration {
    cookie: Option<[u8; 16]>,
    tunnel_bound: Option<Arc<AtomicBool>>,
}

/// Supplies the acceptor's Initiate Multitransport Request with its cookie and
/// request id, registering the cookie with the listener *as it is generated*: the
/// client can only open the tunnel after receiving the request, so the listener
/// always knows the cookie first. The cookie is CSPRNG-generated, so it can't be
/// forged by someone who can see the predictable request id.
struct OfferRng {
    registry: CookieRegistry,
    inbound: Option<mpsc::UnboundedSender<Vec<u8>>>,
    registration: Arc<StdMutex<OfferRegistration>>,
}

impl MultitransportSecurityRng for OfferRng {
    fn fill_security_cookie(&mut self, cookie: &mut [u8; 16]) {
        if let Err(error) = getrandom::getrandom(cookie) {
            // The system RNG failing is near impossible; don't panic the server over
            // it. The binding still works (a registry match); only unpredictability
            // is lost in this degenerate case.
            tracing::error!(%error, "system RNG failed for the multitransport cookie; using a weak fallback");
            let seed = MT_REQUEST_ID.load(Ordering::Relaxed);
            for (i, b) in (0u32..).zip(cookie.iter_mut()) {
                *b = seed.wrapping_mul(2_654_435_761).wrapping_add(i).to_le_bytes()[0];
            }
        }
        let Some(inbound) = self.inbound.take() else {
            // A second request for the same connection never happens (the acceptor
            // sends one); keep the first registration rather than replace it.
            warn!("multitransport cookie requested twice for one connection; keeping the first");
            return;
        };
        let tunnel_bound = self.registry.register(*cookie, inbound);
        let mut registration = self
            .registration
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registration.cookie = Some(*cookie);
        registration.tunnel_bound = Some(tunnel_bound);
    }

    fn next_request_id(&mut self) -> u32 {
        MT_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
    }
}

/// Process-wide request id, so ids never repeat across connections.
static MT_REQUEST_ID: AtomicU32 = AtomicU32::new(1);

/// EXPERIMENTAL: migrate EGFX onto the tunnel (legacy env fallback for
/// `--udp-migrate-egfx`). Read once.
#[cfg(feature = "egfx")]
fn migrate_egfx_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| env_truthy("MACRDP_UDP_MIGRATE_EGFX"))
}

/// Isolation test: migrate EGFX onto the LOSSY (DTLS) tunnel instead of the reliable
/// one, to exercise the DTLS `RDP_TUNNEL_DATA` path with a proven payload. Read once.
#[cfg(feature = "egfx")]
fn migrate_egfx_lossy() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| env_truthy("MACRDP_UDP_MIGRATE_EGFX_LOSSY"))
}

/// Link-RTT gate: a connection whose accept-time TCP RTT is at or above
/// `MACRDP_UDP_OFFER_MAX_RTT_MS` (default 80; 0 disables) runs plain TCP from the
/// first byte. On overlay links (VPN, ZeroTier, mobile) the tunnel predictably
/// wedges. Read once.
fn offer_max_rtt_ms() -> u32 {
    static V: OnceLock<u32> = OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("MACRDP_UDP_OFFER_MAX_RTT_MS")
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(80)
    })
}

fn tunnel_name(tunnel: SoftSyncTunnelType) -> &'static str {
    if tunnel == SoftSyncTunnelType::LOSSY_UDP {
        "lossy (UdpFecL)"
    } else if tunnel == SoftSyncTunnelType::RELIABLE_UDP {
        "reliable (UdpFecR)"
    } else {
        "unknown"
    }
}

// ---- Configuration (macrdp calls these once, at startup) ----

impl RdpServer {
    pub fn set_multitransport_provider(&mut self, provider: Option<Box<dyn MultitransportProvider>>) {
        self.macrdp.mt.provider = provider;
    }

    pub fn set_multitransport_cookie_registry(&mut self, registry: Option<CookieRegistry>) {
        self.macrdp.mt.cookies = registry;
    }

    pub fn set_multitransport_tunnel_sender(&mut self, sender: Option<TunnelSender>) {
        self.macrdp.mt.tunnel_sender = sender;
    }

    /// Raised while EGFX rides the LOSSY tunnel, so the H.264 pipeline can arm
    /// ack-driven IDR recovery.
    pub fn set_egfx_on_lossy_handle(&mut self, handle: Option<Arc<AtomicBool>>) {
        self.macrdp.mt.egfx_on_lossy_handle = handle;
    }

    /// Raised while EGFX rides a UDP tunnel, so the H.264 pipeline applies
    /// frame-ack-lag backpressure.
    pub fn set_egfx_on_udp_handle(&mut self, handle: Option<Arc<AtomicBool>>) {
        self.macrdp.mt.egfx_on_udp_handle = handle;
    }

    /// Set by the H.264 pipeline when the reliable tunnel has wedged; EGFX then moves
    /// back to TCP for the rest of the session.
    pub fn set_demigrate_request_handle(&mut self, handle: Option<Arc<AtomicBool>>) {
        self.macrdp.mt.demigrate_request = handle;
    }

    pub fn set_migrate_egfx(&mut self, on: bool) {
        self.macrdp.mt.migrate_egfx = on;
    }

    /// Registers the dual audio DVCs (reliable handshake over TCP, lossy waves over
    /// the DTLS tunnel) when `Some`.
    pub fn set_multitransport_lossy_audio_formats(&mut self, formats: Option<Vec<ironrdp_rdpsnd::pdu::AudioFormat>>) {
        self.macrdp.mt.lossy_audio_formats = formats;
    }
}

// ---- Connection lifecycle ----

impl RdpServer {
    /// Before negotiation: offer UDP to this connection unless the tunnel-death
    /// cooldown or the link-RTT gate says not to. Not called for a preemption
    /// candidate: this state is shared with the live connection.
    pub(super) fn mt_offer(&mut self, acceptor: &mut Acceptor) {
        let Some(provider) = self.macrdp.mt.provider.as_ref() else {
            return;
        };
        let protocol = provider.requested_protocol();
        // After the listener declares a tunnel dead (an overlay network dropping
        // UDP), offers stay off for a cooldown, so the client's reconnect (mstsc
        // resets the session ~60 s after its tunnel dies) lands as stable plain TCP
        // instead of re-establishing a doomed tunnel and cycling.
        if self
            .macrdp
            .mt
            .cookies
            .as_ref()
            .is_some_and(CookieRegistry::multitransport_suppressed)
        {
            info!("multitransport offer SUPPRESSED (tunnel-death cooldown) — this connection runs plain TCP");
            return;
        }
        let max_rtt = offer_max_rtt_ms();
        let rtt = self
            .macrdp
            .link_rtt_ms
            .as_ref()
            .map_or(0, |c| c.load(Ordering::Relaxed));
        if max_rtt > 0 && rtt >= max_rtt {
            info!(
                link_rtt_ms = rtt,
                max_rtt_ms = max_rtt,
                "multitransport offer WITHHELD (link RTT above the offer gate) — this connection runs plain TCP; \
                 UDP tunnels wedge on high-latency overlay links"
            );
            return;
        }

        let transport = match protocol {
            RequestedProtocol::UdpFecR => MultiTransportFlags::TRANSPORT_TYPE_UDP_FECR,
            RequestedProtocol::UdpFecL => MultiTransportFlags::TRANSPORT_TYPE_UDP_FECL,
        };
        acceptor.set_multitransport_offer(Some(transport | MultiTransportFlags::SOFT_SYNC_TCP_TO_UDP));
        acceptor.set_multitransport_requested_protocol(protocol);
        if let Some(registry) = self.macrdp.mt.cookies.clone() {
            // The listener forwards this tunnel's client-to-server data here, keyed
            // by cookie; `client_loop` drains the receiver.
            let (inbound, inbound_rx) = mpsc::unbounded_channel();
            let registration = Arc::new(StdMutex::new(OfferRegistration::default()));
            acceptor.set_multitransport_security_rng(Box::new(OfferRng {
                registry,
                inbound: Some(inbound),
                registration: Arc::clone(&registration),
            }));
            self.macrdp.mt.conn.offer = Some(registration);
            self.macrdp.mt.conn.tunnel_inbound_rx = Some(inbound_rx);
        }
    }

    /// After a finalize pass: record the request the acceptor sent, so the client's
    /// response can be matched and Soft-Sync fired once its tunnel binds.
    pub(super) fn mt_on_finalized(&mut self, acceptor: &Acceptor, reactivation: bool) {
        if reactivation {
            return;
        }
        let Some(request) = acceptor.multitransport_request() else {
            return;
        };
        // Without a cookie registry there is no bound flag: the listener binds softly.
        self.macrdp.mt.conn.tunnel_bound = self.macrdp.mt.conn.offer.as_ref().and_then(|r| {
            r.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .tunnel_bound
                .clone()
        });
        self.macrdp.mt.conn.migration = Some(MigrationState {
            request_id: request.request_id,
            cookie: request.security_cookie,
            protocol: request.requested_protocol,
            soft_sync_sent: false,
        });
        debug!(
            request_id = request.request_id,
            protocol = ?request.requested_protocol,
            soft_sync = acceptor.multitransport_soft_sync_negotiated(),
            "Server Initiate Multitransport Request was sent by the acceptor"
        );
    }

    /// The connection ended, however it ended: reset per-connection state and retire
    /// its tunnel. Lowering the bound flag makes the listener treat the abandoned
    /// tunnel as a benign teardown (it ages out) rather than a death that would start
    /// the offer cooldown. Evicting the cookie here, not at activation, also covers a
    /// connection that died before activation (a cert-prompt broken pipe, a failed
    /// CredSSP), and stops a late tunnel bind from resurrecting a retired flag.
    pub(super) fn mt_teardown(&mut self) {
        let conn = core::mem::take(&mut self.macrdp.mt.conn);
        for handle in [
            &self.macrdp.mt.egfx_on_lossy_handle,
            &self.macrdp.mt.egfx_on_udp_handle,
            // A fresh connection retries UDP instead of instantly falling back.
            &self.macrdp.mt.demigrate_request,
        ]
        .into_iter()
        .flatten()
        {
            handle.store(false, Ordering::Relaxed);
        }
        if let Some(flag) = &conn.tunnel_bound {
            flag.store(false, Ordering::Relaxed);
        }
        let cookie = conn
            .offer
            .as_ref()
            .and_then(|r| r.lock().unwrap_or_else(std::sync::PoisonError::into_inner).cookie);
        if let (Some(cookie), Some(registry)) = (cookie, self.macrdp.mt.cookies.as_ref()) {
            registry.remove(&cookie);
        }
    }

    /// The tunnel's client-to-server data, for `client_loop` to drain.
    pub(super) fn mt_take_tunnel_inbound(&mut self) -> Option<mpsc::UnboundedReceiver<Vec<u8>>> {
        self.macrdp.mt.conn.tunnel_inbound_rx.take()
    }

    /// The client's Initiate Multitransport Response, from the message channel. mstsc
    /// never sends a success here (it signals success by creating the tunnel); a
    /// client that does gets the empty Soft-Sync the response gate owes, sent by
    /// [`Self::mt_send_owed_soft_sync`].
    pub(super) fn mt_on_response(&mut self, response: &MultitransportResponsePdu) {
        let Some(state) = self.macrdp.mt.conn.migration.as_mut() else {
            return;
        };
        if state.request_id != response.request_id {
            warn!(
                got = response.request_id,
                in_flight = state.request_id,
                "Initiate Multitransport Response request_id mismatch; ignoring"
            );
            return;
        }
        if !response.is_success() {
            debug!(
                request_id = response.request_id,
                protocol = ?state.protocol,
                hr = format_args!("{:#010x}", response.hr_response),
                "client could not establish UDP; staying on TCP"
            );
            self.macrdp.mt.conn.migration = None;
            return;
        }
        if state.soft_sync_sent {
            debug!(
                request_id = response.request_id,
                "Initiate Multitransport Response S_OK after Soft-Sync; ignoring"
            );
            return;
        }
        state.soft_sync_sent = true;
        self.macrdp.mt.conn.soft_sync_owed = true;
    }

    pub(super) async fn mt_send_owed_soft_sync(
        &mut self,
        writer: &mut impl FramedWrite,
        user_channel_id: u16,
    ) -> ServerResult<()> {
        if core::mem::take(&mut self.macrdp.mt.conn.soft_sync_owed) {
            debug!("Initiate Multitransport Response S_OK — Soft-Sync gate open");
            // Empty: EGFX migration is driven by the listener-bound gate, not this path.
            self.send_soft_sync(writer, user_channel_id, SoftSyncTunnelType::RELIABLE_UDP, Vec::new())
                .await?;
        }
        Ok(())
    }
}

// ---- Channels: Soft-Sync and routing ----

impl RdpServer {
    /// The dual audio DVCs, when lossy audio is configured.
    pub(super) fn mt_attach_audio_dvcs(&self, dvc: dvc::DrdynvcServer) -> dvc::DrdynvcServer {
        match &self.macrdp.mt.lossy_audio_formats {
            Some(formats) => dvc
                .with_dynamic_channel(AudioReliableDvc::new(
                    formats.clone(),
                    self.macrdp.mt.lossy_audio_format.clone(),
                ))
                .with_dynamic_channel(AudioLossyDvc::new()),
            None => dvc,
        }
    }

    fn tunnel_is_bound(&self) -> bool {
        self.macrdp
            .mt
            .conn
            .tunnel_bound
            .as_ref()
            .is_some_and(|f| f.load(Ordering::Relaxed))
    }

    /// Route an EGFX batch over the tunnel once EGFX has migrated. Returns the batch
    /// back for TCP when it hasn't, or when the H.264 watchdog asked to de-migrate:
    /// the reliable tunnel wedged (acks silent while shipping), so EGFX goes back to
    /// TCP DRDYNVC for the rest of the session (mstsc renders EGFX on TCP after a
    /// Soft-Sync). One way; reset on reconnect.
    #[cfg(feature = "egfx")]
    pub(super) fn mt_route_egfx(&mut self, messages: Vec<SvcMessage>) -> ServerResult<Option<Vec<SvcMessage>>> {
        if !self.macrdp.mt.conn.egfx_on_udp {
            return Ok(Some(messages));
        }
        if self
            .macrdp
            .mt
            .demigrate_request
            .as_ref()
            .is_some_and(|h| h.load(Ordering::Relaxed))
        {
            self.macrdp.mt.conn.egfx_on_udp = false;
            if let Some(handle) = &self.macrdp.mt.egfx_on_udp_handle {
                handle.store(false, Ordering::Relaxed);
            }
            warn!(
                "EGFX-over-UDP reliable tunnel wedged — watchdog de-migrating EGFX to TCP DRDYNVC for the rest of this session"
            );
            return Ok(Some(messages));
        }
        self.route_dvc_over_udp(messages)?;
        Ok(None)
    }

    /// Called after EGFX ships over TCP. Once the tunnel is bound and EGFX is live
    /// (its DVC is open, the client is fully in DVC mode), send the one Soft-Sync this
    /// connection gets. mstsc signals multitransport success by creating the tunnel,
    /// never by a TCP response, so this is the real success gate.
    #[cfg(feature = "egfx")]
    pub(super) async fn mt_soft_sync_on_egfx(
        &mut self,
        writer: &mut impl FramedWrite,
        user_channel_id: u16,
    ) -> ServerResult<()> {
        if !self.tunnel_is_bound() {
            return Ok(());
        }

        // Lossy audio: Soft-Sync the LOSSY audio DVC onto the LOSSY tunnel (the inverse
        // of EGFX, which goes to the reliable one). Its format handshake runs on the
        // reliable AUDIO_PLAYBACK_DVC over TCP: mstsc tears the socket down if formats
        // arrive on the lossy name. Wait for the channel to open before claiming the
        // one-time guard, so an unopened channel just retries on the next frame.
        if self.macrdp.mt.lossy_audio_formats.is_some() {
            let Some(audio_id) = self.open_channel_id::<AudioLossyDvc>() else {
                return Ok(());
            };
            if !self.claim_soft_sync() {
                return Ok(());
            }
            debug!(
                audio_channel_id = audio_id,
                "UDP lossy tunnel bound — Soft-Sync migrating AUDIO_PLAYBACK_LOSSY_DVC onto the LOSSY (UdpFecL) \
                 tunnel (waves only; the format handshake runs on the reliable AUDIO_PLAYBACK_DVC over TCP)"
            );
            return self
                .send_soft_sync(writer, user_channel_id, SoftSyncTunnelType::LOSSY_UDP, vec![audio_id])
                .await;
        }

        // EXPERIMENTAL (`--udp-migrate-egfx`, or `MACRDP_UDP_MIGRATE_EGFX`): name the
        // EGFX DVC so the client moves it onto the tunnel. Off, an empty Soft-Sync is
        // the proven safe spike: EGFX stays on TCP.
        let migrate = self.macrdp.mt.migrate_egfx || migrate_egfx_enabled();
        let egfx_id = if migrate {
            let id = self.open_channel_id::<GfxDvcBridge>();
            if id.is_none() {
                // Retry on the next frame rather than spend the one Soft-Sync on nothing.
                return Ok(());
            }
            id
        } else {
            None
        };
        if !self.claim_soft_sync() {
            return Ok(());
        }
        let tunnel = if migrate_egfx_lossy() {
            SoftSyncTunnelType::LOSSY_UDP
        } else {
            SoftSyncTunnelType::RELIABLE_UDP
        };
        if let Some(id) = egfx_id {
            self.macrdp.mt.conn.egfx_on_udp = true;
            if let Some(handle) = &self.macrdp.mt.egfx_on_udp_handle {
                handle.store(true, Ordering::Relaxed);
            }
            // The reliable tunnel never drops; only the lossy one needs ack-driven
            // IDR recovery.
            if tunnel == SoftSyncTunnelType::LOSSY_UDP
                && let Some(handle) = &self.macrdp.mt.egfx_on_lossy_handle
            {
                handle.store(true, Ordering::Relaxed);
            }
            debug!(
                gfx_channel_id = id,
                "--udp-migrate-egfx: Soft-Sync will migrate the EGFX DVC onto the UDP tunnel"
            );
        }
        debug!("UDP tunnel bound + EGFX active — Soft-Sync gate open");
        self.send_soft_sync(writer, user_channel_id, tunnel, egfx_id.into_iter().collect())
            .await
    }

    /// The channel id of `T`'s DVC, once it's open.
    fn open_channel_id<T: dvc::DvcServerProcessor + 'static>(&mut self) -> Option<u32> {
        let drdynvc = self.get_svc_processor::<dvc::DrdynvcServer>()?;
        drdynvc
            .get_channel_id_by_type::<T>()
            .filter(|&id| drdynvc.is_channel_opened(id))
    }

    /// Claim the one Soft-Sync this connection gets. False when it's already spent or
    /// there's no request in flight.
    #[cfg(feature = "egfx")]
    fn claim_soft_sync(&mut self) -> bool {
        match self.macrdp.mt.conn.migration.as_mut() {
            Some(state) if !state.soft_sync_sent => {
                state.soft_sync_sent = true;
                true
            }
            _ => false,
        }
    }

    /// Send a `DYNVC_SOFT_SYNC_REQUEST` over the DRDYNVC static channel on TCP. Empty
    /// `channel_ids` migrates nothing (everything stays on TCP). Channel routing stays
    /// macrdp's (see the vendored ironrdp-dvc's `request_soft_sync`), which is what
    /// keeps the TCP fallback for a wedged tunnel possible.
    async fn send_soft_sync(
        &mut self,
        writer: &mut impl FramedWrite,
        user_channel_id: u16,
        tunnel: SoftSyncTunnelType,
        channel_ids: Vec<u32>,
    ) -> ServerResult<()> {
        let Some(drdynvc_channel_id) = self.get_channel_id_by_type::<dvc::DrdynvcServer>() else {
            warn!("No DRDYNVC channel; cannot send Soft-Sync request");
            return Ok(());
        };
        let Some(drdynvc) = self.get_svc_processor::<dvc::DrdynvcServer>() else {
            return Ok(());
        };
        let migrating = !channel_ids.is_empty();
        let message = match drdynvc.request_soft_sync(tunnel, channel_ids) {
            Ok(message) => message,
            Err(error) => {
                warn!(%error, "Soft-Sync request refused; staying on TCP");
                return Ok(());
            }
        };
        let data = server_encode_svc_messages(vec![message], drdynvc_channel_id, user_channel_id)
            .map_err(ServerError::encode)?;
        writer
            .write_all(&data)
            .await
            .map_err(|e| ServerError::io("write_all", e))?;
        if migrating {
            debug!(
                tunnel = tunnel_name(tunnel),
                "Sent DYNVC_SOFT_SYNC_REQUEST (migrating channels onto the UDP tunnel)"
            );
        } else {
            debug!(
                tunnel = tunnel_name(tunnel),
                "Sent DYNVC_SOFT_SYNC_REQUEST (empty channel list — nothing migrated, stays on TCP)"
            );
        }
        Ok(())
    }

    /// Hand DVC messages to the listener for this connection's tunnel. The tunnel
    /// carries the BARE DRDYNVC PDU: `RDP_TUNNEL_DATA` is the framing, so each message
    /// is encoded unframed (a `CHANNEL_PDU_HEADER` would make the client misparse the
    /// stream). Best effort: with no sender or cookie the data is dropped.
    fn route_dvc_over_udp(&self, messages: Vec<SvcMessage>) -> ServerResult<()> {
        let Some(sender) = self.macrdp.mt.tunnel_sender.as_ref() else {
            warn!("migrated channel data but no tunnel sender; dropping");
            return Ok(());
        };
        let Some(cookie) = self.macrdp.mt.conn.migration.as_ref().map(|m| m.cookie) else {
            warn!("migrated channel data but no migration cookie; dropping");
            return Ok(());
        };
        let count = messages.len();
        for message in messages {
            sender.send(cookie, message.encode_unframed_pdu().map_err(ServerError::encode)?);
        }
        trace!(pdus = count, "routed DVC PDUs over the UDP tunnel");
        Ok(())
    }

    /// Ship a wave over the lossy tunnel if that path is live (the reliable audio DVC
    /// negotiated a format, the tunnel is bound, the lossy DVC is open). Returns the
    /// wave back otherwise, for static RDPSND over TCP, so exactly one playback path
    /// is ever in use.
    pub(super) fn mt_route_lossy_audio(&mut self, data: Vec<u8>, ts: u32) -> Option<Vec<u8>> {
        if self.macrdp.mt.lossy_audio_formats.is_none()
            || self.macrdp.mt.tunnel_sender.is_none()
            || self.macrdp.mt.conn.migration.is_none()
            || !self.tunnel_is_bound()
        {
            return Some(data);
        }
        let Some(format_no) = self.macrdp.mt.lossy_audio_format.get() else {
            return Some(data);
        };
        let Some(lossy_id) = self.open_channel_id::<AudioLossyDvc>() else {
            return Some(data);
        };
        let conn = &mut self.macrdp.mt.conn;
        if !conn.lossy_audio_streaming {
            conn.lossy_audio_streaming = true;
            warn!(
                format_no,
                lossy_channel_id = lossy_id,
                "streaming Wave2 audio over the LOSSY UDP/DTLS tunnel (AUDIO_PLAYBACK_LOSSY_DVC) — static rdpsnd now silent"
            );
        }
        let block_no = conn.lossy_audio_block_no;
        conn.lossy_audio_block_no = block_no.wrapping_add(1);
        let wave = crate::mt::audio_dvc::lossy_wave_dvc_message(block_no, ts, format_no, data);
        let routed = dvc::encode_dvc_messages(lossy_id, vec![wave], ChannelFlags::empty())
            .map_err(ServerError::encode)
            .and_then(|messages| self.route_dvc_over_udp(messages));
        if let Err(error) = routed {
            warn!(%error, "lossy audio wave route over UDP tunnel failed");
        }
        None
    }

    /// One client-to-server PDU from the tunnel (a bare DRDYNVC PDU, e.g. an EGFX
    /// frame acknowledgement): feed it to the DRDYNVC processor and ship any replies
    /// back over the tunnel. Errors are logged: the optional UDP path must never end
    /// the TCP-authoritative session.
    pub(super) fn mt_process_tunnel_inbound(&mut self, pdu: &[u8]) {
        use ironrdp_svc::SvcProcessor as _;
        let Some(drdynvc) = self.get_svc_processor::<dvc::DrdynvcServer>() else {
            warn!("inbound tunnel data but no DRDYNVC channel; dropping");
            return;
        };
        let replies = match drdynvc.process(pdu) {
            Ok(replies) => replies,
            Err(error) => {
                warn!(%error, "failed to process inbound tunnel DRDYNVC PDU");
                return;
            }
        };
        if !replies.is_empty()
            && let Err(error) = self.route_dvc_over_udp(replies)
        {
            warn!(%error, "failed to ship tunnel reply PDUs");
        }
    }
}
