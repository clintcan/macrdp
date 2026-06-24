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

#[cfg(test)]
mod tests {
    use ironrdp_core::decode;
    use ironrdp_pdu::mcs::McsMessage;
    use ironrdp_pdu::rdp::multitransport::{MultitransportRequestPdu, RequestedProtocol};
    use ironrdp_pdu::x224::X224;
    use ironrdp_server::encode_initiate_request;

    // The Initiate Multitransport Request the server sends in M1 is the one bit
    // of new on-wire behavior that no real loopback client exercises (mstsc /
    // sdl-freerdp don't advertise UDP over 127.0.0.1). This round-trips the
    // exact bytes the server emits through ironrdp's own X.224/MCS + PDU
    // decoders, proving the IO-channel framing is structurally well-formed
    // (correct channel, BasicSecurityHeader/TRANSPORT_REQ, request fields) — the
    // framing risk flagged in the M1 plan. On-wire acceptance by Windows is
    // verified at M3 with a real UDP-capable client.
    #[test]
    fn initiate_request_round_trips_through_io_channel_framing() {
        let cookie = [0xABu8; 16];
        let bytes =
            encode_initiate_request(42, RequestedProtocol::UdpFecR, cookie, 1003, 1002).unwrap();

        let X224(msg) = decode::<X224<McsMessage<'_>>>(&bytes).unwrap();
        let sdi = match msg {
            McsMessage::SendDataIndication(sdi) => sdi,
            other => panic!("expected SendDataIndication, got {other:?}"),
        };
        assert_eq!(
            sdi.channel_id, 1003,
            "Initiate Request must ride the IO channel"
        );
        assert_eq!(sdi.initiator_id, 1002);

        // The inner PDU decodes as a MultitransportRequestPdu, which itself
        // re-validates the TRANSPORT_REQ security-header flag on decode.
        let req = decode::<MultitransportRequestPdu>(sdi.user_data.as_ref()).unwrap();
        assert_eq!(req.request_id, 42);
        assert_eq!(req.requested_protocol, RequestedProtocol::UdpFecR);
        assert_eq!(req.security_cookie, cookie);
    }
}
