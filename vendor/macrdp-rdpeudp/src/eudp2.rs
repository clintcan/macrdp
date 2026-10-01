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
//! M-eudp2 step 1: the transform + the `PacketPrefixByte` + the base
//! `Eudp2Header`. Step 2 ([`Eudp2Packet::parse`]): walking the ordered optional
//! sub-headers to extract the `DataBody` — the reliable byte-stream payload that
//! feeds the [`crate::state`] reliability machine (and, above it, rustls). The
//! sub-header field splits are taken from the same FreeRDP dissector and
//! validated against the same capture (see the `parse_real_*` tests).
//!
//! # Sub-header order (the non-obvious split)
//!
//! After the base header the sub-headers appear in this fixed order, each gated
//! by its flag: **ACK, OverheadSize, DelayAckInfo, AckOfAcks, Data(SeqNumber),
//! AckVector, Data(ChannelSeqNumber)+DataBody**. The `DataHeader` is **split**:
//! its 2-byte `SeqNumber` is read *before* the AckVector, but its 2-byte
//! `ChannelSeqNumber` (and then the body) come *after* it.

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

/// The ACK sub-header (flag [`Eudp2Flags::ACK`]) — the receiver's cumulative ack
/// plus a short run of per-packet delayed-ack deltas. Layout (all little-endian):
/// `AckSeq(2) AckTimestamp(3) SendTimeGap(1) [NumDelayedAcks:4 | DelayedTimeScale:4](1)`
/// then `NumDelayedAcks` bytes of `DelayedAck` deltas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AckSubheader<'a> {
    /// Highest in-order sequence number the sender has received (cumulative ack).
    pub ack_seq: u16,
    /// 24-bit timestamp the ack was generated at (microseconds, sender clock).
    pub ack_timestamp: u32,
    /// Gap between this ack and the previous one.
    pub send_time_gap: u8,
    /// Number of `delayed_acks` delta bytes that follow (low nibble of the combined byte).
    pub num_delayed_acks: u8,
    /// `log2` scale applied to each `delayed_acks` delta (high nibble).
    pub delayed_time_scale: u8,
    /// Per-packet delayed-ack timestamp deltas (one byte each, scaled by `delayed_time_scale`).
    pub delayed_acks: &'a [u8],
}

/// The DelayAckInfo sub-header (flag [`Eudp2Flags::DELAYACK`]): the sender telling
/// the receiver how to batch its acks. `MaxDelayedAcks(1) DelayedAckTimeoutMs(2 LE)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DelayAckInfo {
    pub max_delayed_acks: u8,
    pub delayed_ack_timeout_ms: u16,
}

/// The AckVector sub-header (flag [`Eudp2Flags::ACKVEC`]): a coded
/// received/lost run-length/bitmap vector starting at `base_seq`, optionally
/// preceded by a 4-byte timestamp. `BaseSeq(2) [HaveTs:1 | CodedSize:7](1)
/// [Timestamp(4)]? CodedVector(CodedSize)`. The coded-vector bytes are kept raw
/// here (decoded run-by-run by the reliability layer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AckVec<'a> {
    /// Sequence number the first vector element refers to.
    pub base_seq: u16,
    /// Optional 4-byte timestamp (present iff the high bit of the size byte is set).
    pub timestamp: Option<u32>,
    /// Raw coded ack-vector bytes (`coded_ack_vec_size` of them).
    pub coded_vector: &'a [u8],
}

/// The DataHeader (flag [`Eudp2Flags::DATA`]). **Split on the wire**: `seq_number`
/// is read before the AckVector sub-header, `channel_seq_number` after it (and a
/// dummy packet, `Packet_Type_Index == 8`, carries only `seq_number` — no channel
/// number, no body).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataHeader {
    pub seq_number: u16,
    /// `None` for a dummy packet (no body follows).
    pub channel_seq_number: Option<u16>,
}

/// A fully-parsed RDP-UDP2 packet: the base header, whichever optional
/// sub-headers its flags selected, and the `DataBody` slice (empty when there is
/// no `DATA` payload or for a dummy packet). Borrows the unwrapped buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Eudp2Packet<'a> {
    pub header: Eudp2Header,
    pub ack: Option<AckSubheader<'a>>,
    pub overhead_size: Option<u8>,
    pub delay_ack_info: Option<DelayAckInfo>,
    pub ack_of_acks: Option<u16>,
    pub ack_vec: Option<AckVec<'a>>,
    pub data: Option<DataHeader>,
    /// The reliable byte-stream payload (TLS records, once secured). Empty if the
    /// packet carries no data.
    pub body: &'a [u8],
}

