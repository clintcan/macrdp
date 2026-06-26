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
//! - Peers are demuxed by source address and never garbage-collected (one client
//!   in the M3b path); GC + idle timeout come with M3c.
//!
//! Cross-platform (pure tokio/std networking), so Linux CI exercises it too — see
//! the loopback integration test in the macrdp crate (the vendored server is
//! built `test = false`).

use std::collections::HashMap;
use std::io;
use std::io::Read as _;
use std::io::Write as _;
use std::net::SocketAddr;
use std::sync::Arc;

use ironrdp_rdpeudp::datagram::Datagram;
use ironrdp_rdpeudp::pdu::FecFlags;
use ironrdp_rdpeudp::state::{Config, RdpeudpState, Role};

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
    /// peer's bytes into rustls (DTLS records aren't TLS records and would only
    /// produce error spam): the spike is observe-only, no DTLS is implemented.
    dtls_observed: bool,
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
    pub async fn bind(
        addr: SocketAddr,
        cfg: ListenerConfig,
        tls_config: Option<Arc<ServerConfig>>,
        cookie_registry: Option<CookieRegistry>,
        tunnel_rx: Option<UnboundedReceiver<TunnelOutbound>>,
    ) -> io::Result<Self> {
        let socket = UdpSocket::bind(addr).await?;
        let local_addr = socket.local_addr()?;
        let socket = Arc::new(socket);
        debug!(
            %local_addr, ?cfg,
            tls = tls_config.is_some(),
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
) {
    for (addr, peer) in peers.iter_mut() {
        if !peer.sm.is_established() {
            continue;
        }
        let out = peer.sm.step(now_ms, None);
        let (retransmits, syn_retransmit) = (out.retransmits, out.syn_retransmit);
        if !out.to_send.is_empty() {
            send_datagrams(socket, *addr, out.to_send, mtu).await;
        }
        if retransmits > 0 || syn_retransmit {
            debug!(peer_addr = %addr, retransmits, syn_retransmit, "RDPEUDP RTO retransmit (timer)");
        }
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
    let Peer { sm, tls, .. } = peer;
    let Some(tls) = tls.as_mut() else {
        warn!(%peer_addr, "tunnel data to a peer with no TLS; dropping");
        return;
    };

    let pdu = emt::encode_tunnel_data(higher_layer);
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
}

/// Parse complete MS-RDPEMT tunnel PDUs from the TLS-decrypted plaintext and, on
/// the client's `RDP_TUNNEL_CREATEREQUEST`, write a `CREATERESPONSE(S_OK)` into
/// the TLS connection (its encrypted bytes are picked up by the caller's
/// `write_tls` drain). Consumes parsed PDUs from the front of `buf`; leaves a
/// partial trailing PDU buffered for the next call. The client retransmits the
/// request until it sees the response, so we reply once (gated by
/// `tunnel_created`) and drop the retransmits. RDP_TUNNEL_DATA (channel
/// migration) is M5 — logged and skipped for now.
///
/// `cookie_registry` (M5a) binds the tunnel to a real TCP session: when present,
/// the CREATEREQUEST's echoed cookie must be one the server issued (and not yet
/// consumed). A forged / replayed / stale cookie is rejected (no response → the
/// client's UDP attempt times out and it stays on TCP). `None` accepts any
/// cookie (the soft pre-M5a behavior, used by the handshake-only test path).
fn handle_emt_tunnel(
    peer_addr: SocketAddr,
    tls: &mut ServerConnection,
    buf: &mut Vec<u8>,
    tunnel_created: &mut bool,
    cookie_registry: Option<&CookieRegistry>,
    inbound: &mut Option<UnboundedSender<Vec<u8>>>,
) -> Option<[u8; 16]> {
    use ironrdp_rdpeudp::emt::{self, TunnelCreateRequest, TunnelCreateResponse};

    // The cookie of a tunnel bound during this call, so the caller can map it to
    // this peer address for the server→listener data handoff (M5c).
    let mut bound_cookie: Option<[u8; 16]> = None;

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
                                Some(sink) => Some(sink),
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
                        let resp = TunnelCreateResponse::ok().to_vec();
                        if tls.writer().write_all(&resp).is_ok() {
                            *tunnel_created = true;
                            *inbound = inbound_sink;
                            bound_cookie = Some(req.security_cookie);
                        } else {
                            warn!(%peer_addr, "failed to write MS-RDPEMT CREATERESPONSE into TLS");
                        }
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

    bound_cookie
}

async fn run_recv_loop(
    socket: Arc<UdpSocket>,
    cfg: ListenerConfig,
    tls_config: Option<Arc<ServerConfig>>,
    cookie_registry: Option<CookieRegistry>,
    mut tunnel_rx: Option<UnboundedReceiver<TunnelOutbound>>,
) {
    let mut peers: HashMap<SocketAddr, Peer> = HashMap::new();
    // (M5c) cookie -> peer address, populated when a tunnel binds, so
    // server-originated `TunnelOutbound` (keyed by cookie) reaches the right peer.
    let mut bound_addrs: HashMap<[u8; 16], SocketAddr> = HashMap::new();
    let mut session_counter: u32 = 0;
    let start = tokio::time::Instant::now();
    let mut buf = vec![0u8; 2048];

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
                pump_peers_on_timer(&socket, &mut peers, now_ms, cfg.mtu).await;
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

        let peer = peers.entry(peer_addr).or_insert_with(|| {
            session_counter = session_counter.wrapping_add(1);
            let initial_seq = cfg.server_isn_seed.wrapping_add(session_counter);
            Peer {
                sm: RdpeudpState::new(
                    Role::Server,
                    Config {
                        recv_window: cfg.recv_window,
                        mtu: cfg.mtu,
                        initial_seq,
                        rto_ms: cfg.rto_ms,
                    },
                ),
                inbound: Vec::new(),
                tls_hello_logged: false,
                tls: None,
                tls_done_logged: false,
                emt_inbound: Vec::new(),
                tunnel_created: false,
                inbound_sink: None,
                dtls_observed: false,
            }
        });

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
                warn!(
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

                    // M4c: answer the client's RDP_TUNNEL_CREATEREQUEST. Writing the
                    // response into the TLS connection here means the wants_write
                    // drain below picks up its encrypted bytes. M5a: bind the
                    // tunnel to a real TCP session via the cookie registry.
                    if let Some(cookie) = handle_emt_tunnel(
                        peer_addr,
                        tls,
                        emt_inbound,
                        tunnel_created,
                        cookie_registry.as_ref(),
                        inbound_sink,
                    ) {
                        // (M5c) Record cookie -> this peer so server-originated
                        // tunnel data (keyed by the same cookie) reaches it.
                        bound_addrs.insert(cookie, peer_addr);
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
