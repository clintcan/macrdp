//! `ironrdp-rdpeudp` — wire types + (later) reliability state machine for RDP
//! UDP multitransport (MS-RDPEUDP / MS-RDPEUDP2).
//!
//! This is the **sans-I/O core** of macrdp's UDP multitransport effort (M2 of
//! the plan in `docs/rdp-udp-multitransport-feasibility.md`): no sockets, no
//! async — just `Decode`/`Encode` over `ironrdp-core` cursors plus, eventually,
//! a pure reliability state machine that takes datagrams in and emits datagrams
//! out. New crate; a candidate for upstreaming into IronRDP. See `CLAUDE.md`.
//!
//! # Milestone status
//!
//! - **M2a (this code):** MS-RDPEUDP **v1** PDU codecs — the SYN-handshake +
//!   source-packet framing (`FecHeader`, `SynData`, `SynDataEx`,
//!   `CorrelationId`, `SourcePayloadHeader`). Round-trip tested, with one test
//!   anchored to the real network capture in the MS-RDPEUDP spec.
//! - **M2b (done):** the `RDPUDP_ACK_VECTOR_HEADER` + the reliability state
//!   machine (sequencing / ACK / retransmit / reassembly), tested in-memory.
//! - **EUDP2 foundation (done):** `eudp2` — the RDPEUDP2 (`0x0101`) framing,
//!   cracked from FreeRDP's `rdp-udp.lua` dissector and verified byte-exact
//!   against a real mstsc capture (the wire format swaps byte 0 and byte 7).
//!
//! # Endianness
//!
//! **RDPEUDP is big-endian (network byte order)** — unlike MS-RDPBCGR, which is
//! little-endian. Every multi-byte field here is encoded/decoded big-endian (via
//! `to_be_bytes`/`from_be_bytes`), confirmed against the spec's capture example.

pub mod datagram;
pub mod eudp2;
pub mod pdu;
pub mod state;
