//! Sans-I/O reliable RDPEUDP **v1** transport state machine.
//!
//! Feed it inbound datagrams + a monotonic millisecond clock; it returns the
//! application bytes to deliver (in order, exactly once) and the datagrams to
//! put on the wire. No sockets, no async — the caller owns I/O and the clock,
//! which makes the whole thing deterministically testable (two instances driven
//! against each other over an in-memory lossy channel; see the tests).
//!
//! # Scope (M2b-2)
//!
//! - Passive (server) **and** active (client) open, so two instances can talk.
//! - Reliable, in-order, de-duplicated delivery: the receiver buffers
//!   out-of-order datagrams and delivers contiguous runs; the sender uses the
//!   **cumulative** ACK (`snSourceAck`) to release in-flight packets and
//!   retransmits the unacked window on RTO. Outbound ACKs carry a populated
//!   `RDPUDP_ACK_VECTOR_HEADER` marking the in-order run `Received` — required
//!   for mstsc to retire datagrams (an empty vector is ignored). Selective NACK
//!   runs for out-of-order gaps are a later refinement.
//! - Fixed send window bounded by the peer's `uReceiveWindowSize`. No congestion
//!   control / FEC yet.
//!
//! The on-wire framing is v1 ([`crate::datagram`]); the algorithm here is
//! framing-agnostic and is what an RDPEUDP2 codec would later reuse.

use std::collections::{BTreeMap, VecDeque};

use crate::datagram::{Datagram, SourcePacket};
use crate::pdu::{
    AckVectorElement, AckVectorHeader, SourcePayloadHeader, UdpVersion, VectorElementState,
};

/// Which end of the connection this instance is. macrdp (the RDP server) is the
/// passive [`Role::Server`]; [`Role::Client`] exists so two instances can be
/// driven against each other in tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Active opener — sends the SYN.
    Client,
    /// Passive opener — waits for a SYN, replies SYN+ACK.
    Server,
}

/// Delivery policy for the source-packet data path (MS-RDPEUDP §1.3.2).
///
/// The SYN/SYN+ACK handshake is identical in both modes; only how source
/// payloads are received and sent differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeliveryMode {
    /// `UdpFecR` — reliable, in-order, de-duplicated: out-of-order datagrams are
    /// buffered and delivered as contiguous runs; our own data is retransmitted
    /// on RTO until cumulatively acked. The Phase-1 default.
    #[default]
    Reliable,
    /// `UdpFecL` — lossy: source payloads are **delivered on arrival** (no
    /// in-order head-of-line blocking — *the whole point*), and our own data is
    /// sent **once, never retransmitted**. The upper layer tolerates loss (DTLS
    /// records are independent and self-deduplicating; audio drops a stale frame
    /// for free), and loss *recovery* — where wanted — comes from FEC (P2.3), not
    /// retransmission. A reliable ordered stream head-of-line-blocks on its own
    /// loss exactly like TCP, which is why video-under-loss needs this mode.
    Lossy,
}

/// Per-connection configuration.
#[derive(Debug, Clone, Copy)]
pub struct Config {
    /// Advertised receive window, in datagrams (`uReceiveWindowSize`).
    pub recv_window: u16,
    /// Our MTU (1132..=1232 per spec); bounds the per-packet payload.
    pub mtu: u16,
    /// Our initial sequence number (the first source-packet sequence we send).
    pub initial_seq: u32,
    /// Retransmit timeout, milliseconds.
    pub rto_ms: u64,
    /// Reliable (`UdpFecR`) vs lossy (`UdpFecL`) delivery policy. Defaults to
    /// [`DeliveryMode::Reliable`] so existing callers are unchanged.
    pub mode: DeliveryMode,
    /// Lossy-only redundancy spike (P2.3 FEC pivot): emit each new source
    /// datagram **twice** (same sequence number, same bytes) so that on an
    /// independent-loss link of rate `p` a payload is lost only at `p²`. The
    /// peer's RDPEUDP receiver de-duplicates by sequence number (the same
    /// mechanism that drops a normal retransmit), so the upper layer never sees
    /// the copy — no double-play. A poor-man's 1+1 repetition code standing in
    /// for the structurally-unavailable Reed-Solomon FEC (modern Windows
    /// negotiates RDPUDP2, which has no FEC — see
    /// `docs/rdp-udp-multitransport-feasibility.md` "P2.3 FEC capture RESULT").
    /// Ignored unless `mode == Lossy`. Default `false` (every existing caller is
    /// byte-identical).
    pub duplicate_lossy_sends: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            recv_window: 64,
            mtu: 1232,
            initial_seq: 0,
            rto_ms: 300,
            mode: DeliveryMode::Reliable,
            duplicate_lossy_sends: false,
        }
    }
}

