//! MS-RDPEUDP v1 wire structures (the SYN-handshake + source-packet framing).
//!
//! **All multi-byte fields are big-endian** (network byte order) — unlike
//! MS-RDPBCGR. Section references are to [MS-RDPEUDP].
//!
//! [MS-RDPEUDP]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpeudp/

use bitflags::bitflags;
use ironrdp_core::{
    ensure_fixed_part_size, ensure_size, invalid_field_err, Decode, DecodeResult, Encode,
    EncodeResult, ReadCursor, WriteCursor,
};

bitflags! {
    /// `uFlags` of the [`FecHeader`] — RDPUDP_FLAG_* (MS-RDPEUDP 2.2.2.1).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct FecFlags: u16 {
        /// RDPUDP_FLAG_SYN — start a connection.
        const SYN = 0x0001;
        /// RDPUDP_FLAG_FIN — close a connection.
        const FIN = 0x0002;
        /// RDPUDP_FLAG_ACK — an ACK vector is present.
        const ACK = 0x0004;
        /// RDPUDP_FLAG_DATA — a source payload is present.
        const DATA = 0x0008;
        /// RDPUDP_FLAG_FEC — an FEC payload is present.
        const FEC = 0x0010;
        /// RDPUDP_FLAG_CN — congestion notification.
        const CN = 0x0020;
        /// RDPUDP_FLAG_CWR — congestion window reset.
        const CWR = 0x0040;
        /// RDPUDP_FLAG_SACK_OPTION — not used (per spec).
        const SACK_OPTION = 0x0080;
        /// RDPUDP_FLAG_ACK_OF_ACKS — an RDPUDP_ACK_OF_ACKVECTOR_HEADER is present.
        const ACK_OF_ACKS = 0x0100;
        /// RDPUDP_FLAG_SYNLOSSY — connection does not require persistent retransmits.
        const SYN_LOSSY = 0x0200;
        /// RDPUDP_FLAG_ACKDELAYED — the receiver delayed generating the ACK
        /// (do not use it for RTT estimation).
        const ACK_DELAYED = 0x0400;
        /// RDPUDP_FLAG_CORRELATION_ID — a correlation-id payload is present.
        const CORRELATION_ID = 0x0800;
        /// RDPUDP_FLAG_SYNEX — a SYNDATAEX payload is present.
        const SYNEX = 0x1000;
    }
}

/// RDPUDP_FEC_HEADER (MS-RDPEUDP 2.2.2.1) — the 8-byte header that prefixes
/// every RDPEUDP datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FecHeader {
    /// `snSourceAck` — the highest source sequence number acknowledged. `-1`
    /// (`0xFFFFFFFF`) in a SYN, before anything has been received.
    pub snd_source_ack: i32,
    /// `uReceiveWindowSize` — the receiver's buffer size, in datagrams.
    pub recv_window: u16,
    pub flags: FecFlags,
}

impl FecHeader {
    const NAME: &'static str = "RDPUDP_FEC_HEADER";
    pub const FIXED_PART_SIZE: usize = 4 /* snSourceAck */ + 2 /* uReceiveWindowSize */ + 2 /* uFlags */;
}

impl Encode for FecHeader {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        ensure_fixed_part_size!(in: dst);
        dst.write_slice(&self.snd_source_ack.to_be_bytes());
        dst.write_u16_be(self.recv_window);
        dst.write_u16_be(self.flags.bits());
        Ok(())
    }

    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn size(&self) -> usize {
        Self::FIXED_PART_SIZE
    }
}

impl Decode<'_> for FecHeader {
    fn decode(src: &mut ReadCursor<'_>) -> DecodeResult<Self> {
        ensure_fixed_part_size!(in: src);
        let snd_source_ack = src.read_i32_be();
        let recv_window = src.read_u16_be();
        let flags = FecFlags::from_bits_retain(src.read_u16_be());
        Ok(Self {
            snd_source_ack,
            recv_window,
            flags,
        })
    }
}

