//! MS-RDPEMT tunnel PDU wire structures (the multitransport "tunnel" that rides
//! the secured reliable RDPEUDP stream — TLS for `UdpFecR`).
//!
//! This is a *different* protocol from MS-RDPEUDP (the reliability/transport
//! layer below it): once the reliable byte-stream is up and TLS-secured, the
//! client and server exchange MS-RDPEMT tunnel PDUs **inside** that TLS channel
//! to create the tunnel, bind it to the TCP session via the security cookie, and
//! (later) carry migrated dynamic-channel data.
//!
//! **Unlike MS-RDPEUDP (big-endian), MS-RDPEMT multi-byte fields are
//! little-endian** — it's an RDP byte-stream protocol like MS-RDPBCGR. Section
//! references are to [MS-RDPEMT].
//!
//! Only the handshake PDUs macrdp needs are modelled: decode
//! [`TunnelCreateRequest`] (client→server) and encode [`TunnelCreateResponse`]
//! (server→client). `RDP_TUNNEL_DATA` (channel migration) is M5.
//!
//! [MS-RDPEMT]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpemt/

use ironrdp_core::{
    ensure_fixed_part_size, ensure_size, invalid_field_err, Decode, DecodeResult, Encode,
    EncodeResult, ReadCursor, WriteCursor,
};

/// `Action` field of [`TunnelHeader`] (MS-RDPEMT 2.2.1.1, low 4 bits of the
/// first byte) — RDPTUNNEL_ACTION_*.
pub mod action {
    /// RDPTUNNEL_ACTION_CREATEREQUEST — client→server, create the tunnel.
    pub const CREATE_REQUEST: u8 = 0x0;
    /// RDPTUNNEL_ACTION_CREATERESPONSE — server→client, tunnel result.
    pub const CREATE_RESPONSE: u8 = 0x1;
    /// RDPTUNNEL_ACTION_DATA — channel data carried over the tunnel (M5).
    pub const DATA: u8 = 0x2;
}

/// `HrResponse` success value (`S_OK`) for [`TunnelCreateResponse`].
pub const HR_S_OK: u32 = 0x0000_0000;

/// RDP_TUNNEL_HEADER (MS-RDPEMT 2.2.1.1) — prefixes every tunnel PDU.
///
/// On the wire: 1 byte = `Action` (low 4 bits) | `Flags` (high 4 bits), then a
/// little-endian `PayloadLength` (u16), then `HeaderLength` (u8), then
/// `HeaderLength - 4` bytes of optional sub-headers. The full PDU on the wire is
/// `HeaderLength + PayloadLength` bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TunnelHeader {
    /// `Action` — one of [`action`].
    pub action: u8,
    /// `Flags` — RDPTUNNEL_FLAG_* (high nibble; unused by the handshake).
    pub flags: u8,
    /// `PayloadLength` — bytes of payload following the (variable-length) header.
    pub payload_length: u16,
    /// `HeaderLength` — total header size including any sub-headers (min 4).
    pub header_length: u8,
}

impl TunnelHeader {
    const NAME: &'static str = "RDP_TUNNEL_HEADER";
    /// Action(+Flags) byte + PayloadLength(2) + HeaderLength(1).
    pub const FIXED_PART_SIZE: usize = 4;

    /// Total on-wire size of the whole tunnel PDU (header incl. sub-headers +
    /// payload). `None` if `header_length` is below the fixed minimum (malformed).
    pub fn total_len(&self) -> Option<usize> {
        if usize::from(self.header_length) < Self::FIXED_PART_SIZE {
            return None;
        }
        Some(usize::from(self.header_length) + usize::from(self.payload_length))
    }

    /// Header for a PDU with no sub-headers carrying `payload_length` bytes.
    pub fn simple(action: u8, payload_length: u16) -> Self {
        Self {
            action,
            flags: 0,
            payload_length,
            header_length: Self::FIXED_PART_SIZE as u8,
        }
    }
}

