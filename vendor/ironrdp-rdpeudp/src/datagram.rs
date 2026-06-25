//! Whole-datagram assembly/parsing for RDPEUDP **v1**: composes the [`pdu`]
//! primitives (FEC header + optional SYN / ACK-vector / source sections) into a
//! single datagram and back, driven by the [`FecFlags`].
//!
//! This is the wire layer the reliability state machine ([`crate::state`]) emits
//! and consumes. It deliberately models only what that machine needs (SYN /
//! SYN+ACK / data / ack); it never *produces* the optional FEC or ACK-of-acks
//! sections. On decode it *skips* an inbound RDPUDP_ACK_OF_ACKVECTOR_HEADER
//! (real mstsc sets it periodically) so the source payload after it stays
//! aligned. The RDPEUDP2 (`0x0101`) data framing is a separate, spike-gated
//! codec — see the crate docs.
//!
//! [`pdu`]: crate::pdu
//! [`FecFlags`]: crate::pdu::FecFlags

use ironrdp_core::{
    decode_cursor, encode_cursor, ensure_size, DecodeResult, EncodeResult, ReadCursor, WriteCursor,
};

use crate::pdu::{
    AckVectorHeader, CorrelationId, FecFlags, FecHeader, SourcePayloadHeader, SynData, SynDataEx,
    SynExFlags, UdpVersion,
};

/// The application payload carried by a source (DATA) packet, with its sequence
/// header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourcePacket {
    pub header: SourcePayloadHeader,
    pub payload: Vec<u8>,
}

/// A decoded/decodable RDPEUDP v1 datagram: the mandatory [`FecHeader`] plus
/// whichever optional sections its flags select. Construct via the helpers
/// ([`Datagram::syn`], [`Datagram::data`], [`Datagram::ack`]) so the flags and
/// sections stay consistent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Datagram {
    pub fec: FecHeader,
    pub ack_vector: Option<AckVectorHeader>,
    pub source: Option<SourcePacket>,
    pub syn: Option<SynData>,
    pub correlation: Option<CorrelationId>,
    pub syn_ex: Option<SynDataEx>,
}

impl Datagram {
    /// A client→server **SYN** (or a generic SYN-with-ack-vector): carries SYNDATA
    /// and optionally SYNEX (version negotiation). `snSourceAck` is the caller's
    /// (use `-1` for an initial SYN). For the server's reply use the dedicated
    /// [`Datagram::syn_ack`], which matches real Windows (ACK flag, no vector).
    pub fn syn(
        snd_source_ack: i32,
        recv_window: u16,
        syn: SynData,
        syn_ex: Option<SynDataEx>,
        ack_vector: Option<AckVectorHeader>,
    ) -> Self {
        let mut flags = FecFlags::SYN;
        if syn_ex.is_some() {
            flags |= FecFlags::SYNEX;
        }
        if ack_vector.is_some() {
            flags |= FecFlags::ACK;
        }
        Self {
            fec: FecHeader {
                snd_source_ack,
                recv_window,
                flags,
            },
            ack_vector,
            source: None,
            syn: Some(syn),
            correlation: None,
            syn_ex,
        }
    }

    /// A server→client **SYN+ACK** matching what a real Windows RDP server emits
    /// (verified byte-exact against a capture): flags `SYN | ACK | SYNEX`,
    /// `snSourceAck = client_isn` (the ISN from the client's SYNDATA), our own
    /// SYNDATA, and a SYNEX echoing the negotiated `udp_version`. Two quirks the
    /// generic [`Datagram::syn`] can't express: the ACK flag is set but **no
    /// ack-vector section is appended** (the ack rides `snSourceAck`), and the
    /// SYNEX **omits the cookie hash even for V3** (it's client→server only).
    pub fn syn_ack(
        client_isn: u32,
        recv_window: u16,
        syn: SynData,
        udp_version: UdpVersion,
    ) -> Self {
        Self {
            fec: FecHeader {
                snd_source_ack: client_isn as i32,
                recv_window,
                flags: FecFlags::SYN | FecFlags::ACK | FecFlags::SYNEX,
            },
            ack_vector: None,
            source: None,
            syn: Some(syn),
            correlation: None,
            syn_ex: Some(SynDataEx {
                flags: SynExFlags::VERSION_INFO_VALID,
                udp_version,
                cookie_hash: None,
            }),
        }
    }