/// RDPUDP_SYNDATA_PAYLOAD (MS-RDPEUDP 2.2.2.5) — appended to SYN / SYN+ACK
/// datagrams.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SynData {
    /// `snInitialSequenceNumber` — a random 32-bit starting sequence number.
    pub initial_seq: u32,
    /// `uUpStreamMtu` — MUST be in 1132..=1232.
    pub upstream_mtu: u16,
    /// `uDownStreamMtu` — MUST be in 1132..=1232.
    pub downstream_mtu: u16,
}

impl SynData {
    const NAME: &'static str = "RDPUDP_SYNDATA_PAYLOAD";
    pub const FIXED_PART_SIZE: usize = 4 + 2 + 2;
}

impl Encode for SynData {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        ensure_fixed_part_size!(in: dst);
        dst.write_u32_be(self.initial_seq);
        dst.write_u16_be(self.upstream_mtu);
        dst.write_u16_be(self.downstream_mtu);
        Ok(())
    }

    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn size(&self) -> usize {
        Self::FIXED_PART_SIZE
    }
}

impl Decode<'_> for SynData {
    fn decode(src: &mut ReadCursor<'_>) -> DecodeResult<Self> {
        ensure_fixed_part_size!(in: src);
        Ok(Self {
            initial_seq: src.read_u32_be(),
            upstream_mtu: src.read_u16_be(),
            downstream_mtu: src.read_u16_be(),
        })
    }
}

/// RDP-UDP protocol version (`uUdpVer` of [`SynDataEx`], MS-RDPEUDP 2.2.2.9).
/// `V3` selects the RDPEUDP2 data-transfer framing (MS-RDPEUDP2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UdpVersion {
    /// `RDPUDP_PROTOCOL_VERSION_1` (0x0001).
    V1,
    /// `RDPUDP_PROTOCOL_VERSION_2` (0x0002).
    V2,
    /// `RDPUDP_PROTOCOL_VERSION_3` (0x0101) — data transfer uses MS-RDPEUDP2.
    V3,
}

impl UdpVersion {
    fn from_u16(v: u16) -> Option<Self> {
        match v {
            0x0001 => Some(Self::V1),
            0x0002 => Some(Self::V2),
            0x0101 => Some(Self::V3),
            _ => None,
        }
    }

    fn as_u16(self) -> u16 {
        match self {
            Self::V1 => 0x0001,
            Self::V2 => 0x0002,
            Self::V3 => 0x0101,
        }
    }
}

bitflags! {
    /// `uSynExFlags` of [`SynDataEx`] (MS-RDPEUDP 2.2.2.9).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct SynExFlags: u16 {
        /// RDPUDP_VERSION_INFO_VALID — `uUdpVer` carries a valid version.
        const VERSION_INFO_VALID = 0x0001;
    }
}

/// RDPUDP_SYNDATAEX_PAYLOAD (MS-RDPEUDP 2.2.2.9) — version negotiation, appended
/// to a SYN when [`FecFlags::SYNEX`] is set. `cookie_hash` is the 32-byte
/// SHA-256 of the Initiate Multitransport Request's security cookie, present
/// **iff** `udp_version == V3` (client→server).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SynDataEx {
    pub flags: SynExFlags,
    pub udp_version: UdpVersion,
    pub cookie_hash: Option<[u8; 32]>,
}

impl SynDataEx {
    const NAME: &'static str = "RDPUDP_SYNDATAEX_PAYLOAD";
    pub const FIXED_PART_SIZE: usize = 2 /* uSynExFlags */ + 2 /* uUdpVer */;
    const COOKIE_HASH_SIZE: usize = 32;

