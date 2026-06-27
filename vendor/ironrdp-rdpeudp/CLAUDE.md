# vendor/ironrdp-rdpeudp — new crate (NOT a fork)

A **new** crate authored for macrdp's RDP UDP multitransport effort (M2 of
`docs/rdp-udp-multitransport-feasibility.md`) — not a fork of an upstream crate.
The **sans-I/O core** of UDP multitransport: MS-RDPEUDP / MS-RDPEUDP2 wire
codecs plus (later) the reliability state machine. No sockets, no async; just
`Decode`/`Encode` over `ironrdp-core` cursors and a pure datagram-in/datagram-out
state machine. **Candidate for upstreaming** into IronRDP once proven.

## Why it's structured oddly (standalone, not a workspace member)

macrdp is a single cargo package, **not a workspace**, and converting it would be
invasive (it would change `cargo test` to run every vendored crate's tests,
including the ones with `test = false` / untested forks). So this crate is built
two ways:

- **In the macrdp build (M3+):** as a path dependency; `ironrdp-core` resolves via
  the ROOT `Cargo.toml`'s `[patch.crates-io]` git pin.
- **Standalone (its own tests):** `cargo test --manifest-path vendor/ironrdp-rdpeudp/Cargo.toml`.
  Honors **this crate's own `[patch.crates-io]`** (ignored in the macrdp build) to
  resolve `ironrdp-core` to the **same** git rev. Keep that rev in sync with the
  root Cargo.toml when bumping the IronRDP pin.

CI runs the standalone `fmt --check` + `test` in a dedicated `ci.yml` step (the
root `cargo test`/`fmt --all` don't reach a non-member crate). `target/` and
`Cargo.lock` here are gitignored.

## Load-bearing facts

- **RDPEUDP is big-endian** (network byte order) — *unlike* MS-RDPBCGR. Every
  multi-byte field uses `*_be` cursor methods / `to_be_bytes`. Confirmed against
  the spec's "Source Packet" capture (`fec_header_matches_spec_capture` /
  `source_payload_header_matches_spec_capture` anchor the codec to real bytes).
- `ironrdp-core` needs the **`alloc`** feature for `encode_vec`/`decode`.

## Milestone status

- **P2.3 FEC pivot — 1+1 lossy duplicate sends (done, 2026-06-27):** real
  Reed-Solomon FEC is structurally unavailable (a real-Windows capture shows modern
  mstsc negotiates RDPUDP2, which has no FEC — see
  `docs/rdp-udp-multitransport-feasibility.md` "P2.3 FEC capture RESULT"). This is
  the protocol-safe stand-in: `Config` gained `duplicate_lossy_sends: bool` (default
  `false`, so every existing caller is byte-identical). When set **and**
  `mode == Lossy`, `pump()` pushes each new source datagram into `to_send` **twice**
  — byte-identical, same sequence number — a repetition code so an independent-loss
  link of rate `p` only costs the payload at `p²`. **De-dup is the upper layer's job,
  NOT the transport's** (the lossy receiver deliberately does not dedup — it delivers
  every arrival): in production the payload is a DTLS record and mstsc's DTLS
  anti-replay window drops the identical-bytes duplicate, so the upper layer (audio)
  never double-plays. Reliable mode ignores the flag (it has RTO retransmit instead).
  Unit-tested: `lossy_duplicate_sends_emits_each_source_twice_byte_identical` (two
  identical copies emitted), `lossy_without_duplicate_flag_sends_once` +
  `reliable_ignores_duplicate_flag` (controls), and `lossy_receiver_does_not_dedup`
  (documents *why* dedup must live above the transport). 50 crate tests. Wired in the
  listener behind `MACRDP_UDP_LOSSY_AUDIO_DUP` (lossy flow only; see ironrdp-server
  CLAUDE.md (12) P2.3).

