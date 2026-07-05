//! (vendored, feature=multitransport) UDP listener + RDPEUDP SYN/SYN+ACK
//! handshake on the wire — **M3b**.
//!
//! Owns a [`tokio::net::UdpSocket`], demultiplexes inbound datagrams by peer
//! address, and drives a per-peer [`RdpeudpState`] (the sans-I/O reliability
//! state machine from `ironrdp-rdpeudp`) through the RDPEUDP handshake: a real
//! client's SYN is answered with a wire-correct SYN+ACK (matching real Windows;
//! see `ironrdp-rdpeudp`'s `Datagram::syn_ack`), negotiating the data version
//! (V3 = RDPEUDP2). The reliability/codec logic lives in `ironrdp-rdpeudp`; this
//! file is purely the I/O layer.
//!
//! # Scope (M3b)
//!
//! - Handshake only: SYN → SYN+ACK, version negotiation, established detection.
//!   No TLS, no MS-RDPEMT tunnel, no channel migration yet (M4/M5), and no data
//!   delivery up to a consumer.
//! - **Cookie validation is soft**: the client's SYNEX `cookieHash` is logged but
//!   not verified against the issued security cookie. This listener produces the
//!   first macrdp↔client capture (our own cookie + the client's resulting hash),
//!   which is what's needed to derive/verify the exact hash formula; strict
//!   binding to a specific TCP session lands in M3c.
//! - Peers are demuxed by source address and **idle-timeout garbage-collected**
//!   (M3c): a peer with no *inbound* datagram for `PEER_IDLE_TIMEOUT_MS` is evicted
//!   by `gc_idle_peers` (its `peers` entry + any `bound_addrs` cookie→addr mapping),
//!   so a dead RDP session's reliability SM stops RTO-retransmitting to a gone client
//!   and the maps don't leak for the process lifetime. The prompt server→listener
//!   "instant retire-on-disconnect" signal is the still-deferred other half of M3c
//!   (the idle GC is the needed backstop regardless).
//!
//! Cross-platform (pure tokio/std networking), so Linux CI exercises it too — see
//! the loopback integration test in the macrdp crate (the vendored server is
//! built `test = false`).

use std::collections::HashMap;
use std::io;
use std::io::Read as _;
use std::io::Write as _;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use ironrdp_rdpeudp::datagram::Datagram;
use ironrdp_rdpeudp::pdu::FecFlags;
use ironrdp_rdpeudp::state::{Config, DeliveryMode, RdpeudpState, Role};

use crate::multitransport::dtls::{DtlsConn, DtlsServerContext};
use crate::multitransport::{CookieRegistry, TunnelOutbound};
use tokio::net::UdpSocket;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;
use tokio_rustls::rustls::{ServerConfig, ServerConnection};
use tracing::{debug, trace, warn};

/// Configuration for the UDP multitransport listener.
#[derive(Debug, Clone, Copy)]
pub struct ListenerConfig {
    /// Advertised receive window, in datagrams (`uReceiveWindowSize`).
    pub recv_window: u16,
    /// Our MTU (1132..=1232 per spec). SYN/SYN+ACK packets are zero-padded to it.
    pub mtu: u16,
    /// Base server initial-sequence-number; each new peer session derives its ISN
    /// from this. The exact value is not validated by clients — M3c will seed it
    /// from a CSPRNG.
    pub server_isn_seed: u32,
    /// Retransmit timeout, milliseconds (passed to the reliability SM).
    pub rto_ms: u64,
}

impl Default for ListenerConfig {
    fn default() -> Self {
        Self {
            recv_window: 64,
            mtu: 1232,
            server_isn_seed: 0x5052_4400, // "PRD\0"-ish; replaced by a CSPRNG seed in M3c
            rto_ms: 300,
        }
    }
}

/// A per-peer handshake/transport session: the reliability SM, the reassembled
/// inbound reliable byte-stream, and (M4b) the server-side TLS connection that
/// secures the MS-RDPEMT tunnel riding that stream.
struct Peer {
    sm: RdpeudpState,
    /// The client's reliable application stream, reassembled in order from the
    /// SM's `delivered` output. After the handshake this carries the client's
    /// MS-RDPEMT-over-TLS bytes (its TLS ClientHello first). Kept for the
    /// ClientHello sniff log; the bytes themselves feed `tls`.
    inbound: Vec<u8>,
    /// Whether we've already logged the TLS ClientHello sniff (avoid log spam).
    tls_hello_logged: bool,
    /// The MS-RDPEMT TLS server connection (M4b), created lazily on the first
    /// reliable data when a TLS config is configured. `read_tls`/`write_tls` are
    /// driven sans-I/O off the reliable byte-stream. `None` until then, or when
    /// the listener has no TLS config (the handshake-only / test path).
    tls: Option<ServerConnection>,
    /// Whether we've logged TLS-handshake completion (avoid log spam).
    tls_done_logged: bool,
    /// TLS-decrypted plaintext awaiting MS-RDPEMT tunnel-PDU parsing (M4c). The
    /// client's `RDP_TUNNEL_CREATEREQUEST` arrives here once TLS is up; complete
    /// tunnel PDUs are consumed from the front.
    emt_inbound: Vec<u8>,
    /// Whether we've answered the tunnel `CREATEREQUEST` with a `CREATERESPONSE`
    /// (M4c). The client retransmits the request until it sees the response, so
    /// we reply once and then ignore the retransmits.
    tunnel_created: bool,
    /// (M5c step 3b) The owning connection's inbound-tunnel-data sink, set on a
    /// successful cookie-bound CREATEREQUEST (`CookieRegistry::take`). Each
    /// inbound `RDP_TUNNEL_DATA` HigherLayerData (a bare DRDYNVC PDU from the
    /// migrated channel — e.g. an EGFX frame ack) is forwarded here to the
    /// connection's `client_loop`. `None` on the soft-bound (no-registry) test
    /// path — inbound channel data is then logged and dropped.
    inbound_sink: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
    /// (P2.0 spike) Set once this peer is observed sending a **DTLS** ClientHello
    /// — i.e. it opened a *lossy* (`UdpFecL`) flow. When set, we do NOT feed this
    /// peer's bytes into rustls (DTLS records aren't TLS records). Instead, when a
    /// DTLS server context is configured, P2.1 drives a DTLS handshake (below).
    dtls_observed: bool,
    /// (P2.1a) The DTLS 1.2 server handshake for a lossy peer, created lazily on
    /// the first delivered data once `dtls_observed` and a DTLS context is set.
    /// `None` on the reliable path or the observe-only (no-DTLS-context) path.
    dtls: Option<DtlsConn>,
    /// Whether we've logged DTLS-handshake completion (avoid log spam).
    dtls_done_logged: bool,
    /// The owning connection's tunnel-bound flag, kept after a cookie-bound
    /// CREATEREQUEST (`CookieRegistry::take` now returns it alongside the
    /// sink). Lets tunnel-DEATH detection flip it back **false**: the server
    /// re-checks the flag per audio wave (`lossy_audio_target`), so audio
    /// falls back to the static TCP channel the moment a dead tunnel is
    /// declared. `None` until bound / on the soft-bound test path / after the
    /// peer has been declared dead (cleared so death fires once).
    bound_flag: Option<Arc<core::sync::atomic::AtomicBool>>,
    /// (M3c) Wall-clock ms (listener `start.elapsed()`) of the last *inbound*
    /// datagram from this peer. Drives `gc_idle_peers`: a dead client (its RDP/TCP
    /// session gone) stops acking, so this stops advancing while the reliability SM
    /// would otherwise RTO-retransmit unacked data to it forever and the
    /// `peers`/`bound_addrs` entries would leak for the process lifetime.
    last_seen_ms: u64,
}

