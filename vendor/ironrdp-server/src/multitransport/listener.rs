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

use crate::multitransport::CookieRegistry;
use tokio::net::UdpSocket;
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
    ) -> io::Result<Self> {
        let socket = UdpSocket::bind(addr).await?;
        let local_addr = socket.local_addr()?;
        let socket = Arc::new(socket);
        debug!(
            %local_addr, ?cfg,
            tls = tls_config.is_some(),
            cookie_binding = cookie_registry.is_some(),
            "RDPEUDP multitransport listener bound"
        );
        let task = tokio::spawn(run_recv_loop(
            Arc::clone(&socket),
            cfg,
            tls_config,
            cookie_registry,
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
) {
    use ironrdp_rdpeudp::emt::{self, TunnelCreateRequest, TunnelCreateResponse};

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
                        // check-and-consume, so a cookie is one-time use.
                        let bound = match cookie_registry {
                            Some(reg) => reg.take(&req.security_cookie),
                            None => true, // soft binding (test path)
                        };
                        if !bound {
                            warn!(
                                %peer_addr,
                                request_id = req.request_id,
                                %cookie,
                                "MS-RDPEMT tunnel CREATEREQUEST cookie not recognized — rejecting tunnel (client stays on TCP)"
                            );
                            continue;
                        }
                        debug!(
                            %peer_addr,
                            request_id = req.request_id,
                            %cookie,
                            "MS-RDPEMT tunnel CREATEREQUEST bound to session — replying CREATERESPONSE(S_OK)"
                        );
                        let resp = TunnelCreateResponse::ok().to_vec();
                        if tls.writer().write_all(&resp).is_ok() {
                            *tunnel_created = true;
                        } else {
                            warn!(%peer_addr, "failed to write MS-RDPEMT CREATERESPONSE into TLS");
                        }
                    }
                }
                Err(e) => warn!(%peer_addr, error = %e, "malformed MS-RDPEMT CREATEREQUEST"),
            },
            Some(other) => {
                // RDP_TUNNEL_DATA (action 0x2) and friends carry migrated channel
                // data — M5. Skip until then.
                debug!(
                    %peer_addr,
                    action = other,
                    len = total,
                    "MS-RDPEMT tunnel PDU not handled yet (channel migration is M5)"
                );
            }
            None => break,
        }
    }
}

async fn run_recv_loop(
    socket: Arc<UdpSocket>,
    cfg: ListenerConfig,
    tls_config: Option<Arc<ServerConfig>>,
    cookie_registry: Option<CookieRegistry>,
) {
    let mut peers: HashMap<SocketAddr, Peer> = HashMap::new();
    let mut session_counter: u32 = 0;
    let start = tokio::time::Instant::now();
    let mut buf = vec![0u8; 2048];

    loop {
        let (len, peer_addr) = match socket.recv_from(&mut buf).await {
            Ok(v) => v,
            // UDP recv_from can surface a prior send_to's ICMP error (e.g. a
            // ConnReset on Windows); it's per-datagram, so keep serving.
            Err(e) => {
                trace!(error = %e, "udp recv_from error; continuing");
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
            }
        });

        let was_established = peer.sm.is_established();
        let had_data = Datagram::peek_fec_flags(data).is_some_and(|f| f.contains(FecFlags::DATA));
        let out = peer.sm.step(now_ms, Some(data));
        send_datagrams(&socket, peer_addr, out.to_send, cfg.mtu).await;

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
            if let Some(tls_cfg) = tls_config.as_ref() {
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
                    handle_emt_tunnel(
                        peer_addr,
                        tls,
                        emt_inbound,
                        tunnel_created,
                        cookie_registry.as_ref(),
                    );

                    while tls.wants_write() {
                        if tls.write_tls(&mut tls_out).is_err() {
                            break;
                        }
                    }
                    handshake_done = !tls_err && !tls.is_handshaking();
                }
                if !tls_out.is_empty() {
                    let o = sm.enqueue(now_ms, &tls_out);
                    send_datagrams(&socket, peer_addr, o.to_send, cfg.mtu).await;
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