    /// Decode a SYNEX, choosing whether to expect the 32-byte `cookie_hash`.
    ///
    /// The hash's presence is **directional**, not derivable from the version
    /// alone: the **client→server SYN** carries it (V3), but the **server→client
    /// SYN+ACK omits it even for V3** (verified against a real capture). The
    /// caller — which knows the FEC flags — passes `expect_cookie_hash = false`
    /// for a SYN+ACK (ACK flag set) and `true` for a plain client SYN. The
    /// [`Decode`] impl defaults to the client-SYN case (the only one decoded in
    /// the server's production path).
    pub fn decode_directional(
        src: &mut ReadCursor<'_>,
        expect_cookie_hash: bool,
    ) -> DecodeResult<Self> {
        ensure_fixed_part_size!(in: src);
        let flags = SynExFlags::from_bits_retain(src.read_u16_be());
        let udp_version = UdpVersion::from_u16(src.read_u16_be())
            .ok_or_else(|| invalid_field_err!("uUdpVer", "unknown RDP-UDP protocol version"))?;
        let cookie_hash = if expect_cookie_hash && udp_version == UdpVersion::V3 {
            ensure_size!(in: src, size: Self::COOKIE_HASH_SIZE);
            Some(src.read_array::<{ Self::COOKIE_HASH_SIZE }>())
        } else {
            None
        };
        Ok(Self {
            flags,
            udp_version,
            cookie_hash,
        })
    }
}

impl Encode for SynDataEx {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        ensure_fixed_part_size!(in: dst);
        dst.write_u16_be(self.flags.bits());
        dst.write_u16_be(self.udp_version.as_u16());
        if let Some(hash) = &self.cookie_hash {
            ensure_size!(in: dst, size: Self::COOKIE_HASH_SIZE);
            dst.write_slice(hash);
        }
        Ok(())
    }

    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn size(&self) -> usize {
        Self::FIXED_PART_SIZE
            + if self.cookie_hash.is_some() {
                Self::COOKIE_HASH_SIZE
            } else {
                0
            }
    }
}

impl Decode<'_> for SynDataEx {
    /// Decodes a **client→server SYN**'s SYNEX (the production server path), where
    /// a V3 version carries the cookie hash. A SYN+ACK's SYNEX is decoded via
    /// [`SynDataEx::decode_directional`] with `expect_cookie_hash = false`.
    fn decode(src: &mut ReadCursor<'_>) -> DecodeResult<Self> {
        Self::decode_directional(src, true)
    }
}

/// RDPUDP_CORRELATION_ID_PAYLOAD (MS-RDPEUDP 2.2.2.8) — appended to a SYN when
/// [`FecFlags::CORRELATION_ID`] is set. 16 id bytes + 16 reserved (zero) bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CorrelationId {
    pub id: [u8; 16],
}

impl CorrelationId {
    const NAME: &'static str = "RDPUDP_CORRELATION_ID_PAYLOAD";
    pub const FIXED_PART_SIZE: usize = 16 /* uCorrelationId */ + 16 /* uReserved */;
}

impl Encode for CorrelationId {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        ensure_fixed_part_size!(in: dst);
        dst.write_slice(&self.id);
        dst.write_slice(&[0u8; 16]); // uReserved
        Ok(())
    }

    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn size(&self) -> usize {
        Self::FIXED_PART_SIZE
    }
}

impl Decode<'_> for CorrelationId {
    fn decode(src: &mut ReadCursor<'_>) -> DecodeResult<Self> {
        ensure_fixed_part_size!(in: src);
        let id = src.read_array::<16>();
        let _reserved = src.read_array::<16>();
        Ok(Self { id })
    }
}

/// RDPUDP_SOURCE_PAYLOAD_HEADER (MS-RDPEUDP 2.2.2.6) — prefixes the application
/// data of a source (DATA) packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourcePayloadHeader {
    /// `snCoded` — sequence number used for FEC coding.
    pub sn_coded: u32,
    /// `snSourceStart` — sequence number of this source packet.
    pub sn_source_start: u32,
}

impl SourcePayloadHeader {
    const NAME: &'static str = "RDPUDP_SOURCE_PAYLOAD_HEADER";
    pub const FIXED_PART_SIZE: usize = 4 + 4;
}