- **P2.2 step 1 — lossy delivery mode (done, 2026-06-27; SM logic only, NOT yet
  wired):** `Config` gained `mode: DeliveryMode { Reliable, Lossy }` (default
  `Reliable`, so every existing caller/test is byte-identical). In
  `DeliveryMode::Lossy` the SM (a) **delivers each source payload on arrival** —
  no reorder buffer, no in-order head-of-line block (the whole point; a packet
  after a gap is handed up immediately) — advancing `recv_next` monotonically to
  the highest seen seq + 1 so the cumulative ACK still reports progress (it
  over-reports under loss, but a lossy sender never retransmits on it, so harmless);
  and (b) **sends our own data once, never retransmits** — the send loop skips the
  `unacked` push in lossy mode, so the RTO retransmit loop is a no-op and the window
  check never blocks. We do NOT dedup inbound in lossy mode — the upper layer (DTLS
  records / audio) is self-deduplicating. Unit-tested: `lossy_delivers_out_of_order_
  on_arrival_no_hol` (fed in reverse, every packet but the first is a gap, each
  delivered immediately; reassembly matches), a reliable control
  (`reliable_buffers_out_of_order_head`: the same future packet is buffered →
  nothing delivered), and `lossy_sender_sends_once_never_retransmits` (no resend on
  RTO ever). 46 crate tests. **NOT integrated:** the listener
  (`ironrdp-server/src/multitransport/listener.rs`) still builds every peer with
  `DeliveryMode::Reliable` — the lossy (`UdpFecL`) flow keeps riding the reliable SM
  (fine on a clean link; the DTLS handshake currently relies on it). Switching the
  lossy flow to `DeliveryMode::Lossy` per-flow — and confirming the DTLS handshake
  still completes when the transport stops retransmitting (DTLS has its own flight
  retransmission) — is P2.2 step 2. See `docs/rdp-udp-multitransport-feasibility.md`
  "P2.2".

- **Soak observability (done, 2026-06-26):** `StepOutput` gained two diagnostic
  fields — `retransmits: usize` (in-flight data segments resent on RTO this step)
  and `syn_retransmit: bool` (client SYN resent). Set in `pump()`; the crate stays
  sans-I/O and silent — the **listener** logs them (`RDPEUDP RTO retransmit`) so a
  lossy-link soak can confirm the recovery path actually fires. Unit-tested
  (`retransmit_counter_reports_rto_resends`: 0 before RTO → 1 on RTO → 0 after ACK).
  See `docs/rdp-udp-multitransport-feasibility.md` "Soak testing the UDP path under
  loss" + `scripts/netshape.sh`.
- **M5b-1 (done — data-path wire codecs, 2026-06-26; integration pending):** the
  pure, spec-verified codecs the EGFX-over-UDP data path needs, ahead of the
  (larger) server integration — same codecs-before-wiring pattern as M2. Two adds:
  - `emt.rs`: `encode_tunnel_data(higher_layer)` / `tunnel_data_payload(pdu)` —
    RDP_TUNNEL_DATA (MS-RDPEMT 2.2.2.3, action 0x2): just a `TunnelHeader{DATA}` +
    the HigherLayerData (which, post-switch, is the **same DRDYNVC DATA PDU** that
    would otherwise ride the main connection — only the wrapper differs).
  - ~~**new `softsync.rs`**~~ **(REMOVED 2026-06-26 — moved to vendored
    `ironrdp-dvc`)**: the MS-RDPEDYC Soft-Sync PDUs were briefly authored here, but
    the KEY ARCHITECTURE FINDING is that Soft-Sync is **RDPEDYC (drdynvc), NOT
    RDPEUDP/RDPEMT** — both PDUs travel over the DRDYNVC static channel on the MAIN
    (TCP) connection, not the UDP tunnel (only the channel *data* after the switch
    rides the tunnel). So the codec belongs in the drdynvc crate, where its
    `DrdynvcServerPdu`/`DrdynvcClientPdu` enums already model every other drdynvc
    PDU and the server `process()` loop can decode the client's response without
    erroring. M5b-2 vendored `ironrdp-dvc` and added `SoftSyncRequestPdu` /
    `SoftSyncResponsePdu` there; this module was deleted. See
    `vendor/ironrdp-dvc/CLAUDE.md`.
  Integration (send the request over drdynvc, the Initiate-Response gate, the
  server→listener handoff + recv-loop bidi refactor, route EGFX) is the next,
  bigger step — see ironrdp-server CLAUDE.md (12) M5 plan.