    /// A DATA+ACK datagram: a source payload plus the cumulative/selective ACK.
    pub fn data(
        snd_source_ack: i32,
        recv_window: u16,
        ack_vector: AckVectorHeader,
        source: SourcePacket,
    ) -> Self {
        Self {
            fec: FecHeader {
                snd_source_ack,
                recv_window,
                flags: FecFlags::DATA | FecFlags::ACK,
            },
            ack_vector: Some(ack_vector),
            source: Some(source),
            syn: None,
            correlation: None,
            syn_ex: None,
        }
    }

    /// A pure ACK datagram (no payload).
    pub fn ack(snd_source_ack: i32, recv_window: u16, ack_vector: AckVectorHeader) -> Self {
        Self {
            fec: FecHeader {
                snd_source_ack,
                recv_window,
                flags: FecFlags::ACK,
            },
            ack_vector: Some(ack_vector),
            source: None,
            syn: None,
            correlation: None,
            syn_ex: None,
        }
    }

    /// Peek the FEC flags of a **v1** datagram without a full decode — cheap
    /// enough to call per outbound datagram so the UDP listener can tell whether
    /// a packet is a SYN-family handshake packet (which MS-RDPEUDP requires be
    /// zero-padded to the MTU) vs an ordinary data/ack packet (not padded).
    /// Returns `None` if the buffer is shorter than the 8-byte FEC header.
    pub fn peek_fec_flags(bytes: &[u8]) -> Option<FecFlags> {
        if bytes.len() < FecHeader::FIXED_PART_SIZE {
            return None;
        }
        // FEC header: snSourceAck(4) + uReceiveWindowSize(2) + uFlags(2, BE).
        Some(FecFlags::from_bits_retain(u16::from_be_bytes([
            bytes[6], bytes[7],
        ])))
    }

    pub fn encode(&self) -> EncodeResult<Vec<u8>> {
        // Worst-case size; the cursor encoder only writes what's present.
        let cap = 8 + 8 + 8 + 4 + 32 + 36 + self.source.as_ref().map_or(0, |s| s.payload.len());
        let mut buf = vec![0u8; cap];
        let written = {
            let mut cursor = WriteCursor::new(&mut buf);
            self.encode_cursor(&mut cursor)?;
            cursor.pos()
        };
        buf.truncate(written);
        Ok(buf)
    }