/// (P2.0 spike) Scan a raw RDPEUDP datagram for a **DTLS handshake** record — the
/// signal that a client opened a *lossy* flow (vs a TLS ClientHello on a reliable
/// flow). A DTLS record header is ContentType `0x16` (handshake) followed by the
/// DTLS ProtocolVersion `0xFEFF` (DTLS 1.0) or `0xFEFD` (DTLS 1.2). The record
/// sits at a header-dependent offset inside the datagram, so rather than compute
/// it we scan for the 3-byte signature — specific enough for a diagnostic. Returns
/// the matched version bytes for the log, if any.
fn sniff_dtls_client_hello(datagram: &[u8]) -> Option<[u8; 2]> {
    datagram.windows(3).find_map(|w| {
        (w[0] == 0x16 && w[1] == 0xFE && (w[2] == 0xFF || w[2] == 0xFD)).then_some([w[1], w[2]])
    })
}

/// A running UDP multitransport listener. The receive loop runs on a spawned
/// task; dropping the listener aborts it (and closes the socket).
pub struct UdpMultitransportListener {
    local_addr: SocketAddr,
    task: JoinHandle<()>,
}

impl UdpMultitransportListener {
    /// Bind a UDP socket and start serving the RDPEUDP handshake on a background
    /// task. `addr` is typically the same address/port as the TCP RDP listener
    /// (the client reuses the server address for the UDP flow), or `:0` in tests.
    ///
    /// `tls_config` is the rustls server config that secures the MS-RDPEMT tunnel
    /// (M4b) — pass the SAME cert as the main TCP connection. `None` runs the
    /// handshake-only path (no TLS), used by the loopback handshake test.
    ///
    /// `cookie_registry` (M5a) is the shared set of issued multitransport cookies
    /// (the same one handed to
    /// [`RdpServer::set_multitransport_cookie_registry`](crate::RdpServer::set_multitransport_cookie_registry)).
    /// When set, an inbound tunnel `CREATEREQUEST` is accepted only if its echoed
    /// cookie is registered (binding the UDP flow to a real TCP session, one-time
    /// use). `None` leaves binding soft (accept any cookie — handshake-test path).
    ///
    /// `dtls_config` (Phase 2 / P2.1a) is the DTLS 1.2 server context used to
    /// secure the LOSSY (`UdpFecL`) flow (built from the SAME cert as the TCP /
    /// reliable path). When set, a peer observed sending a DTLS ClientHello (a
    /// lossy flow) is driven through a DTLS handshake instead of rustls. `None`
    /// leaves the lossy flow observe-only (the P2.0 spike behavior).
    pub async fn bind(
        addr: SocketAddr,
        cfg: ListenerConfig,
        tls_config: Option<Arc<ServerConfig>>,
        cookie_registry: Option<CookieRegistry>,
        tunnel_rx: Option<UnboundedReceiver<TunnelOutbound>>,
        dtls_config: Option<DtlsServerContext>,
        congestion_retransmits: Option<Arc<AtomicU64>>,
    ) -> io::Result<Self> {
        let socket = UdpSocket::bind(addr).await?;
        let local_addr = socket.local_addr()?;
        let socket = Arc::new(socket);
        debug!(
            %local_addr, ?cfg,
            tls = tls_config.is_some(),
            dtls = dtls_config.is_some(),
            cookie_binding = cookie_registry.is_some(),
            tunnel_handoff = tunnel_rx.is_some(),
            "RDPEUDP multitransport listener bound"
        );
        let task = tokio::spawn(run_recv_loop(
            Arc::clone(&socket),
            cfg,
            tls_config,
            cookie_registry,
            tunnel_rx,
            dtls_config,
            congestion_retransmits,
        ));
        Ok(Self { local_addr, task })
    }