/// What a [`RdpeudpState::step`]/[`RdpeudpState::enqueue`] call produced.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct StepOutput {
    /// Application payloads to hand up, in order, each exactly once.
    pub delivered: Vec<Vec<u8>>,
    /// Encoded datagrams to transmit.
    pub to_send: Vec<Vec<u8>>,
    /// Diagnostics: how many in-flight **data** segments were retransmitted this
    /// step because their RTO elapsed. The state machine is sans-I/O and silent;
    /// the caller (the UDP listener) logs this so a lossy-link soak can confirm
    /// the recovery path is actually firing. Zero on a clean link.
    pub retransmits: usize,
    /// Diagnostics: the client SYN was retransmitted this step (RTO during the
    /// handshake). Server instances never set this (they don't send a SYN).
    pub syn_retransmit: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnState {
    /// Server, before any SYN.
    Listen,
    /// Client, SYN sent, awaiting SYN+ACK.
    SynSent,
    Established,
}

struct Unacked {
    seq: u32,
    payload: Vec<u8>,
    last_sent_ms: u64,
}

/// A reliable RDPEUDP v1 transport endpoint.
pub struct RdpeudpState {
    role: Role,
    state: ConnState,
    cfg: Config,

    // Negotiated from the peer's SYN / SYN+ACK.
    peer_recv_window: u16,
    /// The peer's initial sequence number (from its SYNDATA). The server echoes
    /// it as `snSourceAck` in the SYN+ACK (matching real Windows).
    peer_isn: u32,
    /// The RDP-UDP version negotiated from the client's SYNEX (defaults to
    /// [`UdpVersion::V2`] if the client sent no SYNEX). A `V3` result means the
    /// data path upgrades to RDPEUDP2 framing after the handshake.
    negotiated_version: UdpVersion,

    // Send side.
    send_next: u32,
    out_queue: VecDeque<u8>,
    unacked: VecDeque<Unacked>,
    syn_last_sent_ms: u64,

    // Receive side.
    recv_next: u32,
    reorder: BTreeMap<u32, Vec<u8>>,
    need_ack: bool,
    have_peer_seq: bool,
}

/// `a <= b` in 32-bit wrapping sequence space (RFC 793 style).
fn seq_leq(a: u32, b: u32) -> bool {
    b.wrapping_sub(a) < 0x8000_0000
}

impl RdpeudpState {
    pub fn new(role: Role, cfg: Config) -> Self {
        Self {
            role,
            state: match role {
                Role::Client => ConnState::SynSent, // becomes active on `start`
                Role::Server => ConnState::Listen,
            },
            cfg,
            peer_recv_window: 1,
            peer_isn: 0,
            negotiated_version: UdpVersion::V2,
            // The SYN consumes one sequence number (it carries `initial_seq` as its
            // own seq), so the first *source* packet is `initial_seq + 1`. This
            // matches real Windows: a server SYN+ACK acks `client_ISN` for the SYN
            // alone, and the client's first data is `client_ISN + 1`. (Verified
            // against mstsc — without the +1 the receiver is off by one and buffers
            // every data packet forever; see `recv_next` init on the SYN below.)
            send_next: cfg.initial_seq.wrapping_add(1),
            out_queue: VecDeque::new(),
            unacked: VecDeque::new(),
            syn_last_sent_ms: 0,
            recv_next: 0,
            reorder: BTreeMap::new(),
            need_ack: false,
            have_peer_seq: false,
        }
    }

    pub fn is_established(&self) -> bool {
        self.state == ConnState::Established
    }

    /// The RDP-UDP version negotiated from the client's SYNEX. [`UdpVersion::V3`]
    /// means the data path should use RDPEUDP2 framing after the handshake.
    pub fn negotiated_version(&self) -> UdpVersion {
        self.negotiated_version
    }

    /// The next in-order source sequence number the receiver expects. Exposed for
    /// diagnostics (the listener logs it against an incoming data packet's seq to
    /// pin the post-handshake sequence convention against real clients).
    pub fn recv_next(&self) -> u32 {
        self.recv_next
    }

    /// Client: emit the initial SYN. Server: no-op (it waits for the SYN).
    pub fn start(&mut self, now_ms: u64) -> StepOutput {
        let mut out = StepOutput::default();
        if self.role == Role::Client {
            self.state = ConnState::SynSent;
            self.syn_last_sent_ms = now_ms;
            out.to_send.push(self.encode_syn());
        }
        self.pump(now_ms, &mut out);
        out
    }

    /// Queue application bytes for reliable delivery, then pump.
    pub fn enqueue(&mut self, now_ms: u64, data: &[u8]) -> StepOutput {
        self.out_queue.extend(data.iter().copied());
        let mut out = StepOutput::default();
        self.pump(now_ms, &mut out);
        out
    }

