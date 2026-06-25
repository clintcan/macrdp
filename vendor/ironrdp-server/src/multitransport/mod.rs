//! (vendored) Server-side RDP UDP multitransport (MS-RDPEMT) support.
//!
//! Gated behind the `multitransport` cargo feature (default off) so the
//! standard build is byte-identical. This is the macrdp UDP-multitransport
//! effort; see `docs/rdp-udp-multitransport-feasibility.md` and the
//! `vendor/ironrdp-server/CLAUDE.md` divergence (12) for the full plan.
//!
//! # Milestone status
//!
//! **M1 (this code): negotiation only.** When a [`MultitransportProvider`] is
//! installed AND the client advertised UDP support in its GCC
//! `MultiTransportChannelData` block (surfaced by the vendored acceptor on
//! `AcceptorResult::multitransport_flags`), the server sends a
//! `MultitransportRequestPdu` on the IO channel after licensing and matches the
//! client's `MultitransportResponsePdu`. There is **no UDP listener yet**, so
//! the client's out-of-band UDP attempt times out and it reports `E_ABORT`; the
//! session continues on TCP unchanged. This proves the negotiation/framing
//! contract and the graceful-fallback path before any socket code exists.
//!
//! Later milestones grow this module with `listener`/`session`/`router`/
//! `migration` submodules (the UDP transport + channel migration); the trait
//! will gain methods accordingly.

pub mod listener;

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use ironrdp_acceptor::MultitransportOffer;
use ironrdp_core::encode_vec;
use ironrdp_pdu::mcs::SendDataIndication;
use ironrdp_pdu::rdp::headers::{BasicSecurityHeader, BasicSecurityHeaderFlags};
use ironrdp_pdu::rdp::multitransport::{MultitransportRequestPdu, RequestedProtocol};
use ironrdp_pdu::x224::X224;

/// Server-side hook for RDP UDP multitransport. A provider, when installed via
/// [`RdpServer::set_multitransport_provider`](crate::RdpServer::set_multitransport_provider),
/// makes the server *offer* an auxiliary UDP transport to clients that
/// advertise support. The provider expresses **what** to offer; the server owns
/// the negotiation handshake and (in later milestones) the transport itself.
pub trait MultitransportProvider: Send {
    /// Which UDP transport protocol to request from the client.
    ///
    /// M1 implementations return [`RequestedProtocol::UdpFecR`] (reliable —
    /// RDPEUDP2 + TLS, no DTLS). Lossy (`UdpFecL`) is a later milestone.
    fn requested_protocol(&self) -> RequestedProtocol;
}

/// Encode a Server Initiate Multitransport Request (MS-RDPBCGR 2.2.15.1) as a
/// `SendDataIndication` on the IO channel. The Initiate Request is a
/// `BasicSecurityHeader`-wrapped PDU, **not** a ShareControl PDU — so this
/// mirrors `server::encode_share_data_pdu` minus the ShareControl/ShareData
/// wrapping. Pure + exported so the framing can be round-trip tested (the
/// vendored crate itself is built with `test = false`, so the test lives in the
/// macrdp crate).
pub fn encode_initiate_request(
    request_id: u32,
    protocol: RequestedProtocol,
    security_cookie: [u8; 16],
    io_channel_id: u16,
    user_channel_id: u16,
) -> Result<Vec<u8>> {
    let pdu = MultitransportRequestPdu {
        security_header: BasicSecurityHeader {
            flags: BasicSecurityHeaderFlags::TRANSPORT_REQ,
        },
        request_id,
        requested_protocol: protocol,
        security_cookie,
    };
    let user_data = encode_vec(&pdu)?.into();
    let mcs_pdu = SendDataIndication {
        initiator_id: user_channel_id,
        channel_id: io_channel_id,
        user_data,
    };
    Ok(encode_vec(&X224(mcs_pdu))?)
}