/// Read a 24-bit little-endian unsigned integer (RDP-UDP2 timestamps are 3 bytes).
fn read_u24_le(src: &mut ReadCursor<'_>) -> u32 {
    let b = src.read_array::<3>();
    u32::from_le_bytes([b[0], b[1], b[2], 0])
}

impl<'a> Eudp2Packet<'a> {
    /// Parse a complete RDP-UDP2 packet from an **unwrapped** buffer (run
    /// [`unwrap_packet`] on the wire bytes first). Walks the sub-headers in wire
    /// order and returns the remaining bytes as [`Eudp2Packet::body`].
    pub fn parse(unwrapped: &'a [u8]) -> DecodeResult<Self> {
        let mut src = ReadCursor::new(unwrapped);
        let header = <Eudp2Header as Decode<'_>>::decode(&mut src)?;
        let flags = header.flags;

        let ack = if flags.contains(Eudp2Flags::ACK) {
            // AckSeq(2) AckTs(3) SendTimeGap(1) combined(1) = 7 fixed bytes.
            ensure_size!(in: src, size: 7);
            let ack_seq = src.read_u16();
            let ack_timestamp = read_u24_le(&mut src);
            let send_time_gap = src.read_u8();
            let combined = src.read_u8();
            let num_delayed_acks = combined & 0x0f;
            let delayed_time_scale = (combined >> 4) & 0x0f;
            ensure_size!(in: src, size: usize::from(num_delayed_acks));
            let delayed_acks = src.read_slice(usize::from(num_delayed_acks));
            Some(AckSubheader {
                ack_seq,
                ack_timestamp,
                send_time_gap,
                num_delayed_acks,
                delayed_time_scale,
                delayed_acks,
            })
        } else {
            None
        };

        let overhead_size = if flags.contains(Eudp2Flags::OVERHEAD) {
            ensure_size!(in: src, size: 1);
            Some(src.read_u8())
        } else {
            None
        };

        let delay_ack_info = if flags.contains(Eudp2Flags::DELAYACK) {
            ensure_size!(in: src, size: 3);
            let max_delayed_acks = src.read_u8();
            let delayed_ack_timeout_ms = src.read_u16();
            Some(DelayAckInfo {
                max_delayed_acks,
                delayed_ack_timeout_ms,
            })
        } else {
            None
        };

        let ack_of_acks = if flags.contains(Eudp2Flags::AOA) {
            ensure_size!(in: src, size: 2);
            Some(src.read_u16())
        } else {
            None
        };

        // DataHeader is split: SeqNumber here, ChannelSeqNumber after the AckVector.
        let is_dummy = header.prefix.is_dummy();
        let data_seq = if flags.contains(Eudp2Flags::DATA) {
            ensure_size!(in: src, size: 2);
            Some(src.read_u16())
        } else {
            None
        };

        let ack_vec = if flags.contains(Eudp2Flags::ACKVEC) {
            ensure_size!(in: src, size: 3);
            let base_seq = src.read_u16();
            let size_byte = src.read_u8();
            let coded_ack_vec_size = usize::from(size_byte & 0x7f);
            let have_ts = size_byte & 0x80 != 0;
            let timestamp = if have_ts {
                ensure_size!(in: src, size: 4);
                Some(src.read_u32())
            } else {
                None
            };
            ensure_size!(in: src, size: coded_ack_vec_size);
            let coded_vector = src.read_slice(coded_ack_vec_size);
            Some(AckVec {
                base_seq,
                timestamp,
                coded_vector,
            })
        } else {
            None
        };

        // Second half of the DataHeader + the body (skipped entirely for a dummy).
        let (data, body): (Option<DataHeader>, &[u8]) = match (data_seq, is_dummy) {
            (Some(seq_number), false) => {
                ensure_size!(in: src, size: 2);
                let channel_seq_number = src.read_u16();
                let body = src.remaining();
                (
                    Some(DataHeader {
                        seq_number,
                        channel_seq_number: Some(channel_seq_number),
                    }),
                    body,
                )
            }
            (Some(seq_number), true) => (
                Some(DataHeader {
                    seq_number,
                    channel_seq_number: None,
                }),
                &[],
            ),
            (None, _) => (None, &[]),
        };

        Ok(Self {
            header,
            ack,
            overhead_size,
            delay_ack_info,
            ack_of_acks,
            ack_vec,
            data,
            body,
        })
    }