    /// Process an inbound datagram (or `None` for a timer-only tick), then pump.
    pub fn step(&mut self, now_ms: u64, inbound: Option<&[u8]>) -> StepOutput {
        let mut out = StepOutput::default();
        if let Some(bytes) = inbound {
            if let Ok(dg) = Datagram::decode(bytes) {
                self.handle(&dg, &mut out);
            }
            // Malformed datagrams are dropped (UDP — anything can arrive).
        }
        self.pump(now_ms, &mut out);
        out
    }

    fn handle(&mut self, dg: &Datagram, out: &mut StepOutput) {
        // Learn the peer's first sequence number from its SYN / SYN+ACK.
        if let Some(syn) = &dg.syn {
            self.peer_recv_window = self.peer_recv_window.max(dg.fec.recv_window).max(1);
            self.peer_isn = syn.initial_seq;
            // Negotiate the data-path version from the client's SYNEX (V3 = upgrade
            // to RDPEUDP2). A SYN with no SYNEX leaves the V2 default.
            if let Some(ex) = &dg.syn_ex {
                self.negotiated_version = ex.udp_version;
            }
            if !self.have_peer_seq {
                // The peer's SYN occupies `initial_seq`; its first source packet is
                // `initial_seq + 1` (the SYN consumes a sequence number, like TCP).
                self.recv_next = syn.initial_seq.wrapping_add(1);
                self.have_peer_seq = true;
            }
            match self.role {
                // Server: a (possibly duplicate) SYN — (re)send SYN+ACK and be established.
                Role::Server => {
                    self.state = ConnState::Established;
                    out.to_send.push(self.encode_syn_ack());
                }
                // Client: the SYN+ACK completes our open.
                Role::Client => {
                    if self.state == ConnState::SynSent {
                        self.state = ConnState::Established;
                    }
                }
            }
        } else {
            self.peer_recv_window = dg.fec.recv_window.max(1);
        }

        // Release packets the peer has cumulatively acknowledged.
        self.process_ack(dg.fec.snd_source_ack);

        // Take in a source payload.
        if let Some(src) = &dg.source {
            self.recv_source(src, out);
        }
    }

    fn process_ack(&mut self, snd_source_ack: i32) {
        let ack = snd_source_ack as u32;
        while let Some(front) = self.unacked.front() {
            // Drop everything up to and including the cumulative ack.
            if seq_leq(front.seq, ack) {
                self.unacked.pop_front();
            } else {
                break;
            }
        }
    }

    fn recv_source(&mut self, src: &SourcePacket, out: &mut StepOutput) {
        let seq = src.header.sn_source_start;
        if !self.have_peer_seq {
            // Data before we learned the peer's ISN: adopt it.
            self.recv_next = seq;
            self.have_peer_seq = true;
        }
        self.need_ack = true;

        if self.cfg.mode == DeliveryMode::Lossy {
            // Lossy: deliver on arrival, never buffer for ordering, never block on
            // a gap (the whole point). We don't dedup here — the upper layer (DTLS)
            // drops any duplicate/replayed record itself, and on a clean in-order
            // link there are no duplicates. `recv_next` still advances
            // monotonically to the highest seen seq + 1 so our cumulative ACK
            // reports forward progress; under loss it over-reports, but a lossy
            // sender never retransmits on that ACK, so it's harmless.
            out.delivered.push(src.payload.clone());
            if seq_leq(self.recv_next, seq) {
                self.recv_next = seq.wrapping_add(1);
            }
            return;
        }

        if seq == self.recv_next {
            out.delivered.push(src.payload.clone());
            self.recv_next = self.recv_next.wrapping_add(1);
            // Drain any contiguous buffered datagrams.
            while let Some(payload) = self.reorder.remove(&self.recv_next) {
                out.delivered.push(payload);
                self.recv_next = self.recv_next.wrapping_add(1);
            }
        } else if seq_leq(self.recv_next, seq) {
            // Future datagram within/after the window: buffer it (dedup by seq).
            self.reorder
                .entry(seq)
                .or_insert_with(|| src.payload.clone());
        }
        // else: already-delivered duplicate — ignore (but still re-ACK).
    }

