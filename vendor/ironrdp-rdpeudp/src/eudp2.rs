//! MS-RDPEUDP2 (`uUdpVer = 0x0101`) framing — the data-transfer format a modern
//! client (mstsc / Windows App) upgrades to after the v1 SYN handshake.
//!
//! # The network-format transform (the non-obvious part)
//!
//! An RDP-UDP2 packet is **not** sent as a straight serialization. A
//! `PacketPrefixByte` is interleaved into the first 8 bytes: on the wire, **byte
//! 0 and byte 7 are swapped** (the prefix byte rides at offset 7, the real first
//! header byte at offset 0). [`unwrap_packet`] undoes this (and, being a single
//! swap, is its own inverse — [`wrap_packet`] is the same operation). After
//! unwrapping, the layout is:
//!
//! ```text
//!   offset 0      : PacketPrefixByte   (reserved:1, Packet_Type_Index:4, Short_Packet_Length:3)
//!   offset 1..=2  : u16 LE  -> Flags (low 12 bits) + LogWindowSize (high 4 bits)
//!   offset 3..    : optional sub-headers (ACK, OverheadSize, DelayAckInfo,
//!                   AckOfAcks, DataHeader, AckVector), in that order, then DataBody
//! ```
//!
//! This was reverse-engineered from FreeRDP's `tools/wireshark/rdp-udp.lua`
//! dissector (`unwrapPacket`) and **verified byte-exact against a real
//! Microsoft-client capture** (see the tests). The receiver also uses the
//! offset-7 byte to tell RDP-UDP2 from RDP-UDP v1 (a v1 packet has an invalid
//! prefix there).
//!
//! M-eudp2 step 1 (this module): the transform + the `PacketPrefixByte` + the
//! base `Eudp2Header`. Walking the optional sub-headers to extract the DataBody
//! is the next step (the ACK / AckVector internals need their exact field
//! splits pinned down first).

use bitflags::bitflags;
use ironrdp_core::{ensure_size, invalid_field_err, Decode, DecodeResult, ReadCursor};

/// Number of leading bytes the network-format transform reorders.
const PREFIX_REGION: usize = 8;

bitflags! {
    /// RDP-UDP2 header flags (low 12 bits of the 16-bit header word, MS-RDPEUDP2
    /// 2.2.1.1). Values cross-checked with FreeRDP's dissector. `from_bits_retain`
    /// keeps any undocumented bits a real client sets (e.g. 0x200 is observed in
    /// captured data packets but isn't in the documented set).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Eudp2Flags: u16 {
        /// ACK payload present (mutually exclusive with `ACKVEC`).
        const ACK = 0x001;
        /// DataHeader + DataBody present.
        const DATA = 0x004;
        /// ACK Vector payload present (mutually exclusive with `ACK`).
        const ACKVEC = 0x008;
        /// AckOfAcks payload present.
        const AOA = 0x010;
        /// OverheadSize payload present.
        const OVERHEAD = 0x040;
        /// DelayAckInfo payload present.
        const DELAYACK = 0x100;
    }
}

/// Undo the RDP-UDP2 network-format transform: swap wire byte 0 and byte 7 so
/// the `PacketPrefixByte` sits at offset 0 and the real header at offset 1. The
/// transform is its own inverse, so [`wrap_packet`] re-applies the same swap.
///
/// Returns `None` if the packet is shorter than the 8-byte reordered region (a
/// `Short_Packet_Length`-encoded tiny packet, handled separately).
fn reorder_first_8(packet: &[u8]) -> Option<Vec<u8>> {
    if packet.len() < PREFIX_REGION {
        return None;
    }
    let mut out = packet.to_vec();
    out.swap(0, 7);
    Some(out)
}

/// Convert a wire RDP-UDP2 packet to its logical (unwrapped) form.
pub fn unwrap_packet(wire: &[u8]) -> Option<Vec<u8>> {
    reorder_first_8(wire)
}

/// Convert a logical RDP-UDP2 packet to wire form (same swap — it's an involution).
pub fn wrap_packet(logical: &[u8]) -> Option<Vec<u8>> {
    reorder_first_8(logical)
}

/// The `PacketPrefixByte` (MS-RDPEUDP2 2.2.1.3), at offset 0 of the unwrapped
/// packet. Encoded as `(short_packet_length << 5) | (packet_type_index << 1) | reserved`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketPrefix {
    /// 0 = a valid RDP-UDP2 packet follows; 8 = a dummy packet (ignore contents,
    /// loss does not trigger retransmit).
    pub packet_type_index: u8,
    /// The packet length if < 7 bytes, else 7.
    pub short_packet_length: u8,
}

impl PacketPrefix {
    fn from_byte(b: u8) -> Self {
        Self {
            packet_type_index: (b >> 1) & 0x0f,
            short_packet_length: (b >> 5) & 0x07,
        }
    }

    /// A dummy packet (`Packet_Type_Index == 8`): treated as a normal datagram by
    /// the transport, but its contents are ignored by higher layers and its loss
    /// is never retransmitted.
    pub fn is_dummy(&self) -> bool {
        self.packet_type_index == 8
    }
}

