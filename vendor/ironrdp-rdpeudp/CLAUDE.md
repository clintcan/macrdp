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