    /// Retransmit timed-out packets, send new data within the window, and emit a
    /// trailing pure ACK if one is still owed.
    fn pump(&mut self, now_ms: u64, out: &mut StepOutput) {
        // Client SYN retransmission.
        if self.state == ConnState::SynSent
            && now_ms.wrapping_sub(self.syn_last_sent_ms) >= self.cfg.rto_ms
        {
            self.syn_last_sent_ms = now_ms;
            out.to_send.push(self.encode_syn());
            out.syn_retransmit = true;
        }

        if !self.is_established() {
            return;
        }

        // One selective-ACK vector for every datagram emitted this pump (the
        // receive state doesn't change while we only send), piggybacked on data
        // and used for the trailing pure ACK.
        let ack_vector = self.ack_vector();
        let mut sent_data = false;

        // Retransmit unacked packets whose RTO elapsed.
        for entry in &mut self.unacked {
            if now_ms.wrapping_sub(entry.last_sent_ms) >= self.cfg.rto_ms {
                entry.last_sent_ms = now_ms;
                out.to_send.push(encode_data(
                    self.recv_next,
                    self.have_peer_seq,
                    self.cfg.recv_window,
                    entry.seq,
                    &entry.payload,
                    ack_vector.clone(),
                ));
                sent_data = true;
                out.retransmits += 1;
            }
        }

        // Send new data while the in-flight window allows.
        let max_payload = (self.cfg.mtu as usize)
            .saturating_sub(HEADERS_OVERHEAD)
            .max(1);
        while !self.out_queue.is_empty() && self.unacked.len() < self.peer_recv_window as usize {
            let take = self.out_queue.len().min(max_payload);
            let payload: Vec<u8> = self.out_queue.drain(..take).collect();
            let seq = self.send_next;
            self.send_next = self.send_next.wrapping_add(1);
            let datagram = encode_data(
                self.recv_next,
                self.have_peer_seq,
                self.cfg.recv_window,
                seq,
                &payload,
                ack_vector.clone(),
            );
            // Lossy redundancy spike: ship a second identical copy (same seq) so
            // independent loss only costs us at p². The peer de-dups by seq, so
            // the upper layer never sees it. Reliable mode never duplicates (it
            // has RTO retransmit instead).
            if self.cfg.mode == DeliveryMode::Lossy && self.cfg.duplicate_lossy_sends {
                out.to_send.push(datagram.clone());
            }
            out.to_send.push(datagram);
            // Reliable: track for RTO retransmit until acked. Lossy: send once and
            // forget (no retransmit) — `unacked` stays empty, so the window check
            // above never blocks and the retransmit loop is a no-op.
            if self.cfg.mode == DeliveryMode::Reliable {
                self.unacked.push_back(Unacked {
                    seq,
                    payload,
                    last_sent_ms: now_ms,
                });
            }
            sent_data = true;
        }

        // Any owed ACK was piggybacked on data; otherwise send a pure ACK.
        if self.need_ack && !sent_data {
            out.to_send.push(
                Datagram::ack(self.ack_num(), self.cfg.recv_window, ack_vector)
                    .encode()
                    .expect("encode ack"),
            );
        }
        if sent_data || self.need_ack {
            self.need_ack = false;
        }
    }

    fn ack_num(&self) -> i32 {
        // Cumulative: the highest in-order sequence delivered so far. Before any
        // data, one below the peer's ISN ("nothing received").
        if self.have_peer_seq {
            self.recv_next.wrapping_sub(1) as i32
        } else {
            -1
        }
    }

    /// Build the selective-ACK vector for an outbound ACK. MS-RDPEUDP 2.2.2.7: the
    /// first element describes the datagram at `snSourceAck` (= `recv_next - 1`)
    /// and subsequent runs go to *decreasing* sequence numbers. We deliver strictly
    /// in order, so the contiguous source-packet run ending at `snSourceAck` — back
    /// to the first source packet (`peer_isn + 1`; the SYN at `peer_isn` is acked by
    /// `snSourceAck` itself, not the source vector) — is all `Received`, emitted as
    /// run-length elements (≤63 each), bounded by our advertised receive window.
    ///
    /// A real (non-empty) vector is REQUIRED for mstsc to retire the datagram —
    /// with an empty vector mstsc ignores the ACK and retransmits forever (verified
    /// live). Selective NACK runs (gaps in `reorder`) are a later refinement; while
    /// the stream stays in order this exact vector is correct.
    fn ack_vector(&self) -> AckVectorHeader {
        if !self.have_peer_seq {
            return AckVectorHeader { elements: vec![] };
        }
        let mut count = self
            .recv_next
            .wrapping_sub(self.peer_isn)
            .saturating_sub(1)
            .min(u32::from(self.cfg.recv_window));
        let mut elements = Vec::new();
        while count > 0 {
            let run = count.min(63) as u8;
            elements.push(AckVectorElement {
                state: VectorElementState::Received,
                run_length: run,
            });
            count -= u32::from(run);
        }
        AckVectorHeader { elements }
    }

    fn encode_syn(&self) -> Vec<u8> {
        let syn = crate::pdu::SynData {
            initial_seq: self.cfg.initial_seq,
            upstream_mtu: self.cfg.mtu,
            downstream_mtu: self.cfg.mtu,
        };
        Datagram::syn(self.ack_num(), self.cfg.recv_window, syn, None, None)
            .encode()
            .expect("encode syn")
    }

