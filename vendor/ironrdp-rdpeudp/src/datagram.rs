//! Whole-datagram assembly/parsing for RDPEUDP **v1**: composes the [`pdu`]
//! primitives (FEC header + optional SYN / ACK-vector / source sections) into a
//! single datagram and back, driven by the [`FecFlags`].
//!
//! This is the wire layer the reliability state machine ([`crate::state`]) emits
//! and consumes. It deliberately models only what that machine needs (SYN /
//! SYN+ACK / data / ack), not every optional MS-RDPEUDP section (ACK-of-acks and
//! FEC payloads are not produced). The RDPEUDP2 (`0x0101`) data framing is a
//! separate, spike-gated codec — see the crate docs.
//!
//! [`pdu`]: crate::pdu
//! [`FecFlags`]: crate::pdu::FecFlags

use ironrdp_core::{
    decode_cursor, encode_cursor, DecodeResult, EncodeResult, ReadCursor, WriteCursor,
};

use crate::pdu::{
    AckVectorHeader, CorrelationId, FecFlags, FecHeader, SourcePayloadHeader, SynData, SynDataEx,
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
    /// A SYN (or SYN+ACK, when `ack_vector` is `Some`) datagram: carries SYNDATA
    /// and optionally SYNEX (version negotiation). `snSourceAck` is the caller's
    /// (use `-1` for an initial SYN).
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

        let ack_vector = if flags.contains(FecFlags::ACK) {
            Some(decode_cursor::<AckVectorHeader>(&mut cursor)?)
        } else {
            None
        };

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
                Some(decode_cursor::<SynDataEx>(&mut cursor)?)
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

    #[test]
    fn pure_ack_round_trips() {
        let d = Datagram::ack(7, 64, empty_ack());
        let bytes = d.encode().unwrap();
        let back = Datagram::decode(&bytes).unwrap();
        assert_eq!(back, d);
        assert!(back.source.is_none() && back.syn.is_none());
    }
}