impl Encode for SourcePayloadHeader {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        ensure_fixed_part_size!(in: dst);
        dst.write_u32_be(self.sn_coded);
        dst.write_u32_be(self.sn_source_start);
        Ok(())
    }

    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn size(&self) -> usize {
        Self::FIXED_PART_SIZE
    }
}

impl Decode<'_> for SourcePayloadHeader {
    fn decode(src: &mut ReadCursor<'_>) -> DecodeResult<Self> {
        ensure_fixed_part_size!(in: src);
        Ok(Self {
            sn_coded: src.read_u32_be(),
            sn_source_start: src.read_u32_be(),
        })
    }
}

/// VECTOR_ELEMENT_STATE (MS-RDPEUDP 2.2.1.1) — the state of a run of datagrams
/// in the receiver's queue, carried in the top 2 bits of an ACK vector element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VectorElementState {
    /// DATAGRAM_RECEIVED (0).
    Received,
    /// DATAGRAM_RESERVED_1 (1).
    Reserved1,
    /// DATAGRAM_RESERVED_2 (2).
    Reserved2,
    /// DATAGRAM_NOT_YET_RECEIVED (3).
    NotYetReceived,
}

impl VectorElementState {
    fn from_bits(v: u8) -> Self {
        match v & 0b11 {
            0 => Self::Received,
            1 => Self::Reserved1,
            2 => Self::Reserved2,
            _ => Self::NotYetReceived,
        }
    }

    fn to_bits(self) -> u8 {
        match self {
            Self::Received => 0,
            Self::Reserved1 => 1,
            Self::Reserved2 => 2,
            Self::NotYetReceived => 3,
        }
    }
}

/// One run in the ACK vector: a `state` shared by `run_length` consecutive
/// datagrams. One byte on the wire: `(state << 6) | run_length` — 2-bit state in
/// the high bits, 6-bit length in the low bits (MS-RDPEUDP "ACK Vector Element").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AckVectorElement {
    pub state: VectorElementState,
    /// Run length, 0..=63 (6 bits).
    pub run_length: u8,
}

impl AckVectorElement {
    fn to_byte(self) -> u8 {
        (self.state.to_bits() << 6) | (self.run_length & 0x3f)
    }

    fn from_byte(b: u8) -> Self {
        Self {
            state: VectorElementState::from_bits(b >> 6),
            run_length: b & 0x3f,
        }
    }
}

/// RDPUDP_ACK_VECTOR_HEADER (MS-RDPEUDP 2.2.2.7) — the selective-ACK vector,
/// present when [`FecFlags::ACK`] is set. Run-length-encoded datagram states for
/// the window after `FecHeader::snd_source_ack`. On the wire, `uAckVectorSize`
/// (the element count) plus the element bytes are zero-padded to a multiple of
/// 4 bytes (confirmed against the spec's ACK-packet capture).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AckVectorHeader {
    pub elements: Vec<AckVectorElement>,
}

impl AckVectorHeader {
    const NAME: &'static str = "RDPUDP_ACK_VECTOR_HEADER";
    pub const FIXED_PART_SIZE: usize = 2; // uAckVectorSize

    /// Total on-wire size for `n` elements: `uAckVectorSize` + elements, padded
    /// up to a 4-byte multiple.
    fn padded_len(n: usize) -> usize {
        (Self::FIXED_PART_SIZE + n).next_multiple_of(4)
    }
}

impl Encode for AckVectorHeader {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        let n = self.elements.len();
        let total = Self::padded_len(n);
        ensure_size!(in: dst, size: total);
        dst.write_u16_be(n as u16); // uAckVectorSize
        for e in &self.elements {
            dst.write_u8(e.to_byte());
        }
        for _ in 0..(total - Self::FIXED_PART_SIZE - n) {
            dst.write_u8(0); // zero-padding to the 4-byte boundary
        }
        Ok(())
    }

    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn size(&self) -> usize {
        Self::padded_len(self.elements.len())
    }
}