    /// Project this packet onto the framing-neutral fields the reliability layer
    /// acts on — the **adapter seam** that lets a (framing-agnostic) transport
    /// state machine consume RDP-UDP2 without knowing its wire layout. The v1
    /// path exposes the analogous fields directly on [`crate::datagram::Datagram`]
    /// (`fec.snd_source_ack`, `fec.recv_window`, `source`).
    ///
    /// This is the **decode (inbound) half** of the adapter — capture-validated
    /// (see `inbound_view_*` tests). Wiring it into [`crate::state::RdpeudpState`]
    /// (which then needs EUDP2's 16-bit sequence space and the symmetric *encode*
    /// half) is deferred to the on-wire milestone, where a bidirectional V3
    /// capture can validate what a real client accepts as our output.
    pub fn inbound_view(&self) -> Eudp2Inbound<'a> {
        Eudp2Inbound {
            // ACK sub-header's AckSeq is the peer's cumulative ack point (the
            // dissector tracks it as `senderLow`). AckVector (mutually exclusive
            // with ACK) is *selective* info, exposed separately via `self.ack_vec`.
            cumulative_ack: self.ack.map(|a| a.ack_seq),
            peer_log_window: self.header.log_window,
            // A dummy packet (`Packet_Type_Index == 8`) carries no deliverable
            // body, so `data` is None even though the DATA flag may be set.
            data: match &self.data {
                Some(d) if d.channel_seq_number.is_some() => Some((d.seq_number, self.body)),
                _ => None,
            },
        }
    }
}

