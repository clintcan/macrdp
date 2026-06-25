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
use std::net::SocketAddr;
use std::sync::Arc;

use ironrdp_rdpeudp::datagram::Datagram;
use ironrdp_rdpeudp::pdu::FecFlags;
use ironrdp_rdpeudp::state::{Config, RdpeudpState, Role};
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;
use tracing::{debug, trace};

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

/// A per-peer handshake/transport session: just the reliability SM for now.
struct Peer {
    sm: RdpeudpState,
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
    pub async fn bind(addr: SocketAddr, cfg: ListenerConfig) -> io::Result<Self> {
        let socket = UdpSocket::bind(addr).await?;
        let local_addr = socket.local_addr()?;
        let socket = Arc::new(socket);
        debug!(%local_addr, ?cfg, "RDPEUDP multitransport listener bound");
        let task = tokio::spawn(run_recv_loop(Arc::clone(&socket), cfg));
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

async fn run_recv_loop(socket: Arc<UdpSocket>, cfg: ListenerConfig) {
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
            }
        });

        let was_established = peer.sm.is_established();
        let out = peer.sm.step(now_ms, Some(data));
        for mut dg in out.to_send {
            if is_syn_family(&dg) && dg.len() < cfg.mtu as usize {
                dg.resize(cfg.mtu as usize, 0); // path-MTU validation padding
            }
            if let Err(e) = socket.send_to(&dg, peer_addr).await {
                trace!(error = %e, %peer_addr, "udp send_to error");
            }
        }

        if !was_established && peer.sm.is_established() {
            debug!(
                %peer_addr,
                version = ?peer.sm.negotiated_version(),
                "RDPEUDP handshake established"
            );
        }
    }
}