/// A shared set of multitransport security cookies the server has issued and not
/// yet consumed/torn down. The per-connection offer path
/// ([`RdpServer`](crate::RdpServer)) registers the cookie it puts in its
/// Initiate Multitransport Request; the process-global UDP
/// [`listener`](crate::multitransport::listener) checks an inbound tunnel
/// `RDP_TUNNEL_CREATEREQUEST`'s echoed cookie against it before accepting the
/// tunnel — **binding the UDP flow to a real, current TCP session** so a forged
/// or replayed cookie can't open a tunnel. Cookies are one-time: the listener
/// removes a cookie when it accepts the tunnel, and the offer path evicts the
/// previous connection's (unconsumed) cookie before registering a new one.
/// Per-cookie registry entry: the tunnel-bound flag plus the owning connection's
/// inbound-tunnel-data sink.
struct CookieEntry {
    /// Flipped `true` when the listener binds the matching tunnel; the
    /// offer-issuing [`RdpServer`](crate::RdpServer) reads it to fire Soft-Sync.
    bound: Arc<AtomicBool>,
    /// (M5c step 3b) The owning connection's inbound-tunnel-data sink. The
    /// listener, on a successful bind, takes this sender and forwards each
    /// inbound `RDP_TUNNEL_DATA` HigherLayerData (a bare DRDYNVC PDU — e.g. an
    /// EGFX frame acknowledgement) to it, so the connection's drdynvc processor
    /// sees the migrated channel's client→server traffic.
    inbound: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
}

#[derive(Clone, Default)]
pub struct CookieRegistry(Arc<Mutex<HashMap<[u8; 16], CookieEntry>>>);

impl CookieRegistry {
    /// A fresh, empty registry. Create one in `main` and share it (clone) with
    /// both the [`RdpServer`](crate::RdpServer) and the UDP listener.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an issued cookie as valid, alongside the connection's inbound
    /// sink (M5c step 3b — where the listener forwards migrated-channel client
    /// data). Returns the **tunnel-bound flag**: the listener sets it `true`
    /// when it binds the matching tunnel, and the offer-issuing
    /// [`RdpServer`](crate::RdpServer) reads it to know the UDP multitransport
    /// connection is up (the cue to send the Soft-Sync request).
    pub fn register(&self, cookie: [u8; 16], inbound: tokio::sync::mpsc::UnboundedSender<Vec<u8>>) -> Arc<AtomicBool> {
        let bound = Arc::new(AtomicBool::new(false));
        if let Ok(mut map) = self.0.lock() {
            map.insert(
                cookie,
                CookieEntry {
                    bound: Arc::clone(&bound),
                    inbound,
                },
            );
        }
        bound
    }

    /// Drop a cookie (evicted on teardown / TCP fallback).
    pub fn remove(&self, cookie: &[u8; 16]) {
        if let Ok(mut map) = self.0.lock() {
            map.remove(cookie);
        }
    }

    /// Atomically check-and-consume a cookie: on a match, removes it, **sets its
    /// tunnel-bound flag**, and returns the connection's inbound sink (so the
    /// listener can forward this tunnel's client→server channel data). Returns
    /// `None` for an unknown cookie. One-time use — a retransmitted/replayed
    /// CREATEREQUEST with the same cookie won't bind a second tunnel.
    pub fn take(&self, cookie: &[u8; 16]) -> Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>> {
        let removed = match self.0.lock() {
            Ok(mut map) => map.remove(cookie),
            Err(_) => None,
        };
        let entry = removed?;
        entry.bound.store(true, Ordering::Relaxed);
        Some(entry.inbound)
    }
}

