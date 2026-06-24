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

use anyhow::Result;
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
}