impl Encode for TunnelHeader {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        ensure_fixed_part_size!(in: dst);
        dst.write_u8((self.action & 0x0f) | ((self.flags & 0x0f) << 4));
        dst.write_u16(self.payload_length);
        dst.write_u8(self.header_length);
        Ok(())
    }

    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn size(&self) -> usize {
        Self::FIXED_PART_SIZE
    }
}

impl Decode<'_> for TunnelHeader {
    fn decode(src: &mut ReadCursor<'_>) -> DecodeResult<Self> {
        ensure_fixed_part_size!(in: src);
        let b0 = src.read_u8();
        let payload_length = src.read_u16();
        let header_length = src.read_u8();
        if usize::from(header_length) < Self::FIXED_PART_SIZE {
            return Err(invalid_field_err!(
                "HeaderLength",
                "below the 4-byte minimum"
            ));
        }
        // Skip any sub-headers (HeaderLength - 4). We don't model them yet.
        let sub_len = usize::from(header_length) - Self::FIXED_PART_SIZE;
        ensure_size!(in: src, size: sub_len);
        let _ = src.read_slice(sub_len);
        Ok(Self {
            action: b0 & 0x0f,
            flags: (b0 >> 4) & 0x0f,
            payload_length,
            header_length,
        })
    }
}

/// Frame a tunnel PDU from a (possibly partial) buffer: peek just the fixed
/// 4-byte header and return the **total on-wire length** of the PDU it starts
/// (`HeaderLength + PayloadLength`). `None` if fewer than 4 bytes are buffered
/// (wait for more) or the header is malformed (`HeaderLength < 4`). Lets a
/// stream reader detect when a whole PDU is present before decoding it, without
/// the sub-header decode ambiguity (need-more-bytes vs malformed).
pub fn peek_pdu_len(buf: &[u8]) -> Option<usize> {
    if buf.len() < TunnelHeader::FIXED_PART_SIZE {
        return None;
    }
    let payload_length = u16::from_le_bytes([buf[1], buf[2]]);
    let header_length = buf[3];
    if usize::from(header_length) < TunnelHeader::FIXED_PART_SIZE {
        return None;
    }
    Some(usize::from(header_length) + usize::from(payload_length))
}

/// The `Action` of a buffered tunnel PDU (low 4 bits of the first byte), or
/// `None` if the buffer is empty.
pub fn peek_action(buf: &[u8]) -> Option<u8> {
    buf.first().map(|b| b & 0x0f)
}

/// RDP_TUNNEL_CREATEREQUEST (MS-RDPEMT 2.2.2.1) — client→server. Binds the UDP
/// tunnel to the main TCP session: `request_id` + `security_cookie` echo the
/// server's Initiate Multitransport Request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelCreateRequest {
    /// `RequestID` — matches the Initiate Multitransport Request's request id.
    pub request_id: u32,
    /// `SecurityCookie` — the 16-byte cookie from that request.
    pub security_cookie: [u8; 16],
}

impl TunnelCreateRequest {
    /// RequestID(4) + Reserved(4) + SecurityCookie(16).
    pub const PAYLOAD_SIZE: usize = 24;

    /// Decode from the **TLS-decrypted plaintext**, validating the tunnel header
    /// action. Returns the request plus the number of bytes consumed (the full
    /// PDU length), so a caller can parse back-to-back PDUs from a buffer.
    pub fn decode(buf: &[u8]) -> DecodeResult<(Self, usize)> {
        let mut src = ReadCursor::new(buf);
        let header = TunnelHeader::decode(&mut src)?;
        if header.action != action::CREATE_REQUEST {
            return Err(invalid_field_err!("Action", "not a CREATEREQUEST"));
        }
        if usize::from(header.payload_length) < Self::PAYLOAD_SIZE {
            return Err(invalid_field_err!(
                "PayloadLength",
                "too short for CREATEREQUEST"
            ));
        }
        ensure_size!(in: src, size: Self::PAYLOAD_SIZE);
        let request_id = src.read_u32();
        let _reserved = src.read_u32();
        let security_cookie = src.read_array::<16>();
        let consumed = header
            .total_len()
            .ok_or_else(|| invalid_field_err!("HeaderLength", "malformed"))?;
        Ok((
            Self {
                request_id,
                security_cookie,
            },
            consumed,
        ))
    }
}

