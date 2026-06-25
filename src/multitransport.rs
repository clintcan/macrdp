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
    use ironrdp_server::{encode_initiate_request, CookieRegistry};

    // M5a: the cookie registry binds an inbound UDP tunnel to a real TCP session.
    // Its security property is one-time use — a registered cookie is accepted by
    // `take` exactly once (so a forged/replayed CREATEREQUEST can't open a second
    // tunnel), and an unregistered cookie is never accepted. The registry lives in
    // the vendored (test = false) server crate, so its behavior is tested here.
    #[test]
    fn cookie_registry_take_is_one_time_and_rejects_unknown() {
        let reg = CookieRegistry::new();
        let cookie = [0xABu8; 16];
        let other = [0x11u8; 16];

        // Unknown cookie is rejected.
        assert!(!reg.take(&other), "unregistered cookie must not bind");

        // Registered cookie binds exactly once, then is consumed.
        reg.insert(cookie);
        assert!(reg.take(&cookie), "registered cookie must bind once");
        assert!(
            !reg.take(&cookie),
            "a consumed cookie must not bind again (replay protection)"
        );

        // Explicit removal (teardown / eviction) also clears it.
        reg.insert(other);
        reg.remove(&other);
        assert!(!reg.take(&other), "removed cookie must not bind");
    }

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

    // M3b: the real UDP listener over loopback. The vendored ironrdp-server is
    // built `test = false`, so this socket-level test lives here. It binds the
    // listener to an ephemeral 127.0.0.1 UDP port, sends a REAL captured Microsoft
    // client SYN from a second socket, and asserts the listener replies with a
    // wire-correct, MTU-padded SYN+ACK (flags SYN|ACK|SYNEX, snSourceAck = the
    // client's ISN, V3 negotiated, no cookie hash, no ack vector). Runs in CI
    // (loopback UDP); a real Windows client accepting the SYN+ACK is verified
    // live. The server ISN is listener-chosen, so we decode + assert fields
    // rather than byte-compare the whole packet.
    #[tokio::test]
    async fn listener_answers_real_client_syn_over_loopback() {
        use ironrdp_rdpeudp::datagram::Datagram;
        use ironrdp_rdpeudp::pdu::{FecFlags, UdpVersion};
        use ironrdp_server::{ListenerConfig, UdpMultitransportListener};
        use std::time::Duration;
        use tokio::net::UdpSocket;

        #[rustfmt::skip]
        let client_syn: [u8; 84] = [
            0xff, 0xff, 0xff, 0xff, 0x00, 0x40, 0x18, 0x01, 0x64, 0x7a, 0x02, 0xbc, 0x04, 0xd0,
            0x04, 0xd0, 0x43, 0x33, 0x3c, 0x63, 0xee, 0x77, 0x40, 0x6e, 0x97, 0xdf, 0x80, 0x0c,
            0xa1, 0xfd, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x01, 0x01, 0xcb, 0x86, 0x4c, 0x5b,
            0x54, 0x3a, 0xdc, 0x7a, 0x7a, 0x36, 0x7b, 0xb8, 0x11, 0x20, 0x71, 0x7c, 0x28, 0x6d,
            0x09, 0x3d, 0x3f, 0x3a, 0xd8, 0x80, 0x2c, 0x59, 0x4f, 0x4f, 0x21, 0x99, 0x86, 0x94,
        ];

        let cfg = ListenerConfig::default();
        // No TLS config and no cookie registry: this test only exercises the
        // SYN→SYN+ACK handshake (soft cookie binding).
        let listener =
            UdpMultitransportListener::bind("127.0.0.1:0".parse().unwrap(), cfg, None, None)
                .await
                .expect("bind listener");
        let server_addr = listener.local_addr();

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.send_to(&client_syn, server_addr).await.unwrap();

        let mut buf = vec![0u8; 2048];
        let len = tokio::time::timeout(Duration::from_secs(2), client.recv(&mut buf))
            .await
            .expect("listener should reply within 2s")
            .expect("recv");
        let reply = &buf[..len];

        // SYN/SYN+ACK packets are MTU-padded for path validation.
        assert_eq!(len, cfg.mtu as usize, "SYN+ACK must be padded to the MTU");

        let dg = Datagram::decode(reply).expect("decode SYN+ACK");
        assert_eq!(
            dg.fec.flags,
            FecFlags::SYN | FecFlags::ACK | FecFlags::SYNEX
        );
        assert_eq!(
            dg.fec.snd_source_ack as u32, 0x647a_02bc,
            "snSourceAck must echo the client's ISN"
        );
        assert!(dg.ack_vector.is_none(), "SYN+ACK carries no ack vector");
        let ex = dg.syn_ex.expect("SYNEX present");
        assert_eq!(ex.udp_version, UdpVersion::V3, "V3/EUDP2 negotiated");
        assert!(
            ex.cookie_hash.is_none(),
            "server SYN+ACK omits the cookie hash"
        );
        assert_eq!(dg.syn.expect("SYNDATA").upstream_mtu, 1232);
    }
}
