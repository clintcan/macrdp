//! MS-RDPEDYC **Soft-Sync** PDUs — the drdynvc-level negotiation that moves
//! dynamic virtual channels (e.g. EGFX) from the main TCP connection onto a
//! multitransport tunnel.
//!
//! Soft-Sync is technically part of MS-RDPEDYC (the dynamic-VC protocol), not
//! MS-RDPEUDP/MS-RDPEMT, but it's the piece that makes the UDP multitransport
//! tunnel actually carry channel data, so its small codec lives alongside the
//! rest of the sans-I/O multitransport wire types. **Both PDUs travel over the
//! DRDYNVC static channel on the MAIN (TCP) RDP connection**, NOT over the UDP
//! tunnel — only the channel *data* (after the switch) rides the tunnel as
//! [`crate::emt::encode_tunnel_data`].
//!
//! Flow (server perspective): after the tunnel is created (and the client's
//! Initiate Multitransport Response has arrived), the server sends a
//! [`SoftSyncRequest`] over drdynvc listing which DVCs move to which tunnel; the
//! client replies with a Soft-Sync Response ([`decode_soft_sync_response`]) and
//! both ends then read/write those DVCs' data on the tunnel.
//!
//! **Little-endian** (an RDP byte-stream protocol). Section references are to
//! [MS-RDPEDYC] 2.2.5 + 3.1.5.1.1.
//!
//! [MS-RDPEDYC]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpedyc/

use ironrdp_core::{invalid_field_err, DecodeResult, ReadCursor};

/// DRDYNVC `Cmd` (high 4 bits of the first byte) for a Soft-Sync Request.
pub const CMD_SOFT_SYNC_REQUEST: u8 = 0x08;
/// DRDYNVC `Cmd` (high 4 bits of the first byte) for a Soft-Sync Response.
pub const CMD_SOFT_SYNC_RESPONSE: u8 = 0x09;

/// `SOFT_SYNC_TCP_FLUSHED` — no more data for the listed DVCs over TCP. MUST be
/// set in a request.
pub const SOFT_SYNC_TCP_FLUSHED: u16 = 0x01;
/// `SOFT_SYNC_CHANNEL_LIST_PRESENT` — one or more channel lists follow.
pub const SOFT_SYNC_CHANNEL_LIST_PRESENT: u16 = 0x02;

/// `TUNNELTYPE_UDPFECR` — the reliable-UDP multitransport tunnel.
pub const TUNNELTYPE_UDPFECR: u32 = 0x0000_0001;
/// `TUNNELTYPE_UDPFECL` — the lossy-UDP multitransport tunnel.
pub const TUNNELTYPE_UDPFECL: u32 = 0x0000_0003;

/// DYNVC_SOFT_SYNC_CHANNEL_LIST (MS-RDPEDYC 3.1.5.1.1) — the DVCs that will have
/// their data written by the server on one tunnel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoftSyncChannelList {
    /// `TunnelType` — e.g. [`TUNNELTYPE_UDPFECR`].
    pub tunnel_type: u32,
    /// `ListOfDVCIds` — the drdynvc channel ids that move to this tunnel.
    pub channel_ids: Vec<u32>,
}

impl SoftSyncChannelList {
    fn encoded_len(&self) -> usize {
        4 + 2 + self.channel_ids.len() * 4 // TunnelType + NumberOfDVCs + ids
    }

    fn encode_into(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.tunnel_type.to_le_bytes());
        out.extend_from_slice(&(self.channel_ids.len() as u16).to_le_bytes());
        for id in &self.channel_ids {
            out.extend_from_slice(&id.to_le_bytes());
        }
    }
}

/// DYNVC_SOFT_SYNC_REQUEST (MS-RDPEDYC 2.2.5.1) — server→client over drdynvc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoftSyncRequest {
    /// One channel list per tunnel the server will write DVC data on.
    pub channel_lists: Vec<SoftSyncChannelList>,
}

impl SoftSyncRequest {
    /// A request that moves `channel_ids` onto the reliable-UDP tunnel. Sets
    /// `SOFT_SYNC_TCP_FLUSHED` (required) and, when there are channels,
    /// `SOFT_SYNC_CHANNEL_LIST_PRESENT`.
    pub fn switch_to_udpfecr(channel_ids: Vec<u32>) -> Self {
        Self {
            channel_lists: vec![SoftSyncChannelList {
                tunnel_type: TUNNELTYPE_UDPFECR,
                channel_ids,
            }],
        }
    }