- **M4c (done — MS-RDPEMT tunnel established on real mstsc, 2026-06-25):** new
  `src/emt.rs` — the MS-RDPEMT *tunnel* PDU codecs (a different protocol from the
  RDPEUDP transport below it; rides the TLS-secured reliable stream). **MS-RDPEMT
  is little-endian** (an RDP byte-stream protocol like MS-RDPBCGR), *unlike*
  RDPEUDP's big-endian — the one endianness exception in this crate. Models the
  handshake PDUs macrdp needs: `TunnelHeader` (Action low-nibble | Flags
  high-nibble, LE PayloadLength, HeaderLength; full PDU = HeaderLength +
  PayloadLength), `TunnelCreateRequest::decode` (client→server: RequestID +
  Reserved + 16-byte SecurityCookie = 24-byte payload; the 28-byte plaintext real
  mstsc sends), `TunnelCreateResponse::ok().to_vec()` (server→client, HrResponse
  = S_OK), and stream-framing helpers `peek_pdu_len` / `peek_action` (frame a PDU
  from a partial buffer by reading just the 4 fixed header bytes — avoids the
  sub-header decode's need-more-bytes-vs-malformed ambiguity). The server's
  listener drives these (see ironrdp-server CLAUDE.md M4c). 40 crate tests (+5).
  Verified live: mstsc's CREATEREQUEST (request_id=2, the issued cookie echoed
  back) is answered with CREATERESPONSE(S_OK); mstsc ACKs, stops retransmitting,
  and the tunnel idles on keepalives — established. RDP_TUNNEL_DATA (channel
  migration) is M5.
- **M4b decode fix (done — verified on real mstsc, 2026-06-25):** `Datagram::decode`
  now **skips the 4-byte RDPUDP_ACK_OF_ACKVECTOR_HEADER** (`snAckOfAcksSeqNum`) that
  sits between the ack vector and the source payload when the `ACK_OF_ACKS` flag is
  set. Real mstsc sets this flag periodically once a session is up; without the
  skip those 4 bytes were folded into the reliable byte-stream (`delivered` jumped
  50→54 on exactly those packets), which corrupted the TLS records riding the
  stream — observed live as rustls `received corrupt message of type
  InvalidContentType` mid-session, right after the first ACK_OF_ACKS packet. We
  don't act on the ack-of-acks value (cumulative-ACK only); we just consume it so
  the source-payload offset stays right. The encoder still never *produces* the
  section. Regression test `data_packet_with_ack_of_acks_skips_the_section`
  hand-assembles such a packet and asserts the payload doesn't absorb the 4 bytes.
  35 crate tests.