/// RDP_TUNNEL_CREATERESPONSE (MS-RDPEMT 2.2.2.2) — server→client. `hr_response`
/// is an HRESULT; [`HR_S_OK`] accepts the tunnel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TunnelCreateResponse {
    pub hr_response: u32,
}

impl TunnelCreateResponse {
    const NAME: &'static str = "RDP_TUNNEL_CREATERESPONSE";
    /// HrResponse(4).
    pub const PAYLOAD_SIZE: usize = 4;

    /// A success (`S_OK`) response.
    pub fn ok() -> Self {
        Self {
            hr_response: HR_S_OK,
        }
    }

    /// Encode to bytes ready to write into the TLS tunnel.
    pub fn to_vec(&self) -> Vec<u8> {
        let total = TunnelHeader::FIXED_PART_SIZE + Self::PAYLOAD_SIZE;
        let mut out = vec![0u8; total];
        let mut dst = WriteCursor::new(&mut out);
        // Infallible: the buffer is sized exactly.
        let _ = self.encode(&mut dst);
        out
    }
}

impl Encode for TunnelCreateResponse {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        let header = TunnelHeader::simple(action::CREATE_RESPONSE, Self::PAYLOAD_SIZE as u16);
        header.encode(dst)?;
        ensure_size!(in: dst, size: Self::PAYLOAD_SIZE);
        dst.write_u32(self.hr_response);
        Ok(())
    }

    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn size(&self) -> usize {
        TunnelHeader::FIXED_PART_SIZE + Self::PAYLOAD_SIZE
    }
}

/// Wrap `higher_layer_data` in an RDP_TUNNEL_DATA PDU (MS-RDPEMT 2.2.2.3) ready
/// to write into the tunnel's TLS. After Soft-Sync, the server carries migrated
/// dynamic-channel traffic this way: `HigherLayerData` is the **same DRDYNVC PDU**
/// (e.g. a DYNVC DATA carrying an EGFX message) that would otherwise ride the
/// main connection's drdynvc SVC — only the transport wrapper differs.
pub fn encode_tunnel_data(higher_layer_data: &[u8]) -> Vec<u8> {
    let header = TunnelHeader::simple(
        action::DATA,
        u16::try_from(higher_layer_data.len()).unwrap_or(u16::MAX),
    );
    let mut out = vec![0u8; TunnelHeader::FIXED_PART_SIZE + higher_layer_data.len()];
    {
        let mut dst = WriteCursor::new(&mut out);
        // Infallible: the buffer is sized exactly for the fixed header.
        let _ = header.encode(&mut dst);
    }
    out[TunnelHeader::FIXED_PART_SIZE..].copy_from_slice(higher_layer_data);
    out
}