    fn encode_cursor(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        encode_cursor(&self.fec, dst)?;
        // Section order mirrors MS-RDPEUDP: ACK vector, then (for data) the
        // source payload; for SYN datagrams the SYN sections follow.
        if let Some(ack) = &self.ack_vector {
            encode_cursor(ack, dst)?;
        }
        if let Some(src) = &self.source {
            encode_cursor(&src.header, dst)?;
            dst.write_slice(&src.payload);
        }
        if let Some(syn) = &self.syn {
            encode_cursor(syn, dst)?;
        }
        if let Some(corr) = &self.correlation {
            encode_cursor(corr, dst)?;
        }
        if let Some(ex) = &self.syn_ex {
            encode_cursor(ex, dst)?;
        }
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> DecodeResult<Self> {
        let mut cursor = ReadCursor::new(bytes);
        let fec: FecHeader = decode_cursor(&mut cursor)?;
        let flags = fec.flags;

        // A SYN / SYN+ACK carries NO ack-vector section even when the ACK flag is
        // set (its acknowledgment rides `snSourceAck`); the vector appears only on
        // non-SYN data/ack packets. Verified against a real SYN+ACK capture.
        let ack_vector = if flags.contains(FecFlags::ACK) && !flags.contains(FecFlags::SYN) {
            Some(decode_cursor::<AckVectorHeader>(&mut cursor)?)
        } else {
            None
        };

        // An RDPUDP_ACK_OF_ACKVECTOR_HEADER (single u32 snAckOfAcksSeqNum, 4
        // bytes) sits between the ack vector and the source payload when the
        // ACK_OF_ACKS flag is set. We don't act on it (cumulative-ACK only), but
        // it MUST be skipped or the source-payload offset is wrong — real mstsc
        // sets this periodically, and folding the 4 bytes into the reliable
        // stream corrupts the TLS record framing (InvalidContentType).
        if flags.contains(FecFlags::ACK_OF_ACKS) && !flags.contains(FecFlags::SYN) {
            ensure_size!(in: cursor, size: 4);
            let _sn_ack_of_acks = cursor.read_u32_be();
        }

        // For a SYN datagram the trailing bytes are SYN sections, not a source
        // payload; for a data datagram they are the source header + payload.
        let (source, syn, correlation, syn_ex) = if flags.contains(FecFlags::SYN) {
            let syn = Some(decode_cursor::<SynData>(&mut cursor)?);
            let correlation = if flags.contains(FecFlags::CORRELATION_ID) {
                Some(decode_cursor::<CorrelationId>(&mut cursor)?)
            } else {
                None
            };
            let syn_ex = if flags.contains(FecFlags::SYNEX) {
                // A SYN+ACK (ACK flag set) omits the cookie hash; a plain client
                // SYN includes it for V3.
                let expect_cookie_hash = !flags.contains(FecFlags::ACK);
                Some(SynDataEx::decode_directional(
                    &mut cursor,
                    expect_cookie_hash,
                )?)
            } else {
                None
            };
            (None, syn, correlation, syn_ex)
        } else if flags.contains(FecFlags::DATA) {
            let header: SourcePayloadHeader = decode_cursor(&mut cursor)?;
            let payload = cursor.read_remaining().to_vec();
            (Some(SourcePacket { header, payload }), None, None, None)
        } else {
            (None, None, None, None)
        };

        Ok(Self {
            fec,
            ack_vector,
            source,
            syn,
            correlation,
            syn_ex,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pdu::{AckVectorElement, UdpVersion, VectorElementState};

    fn empty_ack() -> AckVectorHeader {
        AckVectorHeader { elements: vec![] }
    }

    #[test]
    fn syn_with_synex_round_trips() {
        let d = Datagram::syn(
            -1,
            64,
            SynData {
                initial_seq: 0x1111_2222,
                upstream_mtu: 1232,
                downstream_mtu: 1232,
            },
            Some(SynDataEx {
                flags: crate::pdu::SynExFlags::VERSION_INFO_VALID,
                udp_version: UdpVersion::V2,
                cookie_hash: None,
            }),
            None,
        );
        let bytes = d.encode().unwrap();
        assert_eq!(Datagram::decode(&bytes).unwrap(), d);
    }

    #[test]
    fn data_packet_round_trips() {
        let d = Datagram::data(
            41,
            64,
            AckVectorHeader {
                elements: vec![AckVectorElement {
                    state: VectorElementState::Received,
                    run_length: 3,
                }],
            },
            SourcePacket {
                header: SourcePayloadHeader {
                    sn_coded: 42,
                    sn_source_start: 42,
                },
                payload: b"hello rdpeudp".to_vec(),
            },
        );
        let bytes = d.encode().unwrap();
        let back = Datagram::decode(&bytes).unwrap();
        assert_eq!(back, d);
        assert_eq!(back.source.unwrap().payload, b"hello rdpeudp");
    }

    /// Real mstsc periodically sets the ACK_OF_ACKS flag on a data packet, which
    /// inserts a 4-byte RDPUDP_ACK_OF_ACKVECTOR_HEADER between the ack vector and
    /// the source payload. The decoder must skip it, or those 4 bytes are folded
    /// into the reliable byte-stream and corrupt the TLS records riding it
    /// (observed live as rustls `InvalidContentType` mid-session).
    #[test]
    fn data_packet_with_ack_of_acks_skips_the_section() {
        #[rustfmt::skip]
        let bytes: Vec<u8> = vec![
            // RDPUDP_FEC_HEADER
            0x00, 0x00, 0x00, 0x29, // snSourceAck = 41
            0x00, 0x40,             // uReceiveWindowSize = 64
            0x01, 0x0c,             // uFlags = ACK | DATA | ACK_OF_ACKS (0x010c)
            // RDPUDP_ACK_VECTOR_HEADER (empty, padded to 4 bytes)
            0x00, 0x00,             // uAckVectorSize = 0
            0x00, 0x00,             // zero-padding to the 4-byte boundary
            // RDPUDP_ACK_OF_ACKVECTOR_HEADER (snAckOfAcksSeqNum) — must be skipped
            0xde, 0xad, 0xbe, 0xef,
            // RDPUDP_SOURCE_PAYLOAD_HEADER
            0x00, 0x00, 0x00, 0x2a, // snCoded = 42
            0x00, 0x00, 0x00, 0x2a, // snSourceStart = 42
            // payload
            b'h', b'e', b'l', b'l', b'o',
        ];

        let dg = Datagram::decode(&bytes).unwrap();
        assert!(dg.ack_vector.is_some());
        let src = dg.source.expect("source payload present");
        assert_eq!(src.header.sn_coded, 42);
        assert_eq!(
            src.payload, b"hello",
            "the 4-byte ack-of-acks section must NOT leak into the payload"
        );
    }

    #[test]
    fn peek_fec_flags_reads_syn_without_full_decode() {
        let syn = Datagram::syn(
            -1,
            64,
            SynData {
                initial_seq: 1,
                upstream_mtu: 1232,
                downstream_mtu: 1232,
            },
            None,
            None,
        )
        .encode()
        .unwrap();
        assert!(Datagram::peek_fec_flags(&syn)
            .unwrap()
            .contains(FecFlags::SYN));

        // A pure ack has no SYN bit; a too-short buffer returns None.
        let ack = Datagram::ack(7, 64, empty_ack()).encode().unwrap();
        assert!(!Datagram::peek_fec_flags(&ack)
            .unwrap()
            .contains(FecFlags::SYN));
        assert!(Datagram::peek_fec_flags(&[0, 1, 2]).is_none());
    }

    #[test]
    fn pure_ack_round_trips() {
        let d = Datagram::ack(7, 64, empty_ack());
        let bytes = d.encode().unwrap();
        let back = Datagram::decode(&bytes).unwrap();
        assert_eq!(back, d);
        assert!(back.source.is_none() && back.syn.is_none());
    }

    /// The server's SYN+ACK must be byte-shaped exactly like a real Windows RDP
    /// server's. These 20 bytes are the meaningful prefix (before MTU zero-pad) of
    /// the **server→client SYN+ACK** captured opposite the client SYN below:
    /// FEC(snSourceAck=client ISN, win 64, flags SYN|ACK|SYNEX) + SYNDATA(server
    /// ISN, MTUs) + SYNEX(VERSION_INFO_VALID, V3) — no ack vector, no cookie hash.
    #[test]
    fn syn_ack_matches_real_server_capture() {
        #[rustfmt::skip]
        let expected: [u8; 20] = [
            0x64, 0x7a, 0x02, 0xbc, // snSourceAck = client ISN 0x647a02bc
            0x00, 0x40,             // uReceiveWindowSize = 64
            0x10, 0x05,             // uFlags = SYN|ACK|SYNEX
            0x61, 0xf7, 0xb6, 0xe3, // SYNDATA snInitialSequenceNumber (server ISN)
            0x04, 0xd0,             // uUpStreamMtu = 1232
            0x04, 0xd0,             // uDownStreamMtu = 1232
            0x00, 0x01,             // uSynExFlags = VERSION_INFO_VALID
            0x01, 0x01,             // uUdpVer = 0x0101 = V3 (no cookie hash follows)
        ];

        let d = Datagram::syn_ack(
            0x647a_02bc,
            64,
            SynData {
                initial_seq: 0x61f7_b6e3,
                upstream_mtu: 1232,
                downstream_mtu: 1232,
            },
            UdpVersion::V3,
        );
        let bytes = d.encode().unwrap();
        assert_eq!(
            bytes, expected,
            "SYN+ACK must match the captured server bytes"
        );
        // And it must round-trip back (ACK flag set but no ack vector; V3 SYNEX
        // with no cookie hash — the directional-decode path).
        assert_eq!(Datagram::decode(&bytes).unwrap(), d);
    }

    /// Decode a **real Microsoft RDP client's SYN** from a user capture — the
    /// client connecting to a **Windows RDP server in a VM** (VirtualBox NAT,
    /// host:3390 forwarded to the VM's :3389; the UDP SYNs went unanswered
    /// because only TCP was forwarded). It is *not* a macrdp capture, so it does
    /// not exercise macrdp's own send path — but it's the strongest validation of
    /// the v1 SYN / CORRELATION_ID / SYNEX codecs (real client bytes, not a
    /// hand-built struct): it confirms the `FecFlags` correction
    /// (`0x1801 = SYN|CORRELATION_ID|SYNEX`) and that the client negotiates
    /// `uUdpVer = V3` (EUDP2) with a 32-byte SHA-256 `cookieHash` (the client
    /// received and acted on the *server's* Initiate Multitransport Request).
    #[test]
    fn decodes_real_client_syn_capture() {
        // The first 84 bytes of the captured 1232-byte UDP payload (the rest is
        // zero-padding): FEC(8) + SYNDATA(8) + CORRELATION_ID(32) + SYNEX(4+32).
        #[rustfmt::skip]
        let bytes: [u8; 84] = [
            // FEC header
            0xff, 0xff, 0xff, 0xff,             // snSourceAck = -1
            0x00, 0x40,                         // uReceiveWindowSize = 64
            0x18, 0x01,                         // uFlags = SYN|CORRELATION_ID|SYNEX
            // SYNDATA
            0x70, 0x14, 0xc0, 0xfb,             // snInitialSequenceNumber
            0x04, 0xd0,                         // uUpStreamMtu = 1232
            0x04, 0xd0,                         // uDownStreamMtu = 1232
            // CORRELATION_ID: 16 id + 16 reserved(0)
            0x25, 0x06, 0x04, 0xe2, 0x05, 0xd5, 0x47, 0x4d,
            0x9a, 0x97, 0x96, 0x76, 0x8f, 0x7d, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            // SYNEX
            0x00, 0x01,                         // uSynExFlags = VERSION_INFO_VALID
            0x01, 0x01,                         // uUdpVer = 0x0101 = V3 (EUDP2)
            // cookieHash (32 bytes)
            0x7f, 0x7d, 0x57, 0x77, 0x45, 0x80, 0x73, 0x6f,
            0xea, 0x6c, 0x52, 0x4c, 0x59, 0x82, 0xe1, 0x31,
            0x0e, 0x2a, 0x9c, 0x97, 0x39, 0xad, 0x15, 0xab,
            0x28, 0xb2, 0xb2, 0x21, 0x59, 0xce, 0xf0, 0xef,
        ];

        let d = Datagram::decode(&bytes).unwrap();
        assert_eq!(d.fec.snd_source_ack, -1);
        assert_eq!(d.fec.recv_window, 64);
        assert_eq!(
            d.fec.flags,
            FecFlags::SYN | FecFlags::CORRELATION_ID | FecFlags::SYNEX
        );

        let syn = d.syn.expect("SYNDATA present");
        assert_eq!(syn.initial_seq, 0x7014_c0fb);
        assert_eq!(syn.upstream_mtu, 1232);
        assert_eq!(syn.downstream_mtu, 1232);

        assert_eq!(
            d.correlation.expect("CORRELATION_ID present").id,
            [
                0x25, 0x06, 0x04, 0xe2, 0x05, 0xd5, 0x47, 0x4d, 0x9a, 0x97, 0x96, 0x76, 0x8f, 0x7d,
                0x00, 0x00
            ]
        );

        let ex = d.syn_ex.expect("SYNEX present");
        assert_eq!(ex.flags, crate::pdu::SynExFlags::VERSION_INFO_VALID);
        assert_eq!(ex.udp_version, UdpVersion::V3, "client negotiates EUDP2");
        assert!(
            ex.cookie_hash.is_some(),
            "V3 carries the SHA-256 cookie hash"
        );
    }
}