- **M4a (done — reliable data path verified on real mstsc, 2026-06-25):** the
  reliability SM now actually carries the client's reliable byte-stream end to
  end. Two fixes, both forced by real mstsc (the in-memory two-instance test was
  self-consistent and hid them):
  1. **The SYN consumes one sequence number.** The first *source* packet is
     `initial_seq + 1`, not `initial_seq` — symmetric on both ends (`send_next`
     init `+1`; `recv_next` on the peer's SYN `+1`). Matches real Windows (a
     server SYN+ACK acks `client_ISN` for the SYN alone; the client's first data
     is `client_ISN + 1`). Without it the receiver was off by one and buffered
     every data packet forever (mstsc retransmitted with CWR; `recv_next`
     accessor added so the listener could log the exact `client_seq vs expected`).
  2. **Outbound ACKs MUST carry a populated `RDPUDP_ACK_VECTOR_HEADER`.** The
     "send the vector empty for now" deferral was wrong for interop: mstsc
     ignores an empty-vector ACK and retransmits forever. `ack_vector()` builds a
     `Received` run (≤63 per element, bounded by the recv window) from
     `snSourceAck` backward over the in-order source-packet run; threaded through
     the pure ACK and the data-piggybacked ACKs (`encode_data` gained an
     `ack_vector` param; `empty_ack()` removed). Selective NACK runs for
     out-of-order gaps remain a later refinement. Verified live: mstsc's TLS
     ClientHello (one 444-byte source packet) is delivered + acked, mstsc stops
     retransmitting and idles on `ACK|ACK_DELAYED` keepalives, waiting for the
     server's TLS ServerHello (M4b). 34 crate tests still green (the symmetric
     `+1` keeps the two-instance test self-consistent).
- **M3b (done — UDP listener, in `ironrdp-server`):** this crate became a real
  dependency of `vendor/ironrdp-server` for the first time (path dep, gated by its
  `multitransport` feature; revs already aligned so `ironrdp-core` unifies). The
  listener (`src/multitransport/listener.rs` over there) drives a per-peer
  `RdpeudpState` through the SYN→SYN+ACK handshake on a real socket. New here:
  `Datagram::peek_fec_flags` (cheap SYN-detection so the listener MTU-pads
  handshake packets). Tested over loopback from the macrdp crate (this crate stays
  `test=true`/standalone; the server is `test=false`). 34 crate tests.