/// The mandatory RDP-UDP2 base header: the prefix byte plus the flags / window
/// word. The optional sub-headers indicated by [`Eudp2Header::flags`] follow it
/// (parsed by a later layer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Eudp2Header {
    pub prefix: PacketPrefix,
    pub flags: Eudp2Flags,
    /// `log2` of the receive window in MTUs (window = `(1 << log_window) * MTU`).
    pub log_window: u8,
}

impl Eudp2Header {
    /// Decode from an **unwrapped** packet (call [`unwrap_packet`] on the wire
    /// bytes first).
    pub fn decode(unwrapped: &[u8]) -> DecodeResult<Self> {
        let mut src = ReadCursor::new(unwrapped);
        Self::decode_cursor(&mut src)
    }

    fn decode_cursor(src: &mut ReadCursor<'_>) -> DecodeResult<Self> {
        ensure_size!(in: src, size: 3); // prefix(1) + flags word(2)
        let prefix = PacketPrefix::from_byte(src.read_u8());
        let word = src.read_u16(); // little-endian
        if prefix.packet_type_index != 0 && prefix.packet_type_index != 8 {
            return Err(invalid_field_err!(
                "PacketPrefixByte",
                "Packet_Type_Index must be 0 or 8"
            ));
        }
        Ok(Self {
            prefix,
            flags: Eudp2Flags::from_bits_retain(word & 0x0fff),
            log_window: ((word >> 12) & 0x0f) as u8,
        })
    }
}

impl Decode<'_> for Eudp2Header {
    fn decode(src: &mut ReadCursor<'_>) -> DecodeResult<Self> {
        Self::decode_cursor(src)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unwrap_is_an_involution() {
        let wire = [10u8, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20];
        let unwrapped = unwrap_packet(&wire).unwrap();
        // byte 0 and 7 swapped.
        assert_eq!(unwrapped[0], 17);
        assert_eq!(unwrapped[7], 10);
        assert_eq!(unwrapped[1..7], wire[1..7]);
        assert_eq!(unwrapped[8..], wire[8..]);
        // wrap(unwrap(x)) == x.
        assert_eq!(wrap_packet(&unwrapped).unwrap(), wire);
    }

    #[test]
    fn too_short_returns_none() {
        assert!(unwrap_packet(&[0, 1, 2, 3]).is_none());
    }

    // The following three are REAL Microsoft-client RDP-UDP2 packets captured
    // against a Windows RDP server (the EUDP2 data path), validating the
    // network-format transform + header decode against the wire.

    #[test]
    fn decodes_real_data_packet_pkt3() {
        // Client data packet carrying a TLS ClientHello (DATA|AOA|DELAYACK).
        let wire = [
            0x00, 0x14, 0xf3, 0x01, 0x14, 0x00, 0x64, 0xe0, // first 8 (prefix at offset 7)
            0x64, 0x00, 0x01, 0x00, 0x16, 0x03, 0x01,
            0x01, // ... DataBody begins 0x16 03 01 (TLS)
        ];
        let u = unwrap_packet(&wire).unwrap();
        let h = Eudp2Header::decode(&u).unwrap();
        assert_eq!(h.prefix.packet_type_index, 0);
        assert_eq!(h.prefix.short_packet_length, 7);
        assert!(h
            .flags
            .contains(Eudp2Flags::DATA | Eudp2Flags::AOA | Eudp2Flags::DELAYACK));
        assert!(!h.flags.contains(Eudp2Flags::ACK));
        assert_eq!(h.log_window, 15);
    }

    #[test]
    fn decodes_real_data_packet_pkt5() {
        // Same TLS data, fewer sub-headers (DATA|AOA, no DELAYACK).
        let wire = [
            0x01, 0x14, 0xf2, 0x66, 0x00, 0x66, 0x00, 0xe0, //
            0x00, 0x16, 0x03, 0x01, 0x01, 0x9d, 0x01, 0x00, //
        ];
        let u = unwrap_packet(&wire).unwrap();
        let h = Eudp2Header::decode(&u).unwrap();
        assert_eq!(h.prefix.packet_type_index, 0);
        assert!(h.flags.contains(Eudp2Flags::DATA | Eudp2Flags::AOA));
        assert!(!h.flags.contains(Eudp2Flags::DELAYACK));
        assert_eq!(h.log_window, 15);
    }

    #[test]
    fn decodes_real_ack_packet() {
        // Pure-ish ACK (ACK|OVERHEAD), no DATA.
        let wire = [
            0x02, 0x41, 0xf0, 0x6a, 0x00, 0xc4, 0x04, 0xe0, //
            0x00, 0x02, 0xdf, 0x73, 0x05,
        ];
        let u = unwrap_packet(&wire).unwrap();
        let h = Eudp2Header::decode(&u).unwrap();
        assert_eq!(h.prefix.packet_type_index, 0);
        assert!(h.flags.contains(Eudp2Flags::ACK | Eudp2Flags::OVERHEAD));
        assert!(!h.flags.contains(Eudp2Flags::DATA));
        assert_eq!(h.log_window, 15);
    }
}