    /// Server reply: a SYN+ACK echoing the client's ISN as `snSourceAck` and the
    /// negotiated version in its SYNEX — byte-shaped like a real Windows server's
    /// (no ack vector, no cookie hash). See [`Datagram::syn_ack`].
    fn encode_syn_ack(&self) -> Vec<u8> {
        let syn = crate::pdu::SynData {
            initial_seq: self.cfg.initial_seq,
            upstream_mtu: self.cfg.mtu,
            downstream_mtu: self.cfg.mtu,
        };
        Datagram::syn_ack(
            self.peer_isn,
            self.cfg.recv_window,
            syn,
            self.negotiated_version,
        )
        .encode()
        .expect("encode syn+ack")
    }
}

/// FEC header (8) + ACK vector (4, min) + source payload header (8).
const HEADERS_OVERHEAD: usize = 8 + 4 + 8;

fn encode_data(
    recv_next: u32,
    have_peer_seq: bool,
    recv_window: u16,
    seq: u32,
    payload: &[u8],
    ack_vector: AckVectorHeader,
) -> Vec<u8> {
    let ack = if have_peer_seq {
        recv_next.wrapping_sub(1) as i32
    } else {
        -1
    };
    Datagram::data(
        ack,
        recv_window,
        ack_vector,
        SourcePacket {
            header: SourcePayloadHeader {
                sn_coded: seq,
                sn_source_start: seq,
            },
            payload: payload.to_vec(),
        },
    )
    .encode()
    .expect("encode data")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(initial_seq: u32) -> Config {
        Config {
            recv_window: 8,
            mtu: 1132, // → small-ish payloads exercise fragmentation
            initial_seq,
            rto_ms: 100,
            mode: DeliveryMode::Reliable,
            duplicate_lossy_sends: false,
        }
    }

    /// Drive client↔server to the established state.
    fn handshook() -> (RdpeudpState, RdpeudpState, u64) {
        let mut client = RdpeudpState::new(Role::Client, cfg(1000));
        let mut server = RdpeudpState::new(Role::Server, cfg(9000));
        let mut now = 0;
        let o = client.start(now);
        // SYN → server
        for dg in o.to_send {
            let so = server.step(now, Some(&dg));
            for back in so.to_send {
                client.step(now, Some(&back));
            }
        }
        now += 1;
        assert!(client.is_established() && server.is_established());
        (client, server, now)
    }

    #[test]
    fn handshake_establishes_both_ends() {
        let (c, s, _) = handshook();
        assert!(c.is_established());
        assert!(s.is_established());
    }

    #[test]
    fn delivers_in_order_no_loss() {
        let (mut client, mut server, mut now) = handshook();
        let msg = b"the quick brown fox jumps over the lazy dog".repeat(5);
        let out = client.enqueue(now, &msg);
        let mut delivered = Vec::new();
        let mut wire = out.to_send;
        // Bounce datagrams until quiescent.
        for _ in 0..200 {
            if wire.is_empty() {
                break;
            }
            let mut next_wire = Vec::new();
            for dg in wire.drain(..) {
                let so = server.step(now, Some(&dg));
                for p in so.delivered {
                    delivered.extend_from_slice(&p);
                }
                for back in so.to_send {
                    let co = client.step(now, Some(&back));
                    next_wire.extend(co.to_send);
                }
            }
            now += 1;
            wire = next_wire;
        }
        assert_eq!(delivered, msg);
    }

    /// End-to-end capture validation: feed a **real Microsoft client's SYN**
    /// (V3/EUDP2, with cookie hash) into a server `RdpeudpState` configured with
    /// the **real server's** ISN/window/MTU, and assert the SYN+ACK it emits is
    /// byte-identical to the **real server's SYN+ACK** captured in reply. This
    /// pins the whole server handshake path (decode client SYN → negotiate V3 →
    /// encode SYN+ACK) to the wire, not just the isolated codecs.
    #[test]
    fn server_syn_ack_matches_capture_end_to_end() {
        #[rustfmt::skip]
        let client_syn: [u8; 84] = [
            0xff, 0xff, 0xff, 0xff, 0x00, 0x40, 0x18, 0x01, 0x64, 0x7a, 0x02, 0xbc, 0x04, 0xd0,
            0x04, 0xd0, 0x43, 0x33, 0x3c, 0x63, 0xee, 0x77, 0x40, 0x6e, 0x97, 0xdf, 0x80, 0x0c,
            0xa1, 0xfd, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x01, 0x01, 0xcb, 0x86, 0x4c, 0x5b,
            0x54, 0x3a, 0xdc, 0x7a, 0x7a, 0x36, 0x7b, 0xb8, 0x11, 0x20, 0x71, 0x7c, 0x28, 0x6d,
            0x09, 0x3d, 0x3f, 0x3a, 0xd8, 0x80, 0x2c, 0x59, 0x4f, 0x4f, 0x21, 0x99, 0x86, 0x94,
        ];
        #[rustfmt::skip]
        let expected_syn_ack: [u8; 20] = [
            0x64, 0x7a, 0x02, 0xbc, 0x00, 0x40, 0x10, 0x05, 0x61, 0xf7, 0xb6, 0xe3, 0x04, 0xd0,
            0x04, 0xd0, 0x00, 0x01, 0x01, 0x01,
        ];

        // The real server used ISN 0x61f7b6e3, window 64, MTU 1232.
        let mut server = RdpeudpState::new(
            Role::Server,
            Config {
                recv_window: 64,
                mtu: 1232,
                initial_seq: 0x61f7_b6e3,
                rto_ms: 300,
                mode: DeliveryMode::Reliable,
                duplicate_lossy_sends: false,
            },
        );
        let out = server.step(0, Some(&client_syn));

        assert!(server.is_established());
        assert_eq!(
            server.negotiated_version(),
            UdpVersion::V3,
            "client requested V3 (EUDP2) in its SYNEX"
        );
        assert_eq!(out.to_send.len(), 1, "exactly one SYN+ACK, no extra acks");
        assert_eq!(
            out.to_send[0], expected_syn_ack,
            "server SYN+ACK must be byte-identical to the captured Windows server reply"
        );
    }

    /// Deterministic lossy/reordering/duplicating channel: a tiny LCG decides,
    /// per datagram, to drop / duplicate / hold-and-reorder. Asserts the full
    /// message is still delivered in order.
    #[test]
    fn delivers_under_loss_reorder_dup() {
        for seed in [1u64, 7, 42, 1234] {
            let (mut client, mut server, mut now) = handshook();
            let msg: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
            client.enqueue(now, &msg);

            // Each "wire" holds (deliver_at_ms, bytes). A held packet models reorder.
            let mut c2s: Vec<(u64, Vec<u8>)> = Vec::new();
            let mut s2c: Vec<(u64, Vec<u8>)> = Vec::new();
            let mut rng = seed;
            let next = |rng: &mut u64| {
                *rng = rng
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (*rng >> 33) as u32
            };
            // Seed the wires with the client's initial output.
            for o in client.step(now, None).to_send {
                c2s.push((now, o));
            }

            let mut delivered = Vec::new();
            for _round in 0..20000 {
                let done = delivered.len() >= msg.len() && c2s.is_empty() && s2c.is_empty();
                if done {
                    break;
                }

                // Deliver due client→server datagrams (with loss/dup).
                let (due, held): (Vec<_>, Vec<_>) = c2s.into_iter().partition(|(t, _)| *t <= now);
                c2s = held;
                for (_, dg) in due {
                    let r = next(&mut rng) % 10;
                    if r == 0 {
                        continue; // drop
                    }
                    let times = if r == 1 { 2 } else { 1 }; // occasional dup
                    for _ in 0..times {
                        let so = server.step(now, Some(&dg));
                        for p in so.delivered {
                            delivered.extend_from_slice(&p);
                        }
                        for back in so.to_send {
                            // reorder: random small delay
                            let delay = u64::from(next(&mut rng) % 3);
                            s2c.push((now + delay, back));
                        }
                    }
                }

                // Deliver due server→client datagrams.
                let (due, held): (Vec<_>, Vec<_>) = s2c.into_iter().partition(|(t, _)| *t <= now);
                s2c = held;
                for (_, dg) in due {
                    if next(&mut rng) % 10 == 0 {
                        continue; // drop
                    }
                    let co = client.step(now, Some(&dg));
                    for back in co.to_send {
                        let delay = u64::from(next(&mut rng) % 3);
                        c2s.push((now + delay, back));
                    }
                }

                // Advance the clock; periodically fire timers so retransmits flow.
                now += 1;
                if now % 50 == 0 {
                    for o in client.step(now, None).to_send {
                        c2s.push((now, o));
                    }
                    let so = server.step(now, None);
                    for o in so.to_send {
                        s2c.push((now, o));
                    }
                }
            }
            assert_eq!(delivered, msg, "seed {seed}: full in-order delivery");
        }
    }

    /// The diagnostic `StepOutput.retransmits` counter increments when an
    /// unacked data segment's RTO elapses (the signal a lossy-link soak watches),
    /// and stays zero while the segment is still fresh / once it's acked.
    #[test]
    fn retransmit_counter_reports_rto_resends() {
        let (_client, mut server, now) = handshook();
        let rto = server.cfg.rto_ms;

        // Server sends one data segment; it's now in-flight (unacked).
        let out = server.enqueue(now, b"hello over udp");
        assert!(!out.to_send.is_empty(), "data segment went on the wire");
        assert_eq!(out.retransmits, 0, "first send is not a retransmit");

        // Before the RTO elapses, a timer tick must NOT retransmit.
        let out = server.step(now + rto - 1, None);
        assert_eq!(out.retransmits, 0, "still within RTO — no resend");

        // Once the RTO elapses with no ACK, the segment is retransmitted.
        let out = server.step(now + rto, None);
        assert_eq!(out.retransmits, 1, "RTO elapsed → exactly one resend");
        assert!(
            !out.to_send.is_empty(),
            "the retransmitted datagram is emitted"
        );

        // After the peer cumulatively ACKs it, the window drains and further
        // ticks neither resend nor count.
        let ack = Datagram::ack(
            server.send_next.wrapping_sub(1) as i32,
            8,
            server.ack_vector(),
        )
        .encode()
        .expect("encode ack");
        server.step(now + rto, Some(&ack));
        let out = server.step(now + rto * 4, None);
        assert_eq!(
            out.retransmits, 0,
            "acked segment is no longer retransmitted"
        );
    }

    // ---- P2.2: lossy delivery mode (UdpFecL) -------------------------------

    fn cfg_lossy(initial_seq: u32) -> Config {
        Config {
            mode: DeliveryMode::Lossy,
            ..cfg(initial_seq)
        }
    }

    /// Drive a lossy client↔server pair to established. The handshake itself is
    /// mode-independent, so this mirrors [`handshook`] with the lossy config.
    fn handshook_lossy() -> (RdpeudpState, RdpeudpState, u64) {
        let mut client = RdpeudpState::new(Role::Client, cfg_lossy(1000));
        let mut server = RdpeudpState::new(Role::Server, cfg_lossy(9000));
        let mut now = 0;
        let o = client.start(now);
        for dg in o.to_send {
            let so = server.step(now, Some(&dg));
            for back in so.to_send {
                client.step(now, Some(&back));
            }
        }
        now += 1;
        assert!(client.is_established() && server.is_established());
        (client, server, now)
    }

    /// Capture only the source-carrying (data) datagrams from a pump's output.
    fn data_only(to_send: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
        to_send
            .into_iter()
            .filter(|dg| {
                Datagram::decode(dg)
                    .map(|d| d.source.is_some())
                    .unwrap_or(false)
            })
            .collect()
    }

    /// Lossy mode delivers a source payload on arrival even when it's ahead of the
    /// expected sequence — no in-order head-of-line blocking. Fed in reverse, every
    /// packet but the first is a gap, yet each is delivered immediately.
    #[test]
    fn lossy_delivers_out_of_order_on_arrival_no_hol() {
        let (mut client, mut server, now) = handshook_lossy();
        // ~2500 bytes over a 1132-MTU link → multiple source packets.
        let msg: Vec<u8> = b"abcdefghij".iter().copied().cycle().take(2500).collect();
        let data = data_only(client.enqueue(now, &msg).to_send);
        assert!(data.len() >= 2, "message must span multiple source packets");

        let mut delivered: Vec<Vec<u8>> = Vec::new();
        for (i, dg) in data.iter().rev().enumerate() {
            let so = server.step(now, Some(dg));
            if i == 0 {
                // The highest-seq packet arrives first (a gap from recv_next). Lossy
                // hands it up immediately; reliable would buffer it (see the control
                // test below).
                assert_eq!(
                    so.delivered.len(),
                    1,
                    "lossy delivers the out-of-order head on arrival (no HOL)"
                );
            }
            delivered.extend(so.delivered);
        }
        // Every chunk arrived (in reverse); un-reversing reconstructs the message.
        let reassembled: Vec<u8> = delivered.iter().rev().flatten().copied().collect();
        assert_eq!(reassembled, msg, "all payloads delivered, none dropped");
    }

    /// Control: the reliable machine buffers a future (out-of-order) source packet
    /// instead of delivering it — the HOL behavior lossy mode deliberately avoids.
    #[test]
    fn reliable_buffers_out_of_order_head() {
        let (mut client, mut server, now) = handshook();
        let msg: Vec<u8> = b"abcdefghij".iter().copied().cycle().take(2500).collect();
        let data = data_only(client.enqueue(now, &msg).to_send);
        assert!(data.len() >= 2);
        // Feed the highest-seq packet first: reliable buffers it, delivers nothing.
        let so = server.step(now, Some(data.last().unwrap()));
        assert!(
            so.delivered.is_empty(),
            "reliable buffers the future packet (HOL) until the gap fills"
        );
    }

    /// Lossy mode sends each source packet exactly once — no RTO retransmit, ever,
    /// even when the peer never ACKs. (Reliable would resend; see
    /// `retransmit_counter_reports_rto_resends`.)
    #[test]
    fn lossy_sender_sends_once_never_retransmits() {
        let (_client, mut server, now) = handshook_lossy();
        let rto = server.cfg.rto_ms;

        let out = server.enqueue(now, b"a lossy audio access unit");
        assert!(!out.to_send.is_empty(), "data goes on the wire once");
        assert_eq!(out.retransmits, 0, "first send is not a retransmit");

        // No ACK ever arrives. A reliable sender would resend on RTO; lossy must not.
        let out = server.step(now + rto, None);
        assert_eq!(out.retransmits, 0, "lossy never retransmits");
        assert!(out.to_send.is_empty(), "no resend on RTO in lossy mode");

        let out = server.step(now + rto * 10, None);
        assert_eq!(out.retransmits, 0);
        assert!(out.to_send.is_empty(), "still no resend much later");
    }

    /// Drive a lossy pair to established with `duplicate_lossy_sends` set on the
    /// server, so its data emissions are 1+1 redundant.
    fn handshook_lossy_dup() -> (RdpeudpState, RdpeudpState, u64) {
        let mut client = RdpeudpState::new(Role::Client, cfg_lossy(1000));
        let mut server = RdpeudpState::new(
            Role::Server,
            Config {
                duplicate_lossy_sends: true,
                ..cfg_lossy(9000)
            },
        );
        let mut now = 0;
        let o = client.start(now);
        for dg in o.to_send {
            let so = server.step(now, Some(&dg));
            for back in so.to_send {
                client.step(now, Some(&back));
            }
        }
        now += 1;
        assert!(client.is_established() && server.is_established());
        (client, server, now)
    }

    /// P2.3 FEC pivot: with `duplicate_lossy_sends`, each source datagram is emitted
    /// twice — byte-identical, same sequence number — a 1+1 repetition code so an
    /// independent-loss link costs us only at p². De-duplication of the surviving
    /// copy is the **upper layer's** job, not the transport's: in production the
    /// payload is a DTLS record and mstsc's DTLS anti-replay window drops the
    /// identical-bytes duplicate (the same self-dedup property `recv_source`'s lossy
    /// branch relies on — see the control test `lossy_receiver_does_not_dedup`).
    #[test]
    fn lossy_duplicate_sends_emits_each_source_twice_byte_identical() {
        let (_client, mut server, now) = handshook_lossy_dup();
        // Small enough to be a single source packet, so the duplication is exact.
        let data = data_only(server.enqueue(now, b"one aac access unit").to_send);
        assert_eq!(
            data.len(),
            2,
            "one source packet shipped as two identical copies"
        );
        assert_eq!(
            data[0], data[1],
            "the duplicate is byte-identical (same seq, same payload)"
        );
    }

    /// Documents *why* duplication is safe only because the upper layer dedups: the
    /// lossy receiver itself intentionally does NOT — it delivers every arrival
    /// (no buffering/ordering is the whole point). So at the wire a duplicate seq is
    /// delivered twice; the DTLS replay window above it drops the copy in production.
    #[test]
    fn lossy_receiver_does_not_dedup() {
        let (mut client, mut recv, now) = handshook_lossy();
        let data = data_only(client.enqueue(now, b"one aac access unit").to_send);
        let first = recv.step(now, Some(&data[0]));
        assert_eq!(first.delivered.len(), 1, "first copy delivers the payload");
        let second = recv.step(now, Some(&data[0]));
        assert_eq!(
            second.delivered.len(),
            1,
            "the transport re-delivers the duplicate — dedup is the upper (DTLS) layer's job"
        );
    }

    /// Control: without the flag a lossy sender emits each source packet once
    /// (the default, byte-identical to every existing caller).
    #[test]
    fn lossy_without_duplicate_flag_sends_once() {
        let (_client, mut server, now) = handshook_lossy();
        let data = data_only(server.enqueue(now, b"one aac access unit").to_send);
        assert_eq!(data.len(), 1, "no duplication when the flag is off");
    }

    /// The flag is lossy-only: a reliable sender never duplicates (it has RTO
    /// retransmit instead), even if the flag is somehow set.
    #[test]
    fn reliable_ignores_duplicate_flag() {
        let mut client = RdpeudpState::new(Role::Client, cfg(1000));
        let mut server = RdpeudpState::new(
            Role::Server,
            Config {
                duplicate_lossy_sends: true,
                ..cfg(9000)
            },
        );
        let mut now = 0;
        let o = client.start(now);
        for dg in o.to_send {
            let so = server.step(now, Some(&dg));
            for back in so.to_send {
                client.step(now, Some(&back));
            }
        }
        now += 1;
        let data = data_only(server.enqueue(now, b"reliable payload").to_send);
        assert_eq!(data.len(), 1, "reliable mode never duplicates");
    }
}