    /// The actual bound address (useful when binding to an ephemeral `:0` port).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

impl Drop for UdpMultitransportListener {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Does this encoded datagram have the v1 SYN flag set (a SYN or SYN+ACK)?
/// Such handshake packets must be zero-padded to the MTU.
fn is_syn_family(bytes: &[u8]) -> bool {
    Datagram::peek_fec_flags(bytes).is_some_and(|f| f.contains(FecFlags::SYN))
}

/// Send every datagram the SM produced, zero-padding SYN-family packets to the
/// MTU (MS-RDPEUDP path-MTU validation). A send error is per-datagram (UDP), so
/// log and keep going.
async fn send_datagrams(socket: &UdpSocket, peer_addr: SocketAddr, to_send: Vec<Vec<u8>>, mtu: u16) {
    for mut dg in to_send {
        if is_syn_family(&dg) && dg.len() < mtu as usize {
            dg.resize(mtu as usize, 0);
        }
        if let Err(e) = socket.send_to(&dg, peer_addr).await {
            trace!(error = %e, %peer_addr, "udp send_to error");
        }
    }
}

/// (Soak fix) Pump every established peer's reliability state machine on a timer
/// tick (`step(now, None)`), shipping any due retransmits / queued data / owed ACK.
/// This is what keeps the RTO retransmit clock running when neither inbound nor
/// outbound traffic is flowing — without it a lossy link deadlocks (see the
/// `retransmit_tick` rationale in `run_recv_loop`). Idle ticks emit nothing.
async fn pump_peers_on_timer(
    socket: &UdpSocket,
    peers: &mut HashMap<SocketAddr, Peer>,
    now_ms: u64,
    mtu: u16,
) -> usize {
    let mut total_retransmits = 0usize;
    for (addr, peer) in peers.iter_mut() {
        if !peer.sm.is_established() {
            continue;
        }
        let out = peer.sm.step(now_ms, None);
        let (retransmits, syn_retransmit) = (out.retransmits, out.syn_retransmit);
        total_retransmits += retransmits;
        if !out.to_send.is_empty() {
            send_datagrams(socket, *addr, out.to_send, mtu).await;
        }
        if retransmits > 0 || syn_retransmit {
            debug!(peer_addr = %addr, retransmits, syn_retransmit, "RDPEUDP RTO retransmit (timer)");
        }
    }
    total_retransmits
}

/// (M3c) Evict a peer after this many ms with no *inbound* datagram.
///
/// CORRECTED 2026-06-29 (was 10_000): the original 10s rested on the assumption that
/// a live client always sends frequent RDPEUDP keepalive/delayed ACKs (~200/s). That
/// holds only while the picture is *active* — when the screen goes idle, **mstsc drops
/// to a ~15s UDP keepalive cadence** (verified: inbound datagrams exactly 15s apart
/// once frame-acks stop). The 10s timeout then reaped the peer *between* two keepalives
/// — killing the UDP tunnel of a fully live TCP session (audio still flowing), so EGFX
/// froze permanently until reconnect (the client's next keepalive isn't a SYN, so the
/// peer is never recreated). The threshold must comfortably exceed the client's idle
/// keepalive interval. 60s = 4× the observed 15s — still reclaims a genuinely dead peer
/// promptly enough (the original "UDP keeps retransmitting after disconnect" leak
/// becomes ≤60s instead of indefinite). The fully robust fix is an explicit
/// TCP-session-close → evict signal (the deferred half of M3c); until then this
/// activity-based backstop must be generous enough not to reap an idle-but-live client.
const PEER_IDLE_TIMEOUT_MS: u64 = 60_000;

/// Tunnel-death detection: a peer with a BOUND tunnel (`bound_flag` kept from
/// the cookie bind) that has been inbound-silent past `dead_ms` has lost its
/// UDP path — an overlay network (ZeroTier) dropping UDP, a NAT rebind, a
/// de-migrated-and-abandoned tunnel. Two actions, both aimed at what the
/// server CAN still fix (keepalives can't — if the path is dead they don't
/// arrive either):
/// - flip the connection's tunnel-bound flag **false** → the server's
///   per-wave `lossy_audio_target` check fails → audio falls back to the
///   static TCP channel immediately (EGFX has its own ack-silence watchdog);
/// - start the multitransport-offer **cooldown** → when the client's own
///   ~60 s dead-tunnel timeout resets the session and it auto-reconnects,
///   the new connection gets NO multitransport offer and lands plain-TCP,
///   breaking the observed reset→reconnect→reset cycle (live 2026-07-04
///   over ZeroTier).
/// The flag is cleared after firing so a peer is declared dead exactly once;
/// the 60 s idle GC still evicts it later. O(peers) per tick.
///
/// A peer whose shared flag has ALREADY been lowered by the server (the
/// post-connection reset retires the tunnel when its owning TCP session ends)
/// is benign teardown, not a wedge: the marker is dropped silently and the
/// peer just ages out via the idle GC — no death declaration, no cooldown.
/// Without that distinction every ended session's abandoned tunnel "died"
/// ~30 s later and put healthy links into the 10-min offer cooldown (observed
/// live 2026-07-06 on LAN after a blank-recovery drop).
fn check_dead_tunnels(
    peers: &mut HashMap<SocketAddr, Peer>,
    cookie_registry: Option<&CookieRegistry>,
    now_ms: u64,
    dead_ms: u64,
    cooldown_secs: u64,
) {
    if dead_ms == 0 {
        return;
    }
    for (addr, peer) in peers.iter_mut() {
        if peer.bound_flag.is_none() || now_ms.saturating_sub(peer.last_seen_ms) <= dead_ms {
            continue;
        }
        let flag = peer.bound_flag.take().expect("checked is_none above");
        if !flag.load(core::sync::atomic::Ordering::Relaxed) {
            // Already retired by the server (session over) — benign, not a wedge.
            debug!(
                peer_addr = %addr,
                idle_ms = now_ms.saturating_sub(peer.last_seen_ms),
                "abandoned UDP tunnel from an ended session — retiring quietly (no cooldown)"
            );
            continue;
        }
        flag.store(false, core::sync::atomic::Ordering::Relaxed);
        if cooldown_secs > 0 {
            if let Some(reg) = cookie_registry {
                reg.suppress_multitransport(std::time::Duration::from_secs(cooldown_secs));
            }
        }
        warn!(
            peer_addr = %addr,
            idle_ms = now_ms.saturating_sub(peer.last_seen_ms),
            dead_ms,
            cooldown_secs,
            "UDP tunnel DEAD (bound peer inbound-silent past the threshold) — \
             tunnel-dependent channels fall back to TCP now, and multitransport \
             offers are suppressed for the cooldown so the client's likely \
             dead-tunnel session reset reconnects as a stable plain-TCP session"
        );
    }
}

/// (M3c) Idle-timeout garbage collection. Drop every peer whose last *inbound*
/// datagram is older than `PEER_IDLE_TIMEOUT_MS`, along with any `bound_addrs`
/// cookie→addr mapping pointing at it. Without this, peers inserted by
/// `run_recv_loop` are never removed: a client whose RDP/TCP session has gone away
/// stops acking but the reliability SM keeps RTO-retransmitting unacked EGFX to it
/// forever (`pump_peers_on_timer`), and over a long-running server dead peers
/// accumulate unbounded. Activity-based, so it covers graceful, abrupt, and crashed
/// disconnects uniformly. O(peers) on each `retransmit_tick`; peers is a handful.
fn gc_idle_peers(
    peers: &mut HashMap<SocketAddr, Peer>,
    bound_addrs: &mut HashMap<[u8; 16], SocketAddr>,
    now_ms: u64,
) {
    let gone: Vec<SocketAddr> = peers
        .iter()
        .filter(|(_, p)| now_ms.saturating_sub(p.last_seen_ms) > PEER_IDLE_TIMEOUT_MS)
        .map(|(addr, _)| *addr)
        .collect();
    for addr in gone {
        let idle_ms = match peers.remove(&addr) {
            Some(p) => {
                // Defensive: lower the shared tunnel-bound flag on eviction. With
                // the default thresholds tunnel-death (30 s) always fires before
                // this GC (60 s) and takes the flag itself — but if an operator
                // raises MACRDP_UDP_TUNNEL_DEAD_SECS to >= the GC timeout, the
                // eviction would otherwise strand the server-side flag TRUE
                // forever: no death declared, no cooldown, and lossy audio keeps
                // routing into a tunnel that no longer exists (permanent silence
                // with no TCP fallback).
                if let Some(flag) = p.bound_flag {
                    flag.store(false, core::sync::atomic::Ordering::Relaxed);
                }
                now_ms.saturating_sub(p.last_seen_ms)
            }
            None => 0,
        };
        // Drop any cookie→addr bindings for the evicted peer so a stale cookie can't
        // route server tunnel data to it (or to a later peer that reuses the addr).
        bound_addrs.retain(|_, a| *a != addr);
        debug!(
            peer_addr = %addr,
            idle_ms,
            timeout_ms = PEER_IDLE_TIMEOUT_MS,
            "evicted idle UDP peer (no inbound; reliability retransmits stop, state reclaimed)"
        );
    }
}

/// (M5c) Ship one server-originated HigherLayerData chunk to a bound peer: wrap it
/// in `RDP_TUNNEL_DATA` (action 0x2), encrypt through the peer's MS-RDPEMT TLS, and
/// send the resulting ciphertext reliably over RDPEUDP. Best-effort — an unbound
/// cookie / unknown peer / no-TLS peer / write error is logged and dropped (the
/// tunnel is the optional UDP fast-path; the TCP path remains authoritative).
async fn ship_outbound(
    socket: &UdpSocket,
    peers: &mut HashMap<SocketAddr, Peer>,
    bound_addrs: &HashMap<[u8; 16], SocketAddr>,
    cookie: [u8; 16],
    higher_layer: &[u8],
    now_ms: u64,
    mtu: u16,
) {
    use ironrdp_rdpeudp::emt;

    let Some(&peer_addr) = bound_addrs.get(&cookie) else {
        trace!("tunnel data for an unbound cookie; dropping");
        return;
    };
    let Some(peer) = peers.get_mut(&peer_addr) else {
        trace!(%peer_addr, "tunnel data for an unknown peer; dropping");
        return;
    };
    let pdu = emt::encode_tunnel_data(higher_layer);
    let Peer { sm, tls, dtls, .. } = peer;

    // The flow is secured by exactly one of rustls (reliable UdpFecR) or DTLS
    // (lossy UdpFecL). Encrypt the RDP_TUNNEL_DATA through whichever this peer
    // holds, then ship the ciphertext reliably over the RDPEUDP SM.
    if let Some(tls) = tls.as_mut() {
        if tls.writer().write_all(&pdu).is_err() {
            warn!(%peer_addr, "failed to write RDP_TUNNEL_DATA into TLS");
            return;
        }
        let mut tls_out = Vec::new();
        while tls.wants_write() {
            if tls.write_tls(&mut tls_out).is_err() {
                break;
            }
        }
        if !tls_out.is_empty() {
            let o = sm.enqueue(now_ms, &tls_out);
            send_datagrams(socket, peer_addr, o.to_send, mtu).await;
        }
    } else if let Some(dtls) = dtls.as_mut().filter(|c| c.is_handshake_done()) {
        // P2.4b: lossy (UdpFecL) audio rides the DTLS-secured tunnel.
        match dtls.send(&pdu) {
            Ok(dgs) => {
                for dg in dgs {
                    let o = sm.enqueue(now_ms, &dg);
                    send_datagrams(socket, peer_addr, o.to_send, mtu).await;
                }
            }
            Err(e) => warn!(%peer_addr, error = %e, "failed to encrypt RDP_TUNNEL_DATA through DTLS"),
        }
    } else {
        warn!(%peer_addr, "tunnel data to a peer with no TLS/DTLS (or DTLS not ready); dropping");
    }
}

/// What `handle_emt_tunnel` decided this call: the cookie of a tunnel bound (so
/// the caller can map it to the peer for the server→listener handoff), and the
/// plaintext `CREATERESPONSE` to send back (the caller encrypts + ships it
/// through whichever transport secures this flow — rustls for reliable, DTLS for
/// lossy). Transport-agnostic so both flows share the EMT PDU parsing.
#[derive(Default)]
struct EmtTunnelOutcome {
    bound_cookie: Option<[u8; 16]>,
    response: Option<Vec<u8>>,
}

/// Parse complete MS-RDPEMT tunnel PDUs from the *decrypted* plaintext and, on
/// the client's `RDP_TUNNEL_CREATEREQUEST`, return a `CREATERESPONSE(S_OK)` for
/// the caller to encrypt + send. Consumes parsed PDUs from the front of `buf`;
/// leaves a partial trailing PDU buffered for the next call. The client
/// retransmits the request until it sees the response, so we reply once (gated
/// by `tunnel_created`) and drop the retransmits. RDP_TUNNEL_DATA is forwarded
/// to the owning connection's inbound sink.
///
/// `cookie_registry` (M5a) binds the tunnel to a real TCP session: when present,
/// the CREATEREQUEST's echoed cookie must be one the server issued (and not yet
/// consumed). A forged / replayed / stale cookie is rejected (no response → the
/// client's UDP attempt times out and it stays on TCP). `None` accepts any
/// cookie (the soft pre-M5a behavior, used by the handshake-only test path).
fn handle_emt_tunnel(
    peer_addr: SocketAddr,
    buf: &mut Vec<u8>,
    tunnel_created: &mut bool,
    cookie_registry: Option<&CookieRegistry>,
    inbound: &mut Option<UnboundedSender<Vec<u8>>>,
    bound_flag: &mut Option<Arc<core::sync::atomic::AtomicBool>>,
) -> EmtTunnelOutcome {
    use ironrdp_rdpeudp::emt::{self, TunnelCreateRequest, TunnelCreateResponse};

    let mut outcome = EmtTunnelOutcome::default();

    while let Some(total) = emt::peek_pdu_len(buf) {
        if buf.len() < total {
            break; // wait for the rest of this PDU
        }
        let pdu: Vec<u8> = buf.drain(..total).collect();

        match emt::peek_action(&pdu) {
            Some(emt::action::CREATE_REQUEST) => match TunnelCreateRequest::decode(&pdu) {
                Ok((req, _)) => {
                    if !*tunnel_created {
                        let cookie = req
                            .security_cookie
                            .iter()
                            .map(|b| format!("{b:02x}"))
                            .collect::<String>();
                        // M5a: bind to a real TCP session. `take` is atomic
                        // check-and-consume (one-time use), and returns the
                        // connection's inbound sink so we can forward this
                        // tunnel's client→server channel data (M5c step 3b).
                        let inbound_sink = match cookie_registry {
                            Some(reg) => match reg.take(&req.security_cookie) {
                                Some((sink, flag)) => {
                                    // Keep the bound flag with the peer so
                                    // tunnel-death detection can flip it false.
                                    *bound_flag = Some(flag);
                                    Some(sink)
                                }
                                None => {
                                    warn!(
                                        %peer_addr,
                                        request_id = req.request_id,
                                        %cookie,
                                        "MS-RDPEMT tunnel CREATEREQUEST cookie not recognized — rejecting tunnel (client stays on TCP)"
                                    );
                                    continue;
                                }
                            },
                            None => None, // soft binding (test path): accept, no forwarding
                        };
                        debug!(
                            %peer_addr,
                            request_id = req.request_id,
                            %cookie,
                            "MS-RDPEMT tunnel CREATEREQUEST bound to session — replying CREATERESPONSE(S_OK)"
                        );
                        *tunnel_created = true;
                        *inbound = inbound_sink;
                        outcome.bound_cookie = Some(req.security_cookie);
                        outcome.response = Some(TunnelCreateResponse::ok().to_vec());
                    }
                }
                Err(e) => warn!(%peer_addr, error = %e, "malformed MS-RDPEMT CREATEREQUEST"),
            },
            Some(emt::action::DATA) => {
                // (M5c step 3b) Migrated-channel client→server data (e.g. an EGFX
                // frame ack). Forward the bare DRDYNVC PDU (HigherLayerData) to
                // the owning connection's drdynvc processor.
                match emt::tunnel_data_payload(&pdu) {
                    Ok(higher) => match inbound.as_ref() {
                        Some(sink) => {
                            if sink.send(higher.to_vec()).is_err() {
                                trace!(%peer_addr, "inbound tunnel sink closed; dropping channel data");
                            }
                        }
                        None => debug!(
                            %peer_addr,
                            len = higher.len(),
                            "inbound RDP_TUNNEL_DATA with no inbound sink (soft-bound); dropping"
                        ),
                    },
                    Err(e) => warn!(%peer_addr, error = %e, "malformed inbound RDP_TUNNEL_DATA"),
                }
            }
            Some(other) => {
                debug!(%peer_addr, action = other, len = total, "MS-RDPEMT tunnel PDU: unexpected action, ignoring");
            }
            None => break,
        }
    }

    outcome
}

async fn run_recv_loop(
    socket: Arc<UdpSocket>,
    cfg: ListenerConfig,
    tls_config: Option<Arc<ServerConfig>>,
    cookie_registry: Option<CookieRegistry>,
    mut tunnel_rx: Option<UnboundedReceiver<TunnelOutbound>>,
    dtls_config: Option<DtlsServerContext>,
    // Adaptive-bitrate loss signal: cumulative reliable-tunnel retransmit count the
    // macrdp H.264 controller samples (deltas) to drive congestion-responsive
    // bitrate. `None` = not wired (the feature off / TCP-only build).
    congestion_retransmits: Option<Arc<AtomicU64>>,
) {
    // Accumulate reliable retransmits into the shared loss-signal counter (no-op
    // when not wired). Closure over the Option so the 3 step sites stay one-liners.
    let bump_loss = |n: usize| {
        if n > 0 {
            if let Some(c) = &congestion_retransmits {
                c.fetch_add(n as u64, Ordering::Relaxed);
            }
        }
    };
    let mut peers: HashMap<SocketAddr, Peer> = HashMap::new();
    // (M5c) cookie -> peer address, populated when a tunnel binds, so
    // server-originated `TunnelOutbound` (keyed by cookie) reaches the right peer.
    let mut bound_addrs: HashMap<[u8; 16], SocketAddr> = HashMap::new();
    let mut session_counter: u32 = 0;
    let start = tokio::time::Instant::now();
    let mut buf = vec![0u8; 2048];

    // (P2.2 step 2, EXPERIMENTAL) When set, a lossy (`SYN_LOSSY`) flow drives the
    // SM in `DeliveryMode::Lossy` (deliver-on-arrival, send-once-no-retransmit)
    // instead of the reliable policy. Default OFF — the lossy flow keeps riding the
    // reliable SM (the proven P2.1a/P2.4a path), so this is a one-env-var A/B and a
    // clean fallback. The reliable (`UdpFecR`) flow is never affected.
    let lossy_delivery = super::env_truthy("MACRDP_UDP_LOSSY_DELIVERY");
    if lossy_delivery {
        debug!("MACRDP_UDP_LOSSY_DELIVERY set — lossy (SYN_LOSSY) flows will use DeliveryMode::Lossy");
    }

    // (P2.3 FEC pivot, EXPERIMENTAL) Real FEC is structurally unavailable (modern
    // Windows negotiates RDPUDP2, which has no FEC — see the feasibility doc "P2.3
    // FEC capture RESULT"). This is the protocol-safe stand-in: on a lossy flow,
    // ship each source datagram twice (same seq) so an independent-loss link costs
    // us only at p²; the peer de-dups by sequence number, so the upper layer (audio)
    // never sees the copy. Only meaningful with MACRDP_UDP_LOSSY_DELIVERY (it needs
    // the lossy SM); default OFF.
    let lossy_dup = super::env_truthy("MACRDP_UDP_LOSSY_AUDIO_DUP");
    // Tunnel-death threshold: a BOUND peer with no inbound datagram for this
    // long is declared dead (mstsc's idle keepalive cadence is ~15 s, so the
    // 30 s default = two missed keepalives; must stay < the 60 s idle GC so
    // death is detected before the peer is evicted). 0 disables detection.
    let tunnel_dead_ms: u64 = std::env::var("MACRDP_UDP_TUNNEL_DEAD_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|s| s * 1000)
        .unwrap_or(30_000);
    // Multitransport-offer cooldown after a tunnel death (0 = no suppression).
    let mt_cooldown_secs: u64 = std::env::var("MACRDP_UDP_MT_COOLDOWN_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(600);
    if lossy_dup {
        debug!(
            "MACRDP_UDP_LOSSY_AUDIO_DUP set — lossy flows will duplicate each source datagram (1+1 redundancy)"
        );
    }

    // (Soak fix) Periodic clock for the reliability state machines. The SM only
    // retransmits / drains its send window when it's pumped (step/enqueue), and
    // those are otherwise driven solely by inbound datagrams or new outbound data.
    // On a lossy link that deadlocks: the initial EGFX burst loses segments, no new
    // frames flow (static screen) and the client goes quiet waiting for the missing
    // data, so nothing pumps → the lost segments are never resent → the in-flight
    // window fills → frozen session (observed live at 5% loss). A timer well under
    // the RTO pumps every peer on a `None` tick so retransmits fire on schedule even
    // when both traffic directions are idle. Idle ticks send nothing (pump only
    // emits due retransmits / queued data / an owed ACK), so the cost is negligible.
    let tick_ms = (cfg.rto_ms / 4).clamp(20, 100);
    let mut retransmit_tick = tokio::time::interval(std::time::Duration::from_millis(tick_ms));
    retransmit_tick
        .set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        // (M5c) The recv loop is now bidirectional: it reads inbound datagrams AND
        // ships server-originated channel data (EGFX over the tunnel). When no
        // handoff channel is wired, the outbound arm is a never-ready future, so
        // behavior is identical to the recv-only path.
        let outbound = async {
            match tunnel_rx.as_mut() {
                Some(rx) => rx.recv().await,
                None => std::future::pending().await,
            }
        };

        let (len, peer_addr) = tokio::select! {
            recv = socket.recv_from(&mut buf) => match recv {
                Ok(v) => v,
                // UDP recv_from can surface a prior send_to's ICMP error (e.g. a
                // ConnReset on Windows); it's per-datagram, so keep serving.
                Err(e) => {
                    trace!(error = %e, "udp recv_from error; continuing");
                    continue;
                }
            },
            _ = retransmit_tick.tick() => {
                let now_ms = start.elapsed().as_millis() as u64;
                bump_loss(pump_peers_on_timer(&socket, &mut peers, now_ms, cfg.mtu).await);
                // (M3c) Reap peers whose client has gone away (no inbound for
                // PEER_IDLE_TIMEOUT_MS) so the dead-peer retransmits above stop and
                // peers/bound_addrs don't leak. Same tick — cheap, peers is small.
                check_dead_tunnels(
                    &mut peers,
                    cookie_registry.as_ref(),
                    now_ms,
                    tunnel_dead_ms,
                    mt_cooldown_secs,
                );
                gc_idle_peers(&mut peers, &mut bound_addrs, now_ms);
                continue;
            }
            out = outbound => {
                let now_ms = start.elapsed().as_millis() as u64;
                match out {
                    Some(TunnelOutbound { cookie, data }) => {
                        ship_outbound(&socket, &mut peers, &bound_addrs, cookie, &data, now_ms, cfg.mtu).await;
                    }
                    // Sender dropped: the owning connection went away. Nothing more
                    // will be sent over the tunnel; keep serving inbound.
                    None => tunnel_rx = None,
                }
                continue;
            }
        };
        let now_ms = start.elapsed().as_millis() as u64;
        let data = &buf[..len];

        // Per-datagram receipt trace: confirms a real client's UDP actually
        // reaches the socket (the core M3 question) and shows how we classify
        // it, so a SYN that fails to decode is still visible rather than silent.
        debug!(
            %peer_addr,
            len,
            fec_flags = ?Datagram::peek_fec_flags(data),
            "RDPEUDP datagram received"
        );

        // Log the client's SYN cookie hash + negotiated version (cookie check is
        // soft in M3b — this is the data the future strict-validation work needs).
        if is_syn_family(data) {
            if let Ok(dg) = Datagram::decode(data) {
                if let Some(ex) = dg.syn_ex {
                    // Log the full client cookie hash (hex). Cookie validation is
                    // still soft; pair this with the server's logged issued cookie
                    // ("sent Server Initiate Multitransport Request", cookie=…) from
                    // a live run to derive the hash formula, then tighten.
                    let cookie_hash = ex
                        .cookie_hash
                        .map(|h| h.iter().map(|b| format!("{b:02x}")).collect::<String>())
                        .unwrap_or_else(|| "none".to_owned());
                    debug!(
                        %peer_addr,
                        version = ?ex.udp_version,
                        %cookie_hash,
                        "RDPEUDP SYN received (cookie validation is soft; correlate cookie_hash with the issued cookie)"
                    );
                }
            }
        }

        // (P2.2 step 2) Classify the flow at creation from its opening SYN: a
        // `SYN_LOSSY` SYN is the `UdpFecL` flow. The first datagram from any peer is
        // always its SYN (RDPEUDP opens with one), so peeking here is reliable. Only
        // promotes to lossy delivery when the experimental env is set; otherwise (and
        // for the reliable flow) it stays `Reliable`.
        let use_lossy = lossy_delivery
            && Datagram::peek_fec_flags(data).is_some_and(|f| f.contains(FecFlags::SYN_LOSSY));

        // (M3c) Port reuse on reconnect: if a *new* RDPEUDP flow opens (a SYN) on
        // the source address of an already-ESTABLISHED peer, the previous
        // connection's client reused this addr/port (common on a fast in-process
        // reconnect, within the 10s idle-GC window). The stale peer's SM is
        // established and its tunnel state (`tunnel_created` + an `inbound_sink`
        // pointing at the GONE connection's receiver) would make `handle_emt_tunnel`
        // skip the new tunnel's CREATEREQUEST and silently drop the new connection's
        // inbound EGFX acks → the new session wedges blank/black. Drop the stale peer
        // (and its cookie bindings) so a fresh one is built below and the new tunnel
        // binds cleanly. (A SYN on a still-HANDSHAKING peer is a normal SYN
        // retransmit — `is_established()` gates that out.)
        if is_syn_family(data) && peers.get(&peer_addr).is_some_and(|p| p.sm.is_established()) {
            peers.remove(&peer_addr);
            bound_addrs.retain(|_, a| *a != peer_addr);
            debug!(%peer_addr, "RDPEUDP SYN on an established peer — replacing stale peer (port reuse / reconnect)");
        }

        let peer = peers.entry(peer_addr).or_insert_with(|| {
            session_counter = session_counter.wrapping_add(1);
            let initial_seq = cfg.server_isn_seed.wrapping_add(session_counter);
            let mode = if use_lossy {
                debug!(%peer_addr, "RDPEUDP peer using LOSSY delivery (SYN_LOSSY flow, MACRDP_UDP_LOSSY_DELIVERY)");
                DeliveryMode::Lossy
            } else {
                DeliveryMode::Reliable
            };
            // 1+1 redundancy only applies to the lossy flow (the reliable SM has
            // RTO retransmit instead and never duplicates).
            let duplicate_lossy_sends = use_lossy && lossy_dup;
            if duplicate_lossy_sends {
                debug!(%peer_addr, "RDPEUDP lossy peer will duplicate source datagrams (MACRDP_UDP_LOSSY_AUDIO_DUP)");
            }
            Peer {
                sm: RdpeudpState::new(
                    Role::Server,
                    Config {
                        recv_window: cfg.recv_window,
                        mtu: cfg.mtu,
                        initial_seq,
                        rto_ms: cfg.rto_ms,
                        mode,
                        duplicate_lossy_sends,
                    },
                ),
                inbound: Vec::new(),
                tls_hello_logged: false,
                tls: None,
                tls_done_logged: false,
                emt_inbound: Vec::new(),
                tunnel_created: false,
                inbound_sink: None,
                bound_flag: None,
                dtls_observed: false,
                dtls: None,
                dtls_done_logged: false,
                last_seen_ms: now_ms,
            }
        });
        // (M3c) Any inbound datagram is liveness for the idle-GC clock. A dead client
        // still emits outbound retransmits but receives nothing, so only *inbound*
        // bumps this — its `last_seen_ms` then stops advancing and it ages out.
        peer.last_seen_ms = now_ms;

        // (P2.0 spike) Before anything else, sniff the raw datagram for a DTLS
        // ClientHello. This fires on a *lossy* flow regardless of whether the
        // reliable SM delivers the payload, so it's the most robust go/no-go
        // signal: if this logs, modern mstsc DID open a lossy UDP flow (GREEN).
        if !peer.dtls_observed {
            if let Some(ver) = sniff_dtls_client_hello(data) {
                peer.dtls_observed = true;
                let dtls = match ver {
                    [0xFE, 0xFF] => "DTLS 1.0",
                    [0xFE, 0xFD] => "DTLS 1.2",
                    _ => "DTLS",
                };
                debug!(
                    %peer_addr, %dtls,
                    "P2.0 SPIKE GREEN: client opened a LOSSY (UdpFecL) UDP flow — \
                     it sent a {dtls} ClientHello. Lossy multitransport is reachable; \
                     observe-only (no DTLS implemented). See feasibility doc P2.0."
                );
            }
        }

        let was_established = peer.sm.is_established();
        let had_data = Datagram::peek_fec_flags(data).is_some_and(|f| f.contains(FecFlags::DATA));
        let out = peer.sm.step(now_ms, Some(data));
        let (retransmits, syn_retransmit) = (out.retransmits, out.syn_retransmit);
        bump_loss(retransmits);
        send_datagrams(&socket, peer_addr, out.to_send, cfg.mtu).await;
        if retransmits > 0 || syn_retransmit {
            // The reliable transport recovered lost packets — the signal a
            // lossy-link soak watches. Quiet (count is 0) on a clean link.
            debug!(%peer_addr, retransmits, syn_retransmit, "RDPEUDP RTO retransmit");
        }

        if !was_established && peer.sm.is_established() {
            debug!(
                %peer_addr,
                version = ?peer.sm.negotiated_version(),
                "RDPEUDP handshake established"
            );
        }

        // M4a: accumulate the reassembled reliable byte-stream the SM delivered.
        // This is the foundation the TLS server (M4b) + EMT tunnel (M4c) consume.
        let delivered: usize = out.delivered.iter().map(Vec::len).sum();
        if delivered > 0 {
            for chunk in &out.delivered {
                peer.inbound.extend_from_slice(chunk);
            }
            debug!(
                %peer_addr,
                delivered,
                total = peer.inbound.len(),
                "RDPEUDP reliable data delivered"
            );
            // Sniff for a TLS ClientHello (handshake record 0x16, version 0x03xx)
            // — confirms the client is starting the MS-RDPEMT-over-TLS handshake
            // and that our v1 receive path reassembled mstsc's V2 stream correctly.
            if !peer.tls_hello_logged
                && peer.inbound.len() >= 3
                && peer.inbound[0] == 0x16
                && peer.inbound[1] == 0x03
            {
                peer.tls_hello_logged = true;
                debug!(
                    %peer_addr,
                    "RDPEUDP reliable stream carries a TLS ClientHello (MS-RDPEMT handshake starting)"
                );
            }

            // (P2.1a) LOSSY flow: drive the DTLS 1.2 server handshake. The DTLS
            // records ride this flow's RDPEUDP payloads, so we feed each delivered
            // chunk (one datagram — the boundary rule in `dtls.rs`) into boring
            // and ship the resulting datagram(s) back over RDPEUDP. Reaching
            // "established" proves MS-RDPEMT-over-DTLS is reachable (P2.1a scope;
            // the EMT tunnel + lossy audio DVC over it are P2.4+). Mutually
            // exclusive with the rustls block below (that one is gated on
            // `!dtls_observed`).
            if peer.dtls_observed {
                if let Some(dtls_ctx) = dtls_config.as_ref() {
                    if peer.dtls.is_none() {
                        match dtls_ctx.new_conn() {
                            Ok(c) => peer.dtls = Some(c),
                            Err(e) => warn!(%peer_addr, error = %e, "failed to create DTLS server conn"),
                        }
                    }
                    let Peer {
                        sm,
                        dtls: dtls_opt,
                        dtls_done_logged,
                        emt_inbound,
                        tunnel_created,
                        inbound_sink,
                        bound_flag,
                        ..
                    } = peer;
                    if let Some(conn) = dtls_opt.as_mut() {
                        let mut dtls_err = false;
                        for chunk in &out.delivered {
                            if conn.is_handshake_done() {
                                // (P2.4) Post-handshake: decrypt the DTLS app record
                                // and accumulate the MS-RDPEMT plaintext for parsing.
                                match conn.recv(chunk) {
                                    Ok(Some(pt)) => emt_inbound.extend_from_slice(&pt),
                                    Ok(None) => {}
                                    Err(e) => {
                                        warn!(%peer_addr, error = %e, "DTLS app-record decrypt error on lossy flow");
                                        dtls_err = true;
                                        break;
                                    }
                                }
                            } else {
                                // Handshake flights: feed one datagram, ship the
                                // resulting datagram(s) (DTLS MTU < RDPEUDP MTU so
                                // each is never split across two).
                                match conn.read_datagram(chunk) {
                                    Ok(outs) => {
                                        for dg in outs {
                                            let o = sm.enqueue(now_ms, &dg);
                                            send_datagrams(&socket, peer_addr, o.to_send, cfg.mtu).await;
                                        }
                                    }
                                    Err(e) => {
                                        warn!(%peer_addr, error = %e, "DTLS handshake error on lossy flow");
                                        dtls_err = true;
                                        break;
                                    }
                                }
                            }
                        }
                        if !dtls_err && conn.is_handshake_done() && !*dtls_done_logged {
                            *dtls_done_logged = true;
                            debug!(
                                %peer_addr,
                                "P2.1 GREEN: DTLS 1.2 handshake COMPLETE on the LOSSY (UdpFecL) flow — MS-RDPEMT-over-DTLS reachable"
                            );
                        }
                        // (P2.4a) Once DTLS is established, parse any accumulated
                        // MS-RDPEMT PDUs and answer the tunnel CREATEREQUEST —
                        // encrypted back through DTLS. handle_emt_tunnel gates the
                        // response on !tunnel_created, so it's sent exactly once.
                        if !dtls_err && conn.is_handshake_done() && !emt_inbound.is_empty() {
                            let outcome = handle_emt_tunnel(
                                peer_addr,
                                emt_inbound,
                                tunnel_created,
                                cookie_registry.as_ref(),
                                inbound_sink,
                                bound_flag,
                            );
                            if let Some(cookie) = outcome.bound_cookie {
                                bound_addrs.insert(cookie, peer_addr);
                            }
                            if let Some(resp) = outcome.response {
                                match conn.send(&resp) {
                                    Ok(dgs) => {
                                        for dg in dgs {
                                            let o = sm.enqueue(now_ms, &dg);
                                            send_datagrams(&socket, peer_addr, o.to_send, cfg.mtu).await;
                                        }
                                        debug!(
                                            %peer_addr,
                                            "P2.4 GREEN: MS-RDPEMT tunnel ESTABLISHED over DTLS (lossy flow) — CREATERESPONSE(S_OK) sent"
                                        );
                                    }
                                    Err(e) => warn!(
                                        %peer_addr, error = %e,
                                        "failed to encrypt MS-RDPEMT CREATERESPONSE through DTLS"
                                    ),
                                }
                            }
                        }
                    }
                }
            }

            // M4b/M4c: drive the MS-RDPEMT server-side TLS handshake over the
            // reliable stream, then handle the tunnel PDUs inside it. Feed the
            // just-delivered bytes into rustls, parse any decrypted tunnel PDUs
            // (M4c: answer CREATEREQUEST with CREATERESPONSE), and ship whatever
            // TLS output that produces back through the SM (reliable, fragmented
            // to the MTU). No-op when the listener has no TLS config (test path).
            // (P2.0 spike) Skipped for a peer on a lossy flow (DTLS observed):
            // DTLS records aren't TLS records, so feeding them to rustls is pure
            // error spam — the spike only observes that the lossy flow exists.
            if let Some(tls_cfg) = tls_config.as_ref().filter(|_| !peer.dtls_observed) {
                if peer.tls.is_none() {
                    match ServerConnection::new(Arc::clone(tls_cfg)) {
                        Ok(conn) => peer.tls = Some(conn),
                        Err(e) => warn!(%peer_addr, error = %e, "failed to create MS-RDPEMT TLS server"),
                    }
                }
                // Disjoint field borrows so the TLS feed/write can run alongside
                // the EMT tunnel buffer + state.
                let Peer {
                    sm,
                    tls: tls_opt,
                    tls_done_logged,
                    emt_inbound,
                    tunnel_created,
                    inbound_sink,
                    bound_flag,
                    ..
                } = peer;

                let mut tls_out = Vec::new();
                let mut handshake_done = false;
                if let Some(tls) = tls_opt.as_mut() {
                    let mut tls_err = false;
                    for chunk in &out.delivered {
                        // Feed the reliable byte-stream into rustls. `read_tls`
                        // returns Ok(0) once the cursor is drained (or its internal
                        // buffer is full); loop until then so a chunk carrying more
                        // than one TLS record is fully consumed.
                        let mut rd = io::Cursor::new(chunk.as_slice());
                        loop {
                            match tls.read_tls(&mut rd) {
                                Ok(0) => break,
                                Ok(_) => {}
                                Err(_) => {
                                    tls_err = true;
                                    break;
                                }
                            }
                            if let Err(e) = tls.process_new_packets() {
                                warn!(%peer_addr, error = %e, "MS-RDPEMT TLS error");
                                tls_err = true;
                                break;
                            }
                            // Drain decrypted application data (the MS-RDPEMT tunnel
                            // PDUs) so rustls's plaintext buffer can't fill and
                            // stall record processing; accumulate it for parsing.
                            // The trailing WouldBlock once the buffer empties is
                            // expected.
                            let _ = tls.reader().read_to_end(emt_inbound);
                        }
                        if tls_err {
                            break;
                        }
                    }

                    // M4c: answer the client's RDP_TUNNEL_CREATEREQUEST. Write the
                    // response into the TLS connection so the wants_write drain
                    // below picks up its encrypted bytes. M5a: bind the tunnel to a
                    // real TCP session via the cookie registry.
                    let outcome = handle_emt_tunnel(
                        peer_addr,
                        emt_inbound,
                        tunnel_created,
                        cookie_registry.as_ref(),
                        inbound_sink,
                        bound_flag,
                    );
                    if let Some(cookie) = outcome.bound_cookie {
                        // (M5c) Record cookie -> this peer so server-originated
                        // tunnel data (keyed by the same cookie) reaches it.
                        bound_addrs.insert(cookie, peer_addr);
                    }
                    if let Some(resp) = outcome.response {
                        if tls.writer().write_all(&resp).is_err() {
                            warn!(%peer_addr, "failed to write MS-RDPEMT CREATERESPONSE into TLS");
                        }
                    }

                    while tls.wants_write() {
                        if tls.write_tls(&mut tls_out).is_err() {
                            break;
                        }
                    }
                    handshake_done = !tls_err && !tls.is_handshaking();
                }
                if !tls_out.is_empty() {
                    let o = sm.enqueue(now_ms, &tls_out);
                    let (retransmits, syn_retransmit) = (o.retransmits, o.syn_retransmit);
                    bump_loss(retransmits);
                    send_datagrams(&socket, peer_addr, o.to_send, cfg.mtu).await;
                    if retransmits > 0 || syn_retransmit {
                        debug!(%peer_addr, retransmits, syn_retransmit, "RDPEUDP RTO retransmit (outbound)");
                    }
                }
                if handshake_done && !*tls_done_logged {
                    *tls_done_logged = true;
                    debug!(%peer_addr, "MS-RDPEMT TLS handshake complete");
                }
            }
        } else if had_data {
            // A DATA datagram that produced no delivery means our receive sequence
            // didn't line up with the client's data seq — the client will keep
            // retransmitting (CWR). Log the client's source seq vs the expected
            // `recv_next` so the convention can be pinned against real mstsc (a
            // gap of exactly 1 is the SYN-consumes-a-seq off-by-one; a wild value
            // means the source-payload decode landed at the wrong offset instead).
            let client_seq = Datagram::decode(data)
                .ok()
                .and_then(|dg| dg.source.map(|s| s.header.sn_source_start));
            debug!(
                %peer_addr,
                ?client_seq,
                expected = peer.sm.recv_next(),
                "RDPEUDP DATA datagram delivered nothing (receive-sequence mismatch)"
            );
        }
    }
}