impl Decode<'_> for AckVectorHeader {
    fn decode(src: &mut ReadCursor<'_>) -> DecodeResult<Self> {
        ensure_fixed_part_size!(in: src);
        let n = usize::from(src.read_u16_be());
        ensure_size!(in: src, size: n);
        let mut elements = Vec::with_capacity(n);
        for _ in 0..n {
            elements.push(AckVectorElement::from_byte(src.read_u8()));
        }
        // Consume the zero-padding to the 4-byte boundary.
        let pad = Self::padded_len(n) - Self::FIXED_PART_SIZE - n;
        ensure_size!(in: src, size: pad);
        let _ = src.read_slice(pad);
        Ok(Self { elements })
    }
}

#[cfg(test)]
mod tests {
    use ironrdp_core::{decode, encode_vec};

    use super::*;

    #[test]
    fn fec_header_round_trips() {
        let h = FecHeader {
            snd_source_ack: -5,
            recv_window: 1024,
            flags: FecFlags::DATA | FecFlags::ACK,
        };
        let bytes = encode_vec(&h).unwrap();
        assert_eq!(bytes.len(), FecHeader::FIXED_PART_SIZE);
        assert_eq!(decode::<FecHeader>(&bytes).unwrap(), h);
    }

    #[test]
    fn fec_header_matches_spec_capture() {
        // MS-RDPEUDP "Source Packet" example (section 4): d6 cf 0a b8 04 00 00 0c
        let bytes = [0xd6, 0xcf, 0x0a, 0xb8, 0x04, 0x00, 0x00, 0x0c];
        let h = decode::<FecHeader>(&bytes).unwrap();
        assert_eq!(
            h.snd_source_ack, -691_074_376,
            "snSourceAck (big-endian, signed)"
        );
        assert_eq!(h.recv_window, 1024);
        assert_eq!(h.flags, FecFlags::DATA | FecFlags::ACK);
        assert_eq!(
            encode_vec(&h).unwrap(),
            bytes,
            "re-encodes byte-identically"
        );
    }

    #[test]
    fn syn_data_round_trips() {
        let s = SynData {
            initial_seq: 0x1234_5678,
            upstream_mtu: 1232,
            downstream_mtu: 1132,
        };
        let bytes = encode_vec(&s).unwrap();
        assert_eq!(decode::<SynData>(&bytes).unwrap(), s);
    }

    #[test]
    fn syn_data_ex_round_trips_each_version() {
        for (ver, cookie) in [
            (UdpVersion::V1, None),
            (UdpVersion::V2, None),
            (UdpVersion::V3, Some([0xAB; 32])),
        ] {
            let ex = SynDataEx {
                flags: SynExFlags::VERSION_INFO_VALID,
                udp_version: ver,
                cookie_hash: cookie,
            };
            let bytes = encode_vec(&ex).unwrap();
            assert_eq!(
                decode::<SynDataEx>(&bytes).unwrap(),
                ex,
                "round-trip {ver:?}"
            );
        }
    }

    #[test]
    fn syn_data_ex_v3_requires_cookie() {
        // uSynExFlags=0x0001, uUdpVer=0x0101 (V3), but no 32-byte cookie → error.
        let bytes = [0x00, 0x01, 0x01, 0x01];
        assert!(decode::<SynDataEx>(&bytes).is_err());
    }

    #[test]
    fn syn_data_ex_rejects_unknown_version() {
        let bytes = [0x00, 0x01, 0x09, 0x99];
        assert!(decode::<SynDataEx>(&bytes).is_err());
    }

    #[test]
    fn correlation_id_round_trips_and_zeroes_reserved() {
        let c = CorrelationId { id: [0x11; 16] };
        let bytes = encode_vec(&c).unwrap();
        assert_eq!(bytes.len(), 32);
        assert_eq!(&bytes[16..], &[0u8; 16], "uReserved must be zero");
        assert_eq!(decode::<CorrelationId>(&bytes).unwrap(), c);
    }