/// Build a fresh [`MultitransportOffer`] for one connection: a process-wide
/// monotonic `request_id` plus a **cryptographically-random** 16-byte security
/// cookie. The acceptor sends it as the Server Initiate Multitransport Request
/// after licensing (before Demand Active); the client echoes `request_id` +
/// `cookie` back inside the UDP tunnel's `RDP_TUNNEL_CREATEREQUEST`, where the
/// listener matches it against the [`CookieRegistry`] to bind the flow. The
/// cookie is CSPRNG-generated (not derivable from `request_id`) so it can't be
/// forged by an attacker who can see the predictable request id.
pub(crate) fn new_offer(protocol: RequestedProtocol) -> MultitransportOffer {
    static MT_REQUEST_ID: AtomicU32 = AtomicU32::new(1);
    let request_id = MT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    let mut cookie = [0u8; 16];
    if let Err(e) = getrandom::getrandom(&mut cookie) {
        // The system RNG failing is catastrophic and near-impossible; fall back
        // to a non-secret derived value so we don't panic the whole server. The
        // tunnel binding still works (registry match); only unpredictability is
        // lost in this degenerate case.
        tracing::error!(error = %e, "system RNG failed for multitransport cookie; using a weak fallback");
        for (i, b) in cookie.iter_mut().enumerate() {
            *b = (request_id.wrapping_mul(2_654_435_761).wrapping_add(i as u32) & 0xff) as u8;
        }
    }
    MultitransportOffer {
        request_id,
        protocol,
        cookie,
    }
}

/// (M5c) One unit of server-originated data to ship over a bound UDP tunnel: the
/// `cookie` selects which peer (the listener maps cookie → peer address on bind),
/// and `data` is the **HigherLayerData** for an `RDP_TUNNEL_DATA` PDU (one SVC
/// channel-data chunk — `CHANNEL_PDU_HEADER` + the DRDYNVC PDU — the same bytes
/// that would otherwise ride the drdynvc static channel over TCP). The listener
/// wraps it in `RDP_TUNNEL_DATA`, encrypts via the peer's MS-RDPEMT TLS, and sends
/// it reliably over RDPEUDP.
#[derive(Debug)]
pub struct TunnelOutbound {
    pub cookie: [u8; 16],
    pub data: Vec<u8>,
}

/// (M5c) Clonable handle the per-connection [`RdpServer`](crate::RdpServer) uses to
/// push channel data onto the process-global UDP listener's bound tunnel (the
/// server→listener handoff). The listener owns the receiving end. Best-effort: a
/// send failure (listener gone / channel full-less unbounded) is dropped — the
/// channel is for the optional UDP fast-path, never the correctness-critical TCP
/// path.
#[derive(Clone)]
pub struct TunnelSender(tokio::sync::mpsc::UnboundedSender<TunnelOutbound>);

impl TunnelSender {
    /// Queue one HigherLayerData chunk for the peer bound to `cookie`.
    pub(crate) fn send(&self, cookie: [u8; 16], data: Vec<u8>) {
        let _ = self.0.send(TunnelOutbound { cookie, data });
    }
}

/// (M5c) Create the server→listener handoff channel. Hand the [`TunnelSender`] to
/// the [`RdpServer`](crate::RdpServer) (via `set_multitransport_tunnel_sender`) and
/// the receiver to [`UdpMultitransportListener::bind`](crate::multitransport::listener::UdpMultitransportListener::bind).
pub fn tunnel_channel() -> (TunnelSender, tokio::sync::mpsc::UnboundedReceiver<TunnelOutbound>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    (TunnelSender(tx), rx)
}

/// Per-connection negotiation state for an in-flight multitransport request:
/// the `request_id` + 16-byte security cookie the server issued in the
/// `MultitransportRequestPdu`, used to match the client's
/// `MultitransportResponsePdu` (and, in later milestones, to bind the inbound
/// UDP flow to this session).
#[derive(Debug, Clone, Copy)]
pub(crate) struct MigrationState {
    pub request_id: u32,
    /// Issued in the request and (M1) not read again — later milestones bind the
    /// inbound UDP flow to this session by matching the echoed cookie.
    #[allow(dead_code)]
    pub cookie: [u8; 16],
    pub protocol: RequestedProtocol,
    /// (M5c) Set once we've sent the `DYNVC_SOFT_SYNC_REQUEST` for this
    /// connection, so a retransmitted Initiate Multitransport Response (or any
    /// other message-channel PDU) doesn't make us re-send it.
    pub soft_sync_sent: bool,
}