/// The framing-neutral, reliability-relevant projection of an inbound RDP-UDP2
/// packet (see [`Eudp2Packet::inbound_view`]). Holds only what a transport state
/// machine needs, lifted out of the EUDP2 wire layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Eudp2Inbound<'a> {
    /// The peer's cumulative ack (highest in-order transport sequence it has
    /// received), if this packet carried an ACK sub-header. EUDP2 sequences are
    /// 16-bit, so callers compare in 16-bit wrapping space.
    pub cumulative_ack: Option<u16>,
    /// The peer's advertised receive window as `log2(window / MTU)` — the window
    /// is `(1 << peer_log_window)` MTUs.
    pub peer_log_window: u8,
    /// A deliverable data segment `(transport seq number, body)` — present iff
    /// the packet carries a non-dummy DATA payload.
    pub data: Option<(u16, &'a [u8])>,
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

    // Full sub-header walk against the same three real captured packets — these
    // pin the DelayAckInfo/AckOfAcks/Data(split)/Ack sizes to the wire and prove
    // the DataBody (TLS) is extracted at the right offset.

    #[test]
    fn parse_real_data_packet_pkt3() {
        // DATA|AOA|DELAYACK -> DelayAckInfo(3) + AckOfAcks(2) + DataSeq(2) +
        // ChannelSeq(2) + body. No ACK/OVERHEAD/ACKVEC.
        let wire = [
            0x00, 0x14, 0xf3, 0x01, 0x14, 0x00, 0x64, 0xe0, //
            0x64, 0x00, 0x01, 0x00, 0x16, 0x03, 0x01, 0x01,
        ];
        let u = unwrap_packet(&wire).unwrap();
        let p = Eudp2Packet::parse(&u).unwrap();
        assert!(p.ack.is_none());
        assert!(p.overhead_size.is_none());
        assert!(p.ack_vec.is_none());
        assert_eq!(
            p.delay_ack_info,
            Some(DelayAckInfo {
                max_delayed_acks: 0x01,
                delayed_ack_timeout_ms: 0x0014,
            })
        );
        assert_eq!(p.ack_of_acks, Some(0x0064));
        assert_eq!(
            p.data,
            Some(DataHeader {
                seq_number: 0x0064,
                channel_seq_number: Some(0x0001),
            })
        );
        // The body is the start of a TLS record (ClientHello fragment).
        assert_eq!(p.body, &[0x16, 0x03, 0x01, 0x01]);
    }

    #[test]
    fn parse_real_data_packet_pkt5() {
        // DATA|AOA (no DelayAck) -> AckOfAcks(2) + DataSeq(2) + ChannelSeq(2) + body.
        let wire = [
            0x01, 0x14, 0xf2, 0x66, 0x00, 0x66, 0x00, 0xe0, //
            0x00, 0x16, 0x03, 0x01, 0x01, 0x9d, 0x01, 0x00,
        ];
        let u = unwrap_packet(&wire).unwrap();
        let p = Eudp2Packet::parse(&u).unwrap();
        assert!(p.delay_ack_info.is_none());
        assert_eq!(p.ack_of_acks, Some(0x0066));
        assert_eq!(
            p.data,
            Some(DataHeader {
                seq_number: 0x0066,
                channel_seq_number: Some(0x0001),
            })
        );
        assert_eq!(p.body, &[0x16, 0x03, 0x01, 0x01, 0x9d, 0x01, 0x00]);
    }

    #[test]
    fn parse_real_ack_packet() {
        // ACK|OVERHEAD, no DATA. The ACK sub-header carries 2 delayed-ack deltas,
        // then the single OverheadSize byte lands exactly at end-of-packet.
        let wire = [
            0x02, 0x41, 0xf0, 0x6a, 0x00, 0xc4, 0x04, 0xe0, //
            0x00, 0x02, 0xdf, 0x73, 0x05,
        ];
        let u = unwrap_packet(&wire).unwrap();
        let p = Eudp2Packet::parse(&u).unwrap();
        let ack = p.ack.expect("ACK present");
        assert_eq!(ack.ack_seq, 0x006a);
        assert_eq!(ack.ack_timestamp, 0x0204c4); // 24-bit LE of c4 04 02
        assert_eq!(ack.send_time_gap, 0x00);
        assert_eq!(ack.num_delayed_acks, 2);
        assert_eq!(ack.delayed_time_scale, 0);
        assert_eq!(ack.delayed_acks, &[0xdf, 0x73]);
        assert_eq!(p.overhead_size, Some(0x05));
        assert!(p.data.is_none());
        assert!(p.body.is_empty());
    }

    // The adapter seam: the framing-neutral projection the reliability layer
    // consumes, validated against the same captured packets.

    #[test]
    fn inbound_view_of_real_data_packet() {
        let wire = [
            0x00, 0x14, 0xf3, 0x01, 0x14, 0x00, 0x64, 0xe0, //
            0x64, 0x00, 0x01, 0x00, 0x16, 0x03, 0x01, 0x01,
        ];
        let u = unwrap_packet(&wire).unwrap();
        let v = Eudp2Packet::parse(&u).unwrap().inbound_view();
        // No ACK sub-header on this data packet.
        assert_eq!(v.cumulative_ack, None);
        assert_eq!(v.peer_log_window, 15);
        assert_eq!(v.data, Some((0x0064, &[0x16, 0x03, 0x01, 0x01][..])));
    }

    #[test]
    fn inbound_view_of_real_ack_packet() {
        let wire = [
            0x02, 0x41, 0xf0, 0x6a, 0x00, 0xc4, 0x04, 0xe0, //
            0x00, 0x02, 0xdf, 0x73, 0x05,
        ];
        let u = unwrap_packet(&wire).unwrap();
        let v = Eudp2Packet::parse(&u).unwrap().inbound_view();
        // Cumulative ack point carried in the ACK sub-header; no data to deliver.
        assert_eq!(v.cumulative_ack, Some(0x006a));
        assert_eq!(v.data, None);
    }

    #[test]
    fn inbound_view_of_dummy_packet_has_no_data() {
        // Packet_Type_Index == 8 (dummy): DATA flag set but no body to deliver.
        // prefix byte: type=8 -> (8<<1)=0x10, len7 -> (7<<5)=0xe0 => 0xf0.
        let unwrapped = [
            0xf0, 0x04, 0x00, // prefix(type=8) + flags word (DATA, logwin 0)
            0x2a, 0x00, // DataHeader SeqNumber = 0x002a (dummy: no channel seq / body)
        ];
        let p = Eudp2Packet::parse(&unwrapped).unwrap();
        assert!(p.header.prefix.is_dummy());
        assert_eq!(
            p.data,
            Some(DataHeader {
                seq_number: 0x002a,
                channel_seq_number: None,
            })
        );
        assert_eq!(p.inbound_view().data, None);
    }

    #[test]
    fn parse_ack_vector_with_timestamp() {
        // Synthetic (no captured ACKVEC packet): exercises the AckVec branch with
        // the timestamp bit set. flags=ACKVEC, logwin=0.
        // prefix, flags-word(LE 0x0008), base_seq(0x0010), size byte 0x82
        // (coded size 2 + have-ts), ts(0x0a0b0c0d LE), vector[0xc1,0x80].
        let unwrapped = [
            0xe0, 0x08, 0x00, // prefix + flags word (ACKVEC, logwin 0)
            0x10, 0x00, // base_seq = 0x0010
            0x82, // have_ts | coded_size=2
            0x0d, 0x0c, 0x0b, 0x0a, // timestamp 0x0a0b0c0d (LE)
            0xc1, 0x80, // 2 coded-vector bytes
        ];
        let p = Eudp2Packet::parse(&unwrapped).unwrap();
        let v = p.ack_vec.expect("AckVec present");
        assert_eq!(v.base_seq, 0x0010);
        assert_eq!(v.timestamp, Some(0x0a0b_0c0d));
        assert_eq!(v.coded_vector, &[0xc1, 0x80]);
        assert!(p.data.is_none());
        assert!(p.body.is_empty());
    }
}