- **M3a (done — server handshake wire-shaped to real Windows):** the server's
  **SYN+ACK** now matches a real Windows RDP server byte-exact (validated against
  a capture). `Datagram::syn_ack(client_isn, win, syn, version)` sets flags
  `SYN|ACK|SYNEX`, `snSourceAck = client_isn`, and a SYNEX with the negotiated
  version — with two quirks the generic `syn` couldn't express: **ACK flag set
  but NO ack-vector section** (the ack rides `snSourceAck`), and **SYNEX omits the
  cookie hash even for V3** (it's client→server only). `SynDataEx::decode_directional`
  decides cookie-hash presence by direction (the `Decode` impl defaults to the
  client-SYN case); `Datagram::decode` reads an ack vector only for
  `ACK && !SYN`, and decodes the SYNEX hash only on a plain SYN. `RdpeudpState`
  (server) captures the client's ISN + SYNEX version from the incoming SYN and
  emits the proper SYN+ACK (`negotiated_version()` accessor exposes V3). Tests:
  byte-exact `syn_ack` vs capture + an **end-to-end** test feeding the real client
  SYN through the SM and asserting the emitted SYN+ACK == the real server's.
  33 crate tests.
- **M2a (done):** MS-RDPEUDP **v1** PDU codecs in `src/pdu.rs` — `FecHeader` +
  `FecFlags`, `SynData`, `SynDataEx` (+ `UdpVersion`/`SynExFlags`, the `0x0101`
  selector for RDPEUDP2), `CorrelationId`, `SourcePayloadHeader`. Round-trip
  tested. (`FecFlags` values were corrected against the spec in M2b-1 — they were
  wrong from `0x0080` up when first authored from memory.)
- **M2b-1 (done):** `RDPUDP_ACK_VECTOR_HEADER` codec in `src/pdu.rs`
  (`VectorElementState` 2-bit + `AckVectorElement` 6-bit run length +
  `AckVectorHeader`, padded to a 4-byte multiple).
- **M2b-2 (done):** `src/datagram.rs` (whole-datagram assemble/parse: SYN /
  SYN+ACK / DATA / ACK) + `src/state.rs` — the sans-I/O reliable transport state
  machine (`RdpeudpState::{start,enqueue,step}`, `now_ms` clock). Passive+active
  open, reliable in-order de-duplicated delivery (receiver buffers out-of-order,
  sender uses cumulative ACK + RTO retransmit), fixed window. Tested by two
  instances over an in-memory lossy/reorder/dup channel across seeds. **Scope
  caveats:** cumulative-ACK only (selective retransmit via the ACK *vector* and
  congestion control are deferred); the two-instance test proves the *algorithm*
  (my SM ↔ my SM), not Windows wire-compat — mstsc's data path is EUDP2.
- **EUDP2 foundation (done):** `src/eudp2.rs` — the RDPEUDP2 (`0x0101`) framing,
  **cracked** via FreeRDP's `tools/wireshark/rdp-udp.lua` dissector + a real
  client capture. The non-obvious part: the wire "network format" **swaps byte 0
  and byte 7** (`unwrap_packet`, an involution); then `prefix(1) + LE-u16 (Flags
  low-12 | LogWindowSize high-4) + sub-headers + DataBody`. `Eudp2Flags`
  (ACK/DATA/ACKVEC/AOA/OVERHEAD/DELAYACK), `PacketPrefix`, `Eudp2Header`.
  **Verified byte-exact against 3 real captured packets** (2 data carrying TLS,
  1 ack). Sub-header order + sizes are known (ACK 7+nacks, OverheadSize 1,
  DelayAckInfo 3, AckOfAcks 2, DataHeader 4 [seq2+chanseq2], AckVector var) and
  confirmed against the capture's DataBody offsets.
- **EUDP2 sub-header walk (done):** `Eudp2Packet::parse` walks the ordered
  optional sub-headers and returns the `DataBody` slice. Field splits taken from
  the FreeRDP dissector's `dissectV2` and validated against the same 3 captured
  packets (`parse_real_*` tests). Order: ACK, OverheadSize, DelayAckInfo,
  AckOfAcks, Data(SeqNumber), AckVector, Data(ChannelSeqNumber)+DataBody. **The
  DataHeader is SPLIT** — `SeqNumber` (2B) before the AckVector, `ChannelSeqNumber`
  (2B) + body after it; a dummy packet (`Packet_Type_Index==8`) carries only
  `SeqNumber`, no body. Sub-header sizes: ACK = 7 + NumDelayedAcks (combined byte
  = NumDelayedAcks low-nibble | DelayedTimeScale high-nibble; AckTimestamp is
  24-bit LE), OverheadSize 1, DelayAckInfo 3, AckOfAcks 2, AckVector =
  BaseSeq(2)+sizeByte(1, high-bit=have-ts, low-7=coded size)+[Timestamp(4)]+vector.
- **EUDP2 inbound adapter (done):** `Eudp2Packet::inbound_view()` →
  `Eudp2Inbound { cumulative_ack: Option<u16>, peer_log_window, data:
  Option<(u16 seq, &[u8] body)> }` — the framing-neutral projection the
  reliability layer consumes (the v1 path exposes the analogous fields on
  `Datagram` directly). Capture-validated (`inbound_view_*` tests). The ACK
  sub-header's AckSeq IS the cumulative ack point (the dissector tracks it as
  `senderLow`); AckVector is selective info, exposed separately. Dummy packets
  (type==8) project `data: None`.
- **EUDP2 next — SM wiring deferred to M3 (on purpose):** the **encode** half +
  feeding `inbound_view` into `RdpeudpState` needs EUDP2's **16-bit** sequence
  space (vs v1's 32-bit; needs its own `seq_leq` half-window) AND the delayed-
  ack-vector model — and the **encode/send** side can't be validated offline (no
  bidirectional EUDP2 capture with known-good *server* output). Do it at M3
  against a live V3 client (propose V3, record the client's reaction to our
  output). Until then the SM stays v1-only (correct for the SYN handshake, which
  is always v1). Capture: ~/Documents/Projects/mstscpcap.pcapng.
