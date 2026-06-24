//! macrdp-side RDP UDP multitransport (MS-RDPEMT) provider.
//!
//! **M1: negotiation only.** This is a thin provider that tells the vendored
//! server to *offer* reliable UDP (`UdpFecR` — RDPEUDP2 over the session's
//! existing TLS) to clients that advertise support. There is no UDP listener
//! yet, so the server's M1 path only performs the Initiate Request → Response
//! handshake and falls back to TCP; see
//! `vendor/ironrdp-server/src/multitransport/` and
//! `docs/rdp-udp-multitransport-feasibility.md`. Wired in `main.rs` behind
//! `--enable-udp-multitransport`.
//!
//! Cross-platform on purpose (it's pure protocol policy, no macOS APIs), so the
//! Linux CI build that compiles the protocol layer exercises it too. Later
//! milestones add the UDP listener/session here (which will be macOS-gated for
//! the parts that touch capture/encode).

use ironrdp_pdu::rdp::multitransport::RequestedProtocol;
use ironrdp_server::MultitransportProvider;

/// M1 provider: offer reliable UDP multitransport. Reliable (`UdpFecR`) rides
/// the connection's existing rustls TLS, so no DTLS / new crypto dependency is
/// needed. Lossy (`UdpFecL`, which needs DTLS) is a later milestone.
pub struct MacMultitransport;

impl MultitransportProvider for MacMultitransport {
    fn requested_protocol(&self) -> RequestedProtocol {
        RequestedProtocol::UdpFecR
    }
}