/// Extract the `HigherLayerData` of an inbound RDP_TUNNEL_DATA PDU (the bytes
/// after the variable-length [`TunnelHeader`]). Used when the client sends
/// migrated dynamic-channel data back over the tunnel. Returns a borrowed slice
/// into `pdu`. Errors if the header is malformed or the buffer is short.
pub fn tunnel_data_payload(pdu: &[u8]) -> DecodeResult<&[u8]> {
    let mut src = ReadCursor::new(pdu);
    let header = TunnelHeader::decode(&mut src)?;
    // `TunnelHeader::decode` consumes exactly `header_length` bytes (4 fixed +
    // sub-headers), so the payload starts there.
    let start = usize::from(header.header_length);
    let end = start
        .checked_add(usize::from(header.payload_length))
        .filter(|&e| e <= pdu.len())
        .ok_or_else(|| invalid_field_err!("PayloadLength", "exceeds buffer"))?;
    Ok(&pdu[start..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_request_decodes_with_expected_fields() {
        // RDP_TUNNEL_HEADER: action=0 (CREATEREQUEST), payloadLen=24 (LE), hdrLen=4
        // then RequestID(4 LE) + Reserved(4) + SecurityCookie(16). 28 bytes total —
        // exactly what real mstsc sends over the TLS tunnel (plaintext_len=28).
        #[rustfmt::skip]
        let bytes: [u8; 28] = [
            0x00, 0x18, 0x00, 0x04, // action|flags=0x00, payloadLength=0x0018=24, headerLength=4
            0x2a, 0x00, 0x00, 0x00, // RequestID = 42 (LE)
            0x00, 0x00, 0x00, 0x00, // Reserved
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08,
            0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10, // SecurityCookie
        ];
        let (req, consumed) = TunnelCreateRequest::decode(&bytes).unwrap();
        assert_eq!(consumed, 28);
        assert_eq!(req.request_id, 42);
        assert_eq!(
            req.security_cookie,
            [
                0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
                0x0f, 0x10
            ]
        );
    }

    #[test]
    fn create_request_skips_subheaders() {
        // headerLength=8 → 4 bytes of sub-headers before the payload.
        #[rustfmt::skip]
        let bytes: [u8; 32] = [
            0x00, 0x18, 0x00, 0x08, // headerLength=8 (4 bytes of sub-headers follow)
            0xaa, 0xbb, 0xcc, 0xdd, // sub-header bytes (ignored)
            0x07, 0x00, 0x00, 0x00, // RequestID = 7
            0x00, 0x00, 0x00, 0x00, // Reserved
            0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11,
            0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11,
        ];
        let (req, consumed) = TunnelCreateRequest::decode(&bytes).unwrap();
        assert_eq!(consumed, 32);
        assert_eq!(req.request_id, 7);
    }

    #[test]
    fn create_request_rejects_wrong_action() {
        let bytes: [u8; 28] = {
            let mut b = [0u8; 28];
            b[0] = action::CREATE_RESPONSE; // wrong action
            b[1] = 0x18;
            b[3] = 0x04;
            b
        };
        assert!(TunnelCreateRequest::decode(&bytes).is_err());
    }

    #[test]
    fn tunnel_data_wraps_and_unwraps_higher_layer() {
        let payload = b"a drdynvc DATA pdu carrying EGFX";
        let pdu = encode_tunnel_data(payload);
        // action|flags=0x02 (DATA), payloadLength=LE len, headerLength=4.
        assert_eq!(pdu[0], action::DATA);
        assert_eq!(u16::from_le_bytes([pdu[1], pdu[2]]) as usize, payload.len());
        assert_eq!(pdu[3], TunnelHeader::FIXED_PART_SIZE as u8);
        assert_eq!(&pdu[4..], payload);
        // Round-trips back to the same HigherLayerData.
        assert_eq!(tunnel_data_payload(&pdu).unwrap(), payload);
    }

    #[test]
    fn tunnel_data_payload_rejects_overlong_length() {
        // headerLength=4, payloadLength=99 but only 2 payload bytes present.
        let bad = [action::DATA, 0x63, 0x00, 0x04, 0xaa, 0xbb];
        assert!(tunnel_data_payload(&bad).is_err());
    }

    #[test]
    fn create_response_encodes_s_ok() {
        let bytes = TunnelCreateResponse::ok().to_vec();
        // action|flags=0x01 (CREATERESPONSE), payloadLength=4 (LE), headerLength=4,
        // HrResponse=0 (S_OK).
        assert_eq!(bytes, vec![0x01, 0x04, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn create_response_round_trips_through_request_shape() {
        // The header our response emits must itself decode as a well-formed
        // tunnel header with the CREATERESPONSE action.
        let bytes = TunnelCreateResponse::ok().to_vec();
        let mut src = ReadCursor::new(&bytes);
        let header = TunnelHeader::decode(&mut src).unwrap();
        assert_eq!(header.action, action::CREATE_RESPONSE);
        assert_eq!(header.payload_length, 4);
        assert_eq!(header.total_len(), Some(8));
    }
}