    #[test]
    fn fec_flag_values_match_spec() {
        // Authoritative values from MS-RDPEUDP 2.2.2.1 (uFlags). Locks the M2a
        // correction (0x0080+ were wrong from memory).
        assert_eq!(FecFlags::SYN.bits(), 0x0001);
        assert_eq!(FecFlags::FIN.bits(), 0x0002);
        assert_eq!(FecFlags::ACK.bits(), 0x0004);
        assert_eq!(FecFlags::DATA.bits(), 0x0008);
        assert_eq!(FecFlags::FEC.bits(), 0x0010);
        assert_eq!(FecFlags::CN.bits(), 0x0020);
        assert_eq!(FecFlags::CWR.bits(), 0x0040);
        assert_eq!(FecFlags::SACK_OPTION.bits(), 0x0080);
        assert_eq!(FecFlags::ACK_OF_ACKS.bits(), 0x0100);
        assert_eq!(FecFlags::SYN_LOSSY.bits(), 0x0200);
        assert_eq!(FecFlags::ACK_DELAYED.bits(), 0x0400);
        assert_eq!(FecFlags::CORRELATION_ID.bits(), 0x0800);
        assert_eq!(FecFlags::SYNEX.bits(), 0x1000);
    }

    #[test]
    fn fec_header_ack_packet_flags_match_spec_capture() {
        // MS-RDPEUDP "ACK Packet" example FEC header: d6 cf 0a b8 04 00 01 0c
        // → uFlags 0x010c = ACK_OF_ACKS | DATA | ACK.
        let bytes = [0xd6, 0xcf, 0x0a, 0xb8, 0x04, 0x00, 0x01, 0x0c];
        let h = decode::<FecHeader>(&bytes).unwrap();
        assert_eq!(
            h.flags,
            FecFlags::ACK_OF_ACKS | FecFlags::DATA | FecFlags::ACK
        );
    }

    #[test]
    fn ack_vector_round_trips_and_pads_to_4_bytes() {
        for elems in [
            vec![],
            vec![AckVectorElement {
                state: VectorElementState::Received,
                run_length: 4,
            }],
            vec![
                AckVectorElement {
                    state: VectorElementState::Received,
                    run_length: 10,
                },
                AckVectorElement {
                    state: VectorElementState::NotYetReceived,
                    run_length: 2,
                },
            ],
            vec![
                AckVectorElement {
                    state: VectorElementState::Received,
                    run_length: 1,
                },
                AckVectorElement {
                    state: VectorElementState::NotYetReceived,
                    run_length: 1,
                },
                AckVectorElement {
                    state: VectorElementState::Received,
                    run_length: 1,
                },
            ],
        ] {
            let h = AckVectorHeader { elements: elems };
            let bytes = encode_vec(&h).unwrap();
            assert_eq!(bytes.len() % 4, 0, "padded to a 4-byte multiple");
            assert_eq!(decode::<AckVectorHeader>(&bytes).unwrap(), h);
        }
    }

    #[test]
    fn ack_vector_matches_spec_capture() {
        // From the "ACK Packet" capture: 00 01 04 00 (size=1, element 0x04
        // = state Received + run length 4, then 1 byte of padding).
        let bytes = [0x00, 0x01, 0x04, 0x00];
        let h = decode::<AckVectorHeader>(&bytes).unwrap();
        assert_eq!(
            h.elements,
            vec![AckVectorElement {
                state: VectorElementState::Received,
                run_length: 4
            }]
        );
        assert_eq!(
            encode_vec(&h).unwrap(),
            bytes,
            "re-encodes byte-identically incl. padding"
        );
    }

    #[test]
    fn source_payload_header_matches_spec_capture() {
        // MS-RDPEUDP "Source Packet" example: ec 47 1a e4 ec 47 1a e4
        let bytes = [0xec, 0x47, 0x1a, 0xe4, 0xec, 0x47, 0x1a, 0xe4];
        let h = decode::<SourcePayloadHeader>(&bytes).unwrap();
        assert_eq!(h.sn_coded, 0xec47_1ae4);
        assert_eq!(h.sn_source_start, 0xec47_1ae4);
        assert_eq!(encode_vec(&h).unwrap(), bytes);
    }
}
