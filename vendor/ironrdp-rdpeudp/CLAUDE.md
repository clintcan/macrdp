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
  tested.
- **M2b (next):** `RDPUDP_ACK_VECTOR_HEADER` (fetch its exact padding from
  MS-RDPEUDP 2.2.2.7 first) + the reliability state machine (`step()`/`enqueue()`
  over v1 framing), tested in-memory under loss/reorder/dup.
- **EUDP2 (spike-gated, NOT started):** the RDPEUDP2 (`0x0101`) bit-packed
  framing is underdocumented — **validate against a real mstsc capture +
  FreeRDP's `rdpudp` source before authoring the structs** (do NOT author from
  the spec alone; internal round-trip tests would be circular).