    /// Encode ready to write onto the drdynvc SVC.
    pub fn to_vec(&self) -> Vec<u8> {
        let mut flags = SOFT_SYNC_TCP_FLUSHED;
        if self.channel_lists.iter().any(|l| !l.channel_ids.is_empty()) {
            flags |= SOFT_SYNC_CHANNEL_LIST_PRESENT;
        }
        let lists_len: usize = self.channel_lists.iter().map(|l| l.encoded_len()).sum();
        // Length counts Length(4) + Flags(2) + NumberOfTunnels(2) + lists.
        let length = (4 + 2 + 2 + lists_len) as u32;

        let mut out = Vec::with_capacity(2 + length as usize);
        out.push(CMD_SOFT_SYNC_REQUEST << 4); // cbId=0, Sp=0, Cmd high nibble
        out.push(0x00); // Pad
        out.extend_from_slice(&length.to_le_bytes());
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(&(self.channel_lists.len() as u16).to_le_bytes());
        for list in &self.channel_lists {
            list.encode_into(&mut out);
        }
        out
    }
}

/// DYNVC_SOFT_SYNC_RESPONSE (MS-RDPEDYC 2.2.5.2) — client→server over drdynvc.
/// Lists the tunnel types the client will write DVC data on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoftSyncResponse {
    /// `TunnelsToSwitch` — the tunnel types (e.g. [`TUNNELTYPE_UDPFECR`]).
    pub tunnels: Vec<u32>,
}

/// Decode a DYNVC_SOFT_SYNC_RESPONSE from a drdynvc PDU. Validates the `Cmd`
/// nibble (`0x09`). Note `NumberOfTunnels` is **4 bytes** here (it's 2 bytes in
/// the request — an asymmetry in the spec).
pub fn decode_soft_sync_response(buf: &[u8]) -> DecodeResult<SoftSyncResponse> {
    let mut src = ReadCursor::new(buf);
    if src.len() < 2 + 4 {
        return Err(invalid_field_err!("DYNVC_SOFT_SYNC_RESPONSE", "too short"));
    }
    let b0 = src.read_u8();
    let cmd = (b0 >> 4) & 0x0f;
    if cmd != CMD_SOFT_SYNC_RESPONSE {
        return Err(invalid_field_err!("Cmd", "not a Soft-Sync Response"));
    }
    let _pad = src.read_u8();
    let number_of_tunnels = src.read_u32();
    let avail = (src.len() / 4) as u32;
    if number_of_tunnels > avail {
        return Err(invalid_field_err!(
            "NumberOfTunnels",
            "exceeds available bytes"
        ));
    }
    let mut tunnels = Vec::with_capacity(number_of_tunnels as usize);
    for _ in 0..number_of_tunnels {
        tunnels.push(src.read_u32());
    }
    Ok(SoftSyncResponse { tunnels })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_switch_to_udpfecr_encodes_spec_bytes() {
        // One channel list, channel id 0x0007, moving to UDPFECR.
        let req = SoftSyncRequest::switch_to_udpfecr(vec![0x0007]);
        let bytes = req.to_vec();
        #[rustfmt::skip]
        let expected = vec![
            0x80,                   // cbId=0|Sp=0|Cmd=0x08 (Soft-Sync Request)
            0x00,                   // Pad
            0x12, 0x00, 0x00, 0x00, // Length = 18 = 4 + 2 + 2 + (4+2+4)
            0x03, 0x00,             // Flags = TCP_FLUSHED|CHANNEL_LIST_PRESENT
            0x01, 0x00,             // NumberOfTunnels = 1
            0x01, 0x00, 0x00, 0x00, // TunnelType = TUNNELTYPE_UDPFECR
            0x01, 0x00,             // NumberOfDVCs = 1
            0x07, 0x00, 0x00, 0x00, // ListOfDVCIds[0] = 7
        ];
        assert_eq!(bytes, expected);
    }

    #[test]
    fn request_with_no_channels_omits_list_present_flag() {
        let req = SoftSyncRequest {
            channel_lists: vec![SoftSyncChannelList {
                tunnel_type: TUNNELTYPE_UDPFECR,
                channel_ids: vec![],
            }],
        };
        let bytes = req.to_vec();
        // Flags at offset 6..8 — only TCP_FLUSHED (0x01), no LIST_PRESENT.
        assert_eq!(
            u16::from_le_bytes([bytes[6], bytes[7]]),
            SOFT_SYNC_TCP_FLUSHED
        );
    }

    #[test]
    fn response_decodes_one_tunnel() {
        #[rustfmt::skip]
        let bytes = [
            0x90,                   // Cmd=0x09 (Soft-Sync Response)
            0x00,                   // Pad
            0x01, 0x00, 0x00, 0x00, // NumberOfTunnels = 1 (4 bytes!)
            0x01, 0x00, 0x00, 0x00, // TunnelsToSwitch[0] = UDPFECR
        ];
        let resp = decode_soft_sync_response(&bytes).unwrap();
        assert_eq!(resp.tunnels, vec![TUNNELTYPE_UDPFECR]);
    }

    #[test]
    fn response_rejects_wrong_cmd_and_overlong_count() {
        let wrong_cmd = [0x80, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00];
        assert!(decode_soft_sync_response(&wrong_cmd).is_err());

        // NumberOfTunnels=9 but no tunnel bytes present.
        let overlong = [0x90, 0x00, 0x09, 0x00, 0x00, 0x00];
        assert!(decode_soft_sync_response(&overlong).is_err());
    }
}
