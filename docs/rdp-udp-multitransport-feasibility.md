# UDP Multitransport on macrdp (extending IronRDP) — Feasibility Notes

*Research notes, 2026-06-25. This is the scoping document for the staged build.
All "what IronRDP/FreeRDP do today" claims were web-verified on the date above;
verify again before acting, code moves.*

> **Status: M1 + M2 + EUDP2 codec + M3a/M3b/M3c LANDED (2026-06-25).**
> - **M1** (PR #15, `bf26824`): MS-RDPEMT negotiation + safe TCP fallback behind
>   the `multitransport` cargo feature + `--enable-udp-multitransport` (default
>   OFF). Verified live on mstsc + sdl-freerdp; Initiate Request framing has a
>   round-trip CI test.
> - **M2** (PRs #16/#17/#18): the offline `ironrdp-rdpeudp` crate — RDPEUDP v1 PDU
>   codecs (`pdu.rs`, big-endian, spec-capture-anchored; flag values corrected
>   against the spec), ACK-vector codec, whole-datagram assemble/parse
>   (`datagram.rs`), and the **sans-I/O reliable transport state machine**
>   (`state.rs`): handshake + in-order de-duplicated delivery + cumulative-ACK +
>   RTO retransmit, proven by a two-instance in-memory loss/reorder/dup test.
> - **EUDP2 codec** (PRs #21/#22/#23): the RDPEUDP2 (`0x0101`) data framing,
>   reverse-engineered from FreeRDP's `rdp-udp.lua` dissector + a real capture and
>   verified byte-exact — network-format transform (byte 0↔7 swap), base header,
>   the full sub-header walk to the `DataBody`, and a framing-neutral inbound view.
>   The underdocumented framing that previously blocked M3 is now fully decodable.
> - **M3a** (PR #24): the server's RDPEUDP **SYN+ACK** wire-shaped byte-exact to a
>   real Windows server (ACK flag without ack-vector; SYNEX without cookie hash;
>   `snSourceAck` = client ISN; V3 negotiation) — capture-validated end to end.
> - **M3b** (PR #25): the **UDP listener** (`UdpMultitransportListener`, vendored
>   `ironrdp-server`, behind the feature) — owns a `tokio` `UdpSocket`, demuxes by
>   peer, drives a per-peer `RdpeudpState` through SYN→SYN+ACK, MTU-pads handshake
>   packets. CI-tested over loopback with the real captured client SYN.
> - **M3c** (this change): **wired into macrdp's accept path** — when
>   `--enable-udp-multitransport` is set (single-process path), macrdp binds the
>   listener on the **same address/port as TCP** at startup, so a client's UDP SYN
>   now reaches a live endpoint and gets a SYN+ACK. The session **still runs over
>   TCP** (no TLS/EMT tunnel/migration yet). Cookie validation is soft; the server
>   logs the issued cookie and the listener logs the client's `cookieHash` so the
>   hash formula can be derived from a live run. Not supported under
>   `--fork-workers` (the persistent UDP socket would belong to the supervisor —
>   deferred; warns + falls back to TCP).
>
> **Next: derive the cookie-hash formula + the EUDP2 data path.** What remains
> needs a **real, non-loopback client** (pure loopback suppresses the client's UDP
> advertisement): point mstsc-in-a-VM at macrdp on the host IP, capture macrdp's
> own cookie (server log) + the client's resulting `cookieHash` (listener log) →
> derive/verify the hash → tighten validation. Then **M4** (rustls over the
> reliable stream + the MS-RDPEMT tunnel) and **M5** (migrate the EGFX channel to
> UDP). The reliability SM is framing-agnostic and the EUDP2 inbound adapter is
> ready; the EUDP2 *encode*/SM-send side is the remaining piece that a live V3
> bidirectional capture will validate.

## TL;DR

- **It's possible to extend IronRDP for UDP multitransport (MS-RDPEMT over
  MS-RDPEUDP/EUDP2), and IronRDP's sans-I/O design is arguably a *better*
  foundation for it than FreeRDP's** — the PDU codecs and the reliability/FEC
  engine fit the sans-I/O idiom and stay unit-testable. But it is a large,
  mostly-from-scratch effort with an invasive `ironrdp-server` I/O refactor.
- **The advantage is entirely a lossy/high-latency-link story** (WAN, Wi-Fi,
  cellular): no head-of-line blocking, drop-stale instead of retransmit, FEC
  recovery without a round-trip. **On a clean LAN — macrdp's design target — the
  benefit is marginal.**
- **Two genuinely hard, new pieces:** (1) DTLS for the lossy transport (rustls has
  **no** DTLS — [issue #40, open since 2016](https://github.com/rustls/rustls/issues/40);
  pure-Rust DTLS crates are immature) — but this is **solvable today via mature
  FFI bindings (`boring`/`openssl`)**, and macrdp's build **already links C crypto**
  (rustls 0.23's default provider is `aws-lc-rs` = AWS-LC, a BoringSSL fork, + `ring`),
  so a C DTLS lib is a smaller leap than "pure-Rust → C" implies; and (2) the
  server-side transport glue (UDP listener, cookie→session mapping, channel
  migration via `drdynvc`, a second writer in the dispatch loop). **Server-side UDP
  is pioneering** — even FreeRDP only finished the *client*; its server path is a
  bootstrap stub.
- **Recommendation:** treat as a multi-month research project, gated on whether
  macrdp's target shifts from LAN to remote-over-internet. On the LAN it's built
  for, don't.

## Context — what macrdp / IronRDP do today

macrdp serves **everything over one TLS-over-TCP connection** — EGFX video,
RDPSND audio, input, clipboard, RDPDR — multiplexed through a single
`SharedWriter` (`Rc<Mutex<&mut W>>`) in the vendored `ironrdp-server`'s
`client_loop`. IronRDP is **TCP-only**: no MS-RDPEUDP / MS-RDPEMT, no UDP
transport crate. (Confirmed 2026-06-25: no such crate in the IronRDP tree.)

The whole IronRDP I/O model assumes **one reliable, ordered byte stream**:
`ironrdp-async`/`ironrdp-tokio` wrap an `AsyncRead`/`AsyncWrite` in a `Framed`
reader/writer. UDP is a *datagram* protocol with its own reliability + FEC — it
does not fit `AsyncRead`/`AsyncWrite` at all.

## Why bother (the advantage, briefly)

The single TCP connection means **head-of-line blocking**: one lost packet
stalls the entire multiplexed stream (video, audio, input) until TCP
retransmits. For live media that's exactly wrong — a late frame is worthless.
MS-RDPEMT adds auxiliary UDP transports so:

- **Lossy UDP (UDP-L)** carries video/audio: a dropped packet is skipped, not
  retransmitted; **FEC** recovers many losses without a round-trip; input/audio
  don't freeze because a video packet dropped.
- **Reliable UDP (UDP-R)** / TCP carries data that must arrive (input, clipboard).
- TCP's loss=congestion assumption (which needlessly throttles on Wi-Fi/cellular
  radio loss) is replaced by RDPEUDP's own congestion control.

Secondary architectural bonus for macrdp: it would **decouple the single-connection
A/V contention** (video on its own transport, off the shared `SharedWriter`).
But note that wouldn't touch the *CPU/mutex* contention macrdp actually sees
today (that's local-scheduling, not network) — see the A/V-contention quirk.

**This only matters off-LAN.** On a clean LAN (loss ≈ 0, RTT < 1 ms) TCP is
already near-optimal and the win is negligible.

## What IronRDP makes easy (fits the sans-I/O design)

IronRDP's core-tier crates are **sans-I/O** state machines (no sockets;
`Decode`/`Encode` over `ReadCursor`/`WriteCursor`/`WriteBuf`, `no_std`-friendly);
only extra-tier crates (`ironrdp-async`, `ironrdp-tokio`, `ironrdp-client`) touch
I/O. Two layers map cleanly onto that:

1. **PDU codecs — a new core-tier crate (`ironrdp-rdpemt` / `ironrdp-rdpeudp`).**
   Encode/decode for the multitransport PDUs (Initiate Multitransport
   Request/Response, Tunnel Create Request/Response) and the RDPEUDP/EUDP2 packet
   + FEC headers. Pure buffer-in/buffer-out, fully unit-testable, cross-platform —
   exactly what `ironrdp-pdu` already does for everything else. **Low risk,
   idiomatic.** Wire formats are symmetric, so a client decoder and a server
   encoder are the same struct inverted.

2. **The RDPEUDP reliability/FEC engine — a sans-I/O state machine.** Sequencing,
   ACK, sliding window, retransmit, forward error correction (and the
   RDPEUDP→EUDP2 upgrade; EUDP2 is reliable-only). This is fundamentally
   "datagram in → datagrams out + delivered payload," the canonical sans-I/O
   shape IronRDP favors, so it can be built and tested without sockets. Nothing
   like it exists yet, but the *idiom* is right. Because the UDP data path is
   **bidirectional** (both peers run sender + receiver), a future IronRDP *client*
   RDPEUDP and this server engine would share most of this core.

## What is genuinely hard / new

3. **DTLS.** The lossy transport is secured with **DTLS** (datagram TLS); the
   reliable transport uses normal TLS. macrdp/IronRDP use **`rustls` for TLS, and
   `rustls` has no DTLS** ([issue #40, open since 2016](https://github.com/rustls/rustls/issues/40)).
   The pure-Rust DTLS options are immature — `webrtc-dtls` (community, WebRTC-
   oriented) and **DusTLS** (reuses rustls primitives, but a DTLS 1.2 PoC / WIP).
   **The clean answer is mature FFI DTLS:** the **`boring`** crate (Cloudflare
   bindings to Google's BoringSSL — DTLS 1.0/1.2, the same DTLS Chrome's WebRTC
   uses; statically links a pinned BoringSSL, so the build is reproducible) or the
   **`openssl`** crate (DTLS 1.0/1.2, and 1.3 on OpenSSL 3.2+). Both expose DTLS via
   `SslMethod::dtls()`. Integration fits the sans-I/O style: drive `SslStream` over
   a **memory BIO** pumped with UDP datagrams (feed received packets in, read
   packets to send out), rather than the stream-oriented `tokio-openssl`/
   `tokio-boring` wrappers — [`tokio-dtls-stream-sink`](https://crates.io/crates/tokio-dtls-stream-sink)
   is a working DTLS-over-UDP reference.

   **Important context (corrected 2026-06-25):** adding `boring`/`openssl` is **not**
   "introducing C crypto for the first time." macrdp's build **already compiles C/asm
   crypto** — rustls 0.23's default crypto provider is **`aws-lc-rs`** (links
   **AWS-LC**, a C library that is itself a **BoringSSL fork**, via `aws-lc-sys`),
   plus **`ring`** (C + assembly). So a C DTLS dependency is a *second* C crypto lib
   (closely related to AWS-LC), not a departure from a "pure-Rust" build macrdp does
   not actually have. You can't reuse AWS-LC for it, though: `aws-lc-rs` exposes
   crypto *primitives* for rustls, not a libssl/DTLS *protocol* stack — hence still
   needing `boring`/`openssl`. The real costs are a **second TLS stack in the binary**
   (rustls for TCP + boring/openssl for DTLS), larger binary, and a wider crypto
   attack surface. **Lean: `boring`** (reproducible static vendored build — no
   system-lib variance, which matters for the signed/notarized macOS app — and the
   most battle-tested DTLS path); `openssl` is more conventional and the only one
   with DTLS 1.3, but its cross-platform build is fussier.
   (Reliable-UDP-only via RDPEUDP2 + normal TLS would sidestep DTLS entirely — see below.)

4. **Server I/O integration — architecturally invasive (`ironrdp-server`).** The
   server model assumes one `Framed` byte-stream writer. UDP needs:
   - A **UDP listener** that accepts arbitrary inbound flows and **associates
     each with an existing TCP session** by generating + **validating the cookie**
     in the Tunnel Create Request. This server-only glue is *not* in FreeRDP's
     client and is **stubbed** in FreeRDP's server (`multitransport_server_request`
     only sends the bootstrap PDU; the response handler is a no-op).
   - **Channel migration**: steering DVC traffic (EGFX video) off the TCP writer
     onto the UDP transport, which requires `drdynvc` (only channel data rides
     UDP) and a **second writer path** threaded through the
     `tokio::select!`-over-`SharedWriter` dispatch loop. This is a refactor of
     `client_loop`, not an additive patch.
   - **Server-grade sender behavior**: congestion window, RTT estimation, FEC
     ratios, retransmit timers — as the *bulk* sender (video). The FreeRDP client
     shows the mechanism but not the tuning (its sender is lightly exercised).

## Modular integration design (making the server hook elegant)

The server-side integration (#4) sounds invasive, but the vendored `ironrdp-server`
already has **the two seams** needed to keep it clean and quarantined — so the UDP
machinery can live in small, separately-accessed files behind one trait, with the
core touched only minimally.

**Seam 1 — an established optional-provider pattern.** `RdpServer` already carries
`sound_factory`, `cliprdr_factory`, `rdpdr_factory`, `gfx_factory`,
`connection_handler` — all `Option<Box<dyn …>>`, set via the builder, default `None`
(`server.rs:290–295`). A multitransport provider drops into the **same mold** — it's
idiomatic, not bolted on.

**Seam 2 — the writer is already an abstraction, cloned per channel.** In
`client_loop` (`server.rs:1088–1091`):
```rust
let mut writer = SharedWriter::new(writer);   // the one TCP+TLS sink
let mut display_writer = writer.clone();      // EGFX video  (migrates to UDP)
let mut event_writer   = writer.clone();      // cliprdr/rdpdr (stays TCP)
let mut audio_writer    = writer.clone();     // rdpsnd (stays TCP, or migrates)
```
Each dispatch task writes through its **own cloneable handle**, so routing
granularity is **per-channel-class, not per-byte** — and EGFX (the thing that moves
to UDP) already has its own task. No raw-byte inspection needed to route.

**Proposed structure:**

A. New **core-tier** crate `ironrdp-rdpeudp` (sans-I/O, no sockets — fits IronRDP's
"core never does I/O" rule; unit-testable):
```
crates/ironrdp-rdpeudp/src/
  pdu.rs          // RDPEUDP/EUDP2 + FEC + MS-RDPEMT PDU codecs (Decode/Encode)
  reliability.rs  // pure state machine: step(datagram) -> (delivered, to_send)
  lib.rs
```

B. New **I/O** module tree in the server `src/multitransport/` — all UDP-specific,
small files, reached only through the trait:
```
multitransport/
  mod.rs        // MultitransportProvider trait + config + the Option<Box<dyn>> field
  listener.rs   // binds the UDP socket, accept loop, per-flow task, drives reliability.rs
  session.rs    // cookie registry + Tunnel Create Request validation + flow<->session map
  dtls.rs       // SecureDatagram trait + boring/openssl impl (memory-BIO pump); OPTIONAL
  router.rs     // TransportRouter + per-channel writer handles + migrated-channel flags
  migration.rs  // which channels migrate (drdynvc steering); flips the router on ready
```

C. The hook in `server.rs` — minimal and **behavior-preserving**:
```rust
// 1. one new optional field, mirroring the 4 existing factories (default None)
multitransport: Option<Box<dyn MultitransportProvider>>,

// 2. the ONLY hot-path touch: swap the writer construction for a router that is a
//    ZERO-OVERHEAD PASSTHROUGH when no provider is present.
let router = TransportRouter::new(writer, self.multitransport.as_deref());
let display_writer = router.channel(Channel::Display);  // UDP-capable
let event_writer   = router.channel(Channel::Events);   // stays TCP
let audio_writer   = router.channel(Channel::Audio);

// the trait — small, well-defined hooks; all UDP lives behind it
trait MultitransportProvider {
    fn offer(&mut self, ctx: &SessionCtx) -> Option<InitiateRequest>; // post-licensing
    fn start(&mut self, sender: EventSender) -> MigrationHandle;       // spawn listener
}
```
A `channel()` handle checks one atomic "migrated?" flag: `false` → write to the TCP
`SharedWriter` (today's exact path); `true` → write to the UDP/DTLS sink. **No
provider ⇒ flag never set, UDP sink never built ⇒ byte-for-byte current behavior.**

Why this is elegant: it reuses the established factory pattern; quarantines all
UDP/DTLS/FEC complexity behind one trait in separate files; the DTLS backend is
itself swappable (`SecureDatagram` trait → `boring`/`openssl`, or **nothing** for the
reliable-only Phase 1); and the core change is one construction site swapped for a
passthrough wrapper plus two no-op-by-default hook calls.

**Two caveats this design does NOT dissolve:**
1. The writer-site swap is in a **working hot path** (`client_loop`, no unit tests).
   Building these seams *before* a real transport exists is a hot-path control-flow
   change with **zero functional payoff yet** — speculative future-proofing to avoid
   (cf. the "don't refactor working hot-paths for cosmetics" rule). The router should
   land **with** a working transport + real-client verification, not ahead of it.
2. A `MultitransportProvider` trait is a clean, **upstream-worthy** extension point —
   so land it **upstream with Devolutions**, not as a standalone vendor divergence.

## How much can be cribbed from FreeRDP

- **Wire formats: ~all of it** (symmetric bytes; FreeRDP client decode = our
  server encode, and vice versa).
- **Handshake choreography: by mirroring** the client's send/receive order.
- **RDPEUDP reliability core: largely** (it's bidirectional, so the client has
  both halves).
- **NOT cribable: the server-only glue** (cookie validation + session mapping,
  the listener/accept architecture — FreeRDP even flags `"TODO: move this static
  variable to the listener"`), and server-grade sender tuning.
- **Trap (macrdp's recurring lesson):** FreeRDP is *lenient* where **mstsc is
  strict** (cf. the RDPDR handshake-ordering and `FILE_GENERIC_READ` bugs).
  Deduce/validate against FreeRDP, but the **authoritative source is the open
  specs** ([MS-RDPEUDP], [MS-RDPEUDP2], [MS-RDPEMT]) and the **conformance gate
  is mstsc**.

## A cheaper middle path (if ever pursued)

**Reliable-UDP-only (RDPEUDP2 + normal TLS), no lossy/DTLS.** EUDP2 is
reliable-only, so it rides ordinary TLS over the reliable RDPEUDP stream —
**reusing macrdp's existing `rustls` and adding no new crypto dependency at all**.
You'd lose the "drop-stale lossy video" benefit (the biggest WAN win for video)
but still gain RDPEUDP's own congestion control and avoid TCP's loss=congestion
throttling. Lower risk, smaller blast radius, and a sensible Phase-1 if the
feature is ever scoped. (macrdp already drops stale *audio* at the app layer, so a
reliable transport isn't as costly for audio as it sounds.) The lossy/DTLS path
(Phase 2) is then a *known, reachable* follow-up via `boring`/`openssl`, not a
blocker — see DTLS above.

## Upstream vs. vendor

This is **far bigger than macrdp's existing vendored divergences** (small,
targeted patches). A whole new transport stack + an invasive `ironrdp-server` I/O
refactor is not a sane long-term vendor fork. It should be done **upstream with
Devolutions** — and you'd be **pioneering the server side** (no open-source RDP
server has a working UDP data path; FreeRDP's is a stub). Implementation would
track the MS-RDPEUDP/EMT specs, with FreeRDP's client as a partial reference and
mstsc as the gate.

## Distribution / packaging implications

- **No entitlement issue** (unlike USB redirection) — UDP is just sockets.
- **NAT / firewall**: UDP through home routers and firewalls is fiddlier than one
  TCP port; the server would need a reachable UDP port and graceful **fall back
  to TCP** when the UDP path can't be established (which the protocol already
  models — multitransport is best-effort over the always-present TCP connection).
- **New crypto dependency** only for the lossy/DTLS path (`boring` or `openssl`) —
  the reliable-only path adds none. Note the build **already** compiles C crypto
  (`aws-lc-sys`/AWS-LC + `ring`), so a C DTLS lib doesn't change the toolchain
  requirements, only adds a second TLS stack + binary size.

## Recommendation

- **Gate on the use case, not on feasibility.** It's *possible* and IronRDP is a
  decent host; it's just multi-month and only pays off off-LAN.
- If pursued: **Phase 1 = the PDU crate + a reliable-UDP-only (RDPEUDP2/TLS) path**
  (no DTLS, no new crypto dep, smaller refactor) to prove the transport-migration
  plumbing in `ironrdp-server`. **Phase 2 = lossy UDP + DTLS + FEC** for the real
  video win, securing the lossy transport with **`boring`** (or `openssl`) — DTLS
  is a known, reachable dependency, not a blocker.
- **Do it upstream**, not as a vendor fork.
- Validate against FreeRDP *and* mstsc; treat the specs as authoritative.

## Sources

- IronRDP architecture (sans-I/O, core vs extra tiers):
  <https://github.com/Devolutions/IronRDP/blob/master/ARCHITECTURE.md>
- rustls has no DTLS (open since 2016): <https://github.com/rustls/rustls/issues/40>
- DusTLS (WIP pure-Rust DTLS reusing rustls): <https://github.com/ShadowJonathan/dustls>
- `boring` (Cloudflare BoringSSL bindings, DTLS support): <https://github.com/cloudflare/boring>,
  <https://deepwiki.com/cloudflare/boring/3-ssltls-support>
- `openssl` / `tokio-openssl` DTLS + DTLS-over-UDP reference:
  <https://docs.rs/crate/tokio-openssl/0.1.1>, <https://crates.io/crates/tokio-dtls-stream-sink>
- macrdp already links C crypto: `aws-lc-rs` (rustls 0.23 default provider, AWS-LC = a
  BoringSSL fork) + `ring` — confirmed in `Cargo.lock`.
- FreeRDP UDP implementation write-up (client-focused):
  <https://www.hardening-consulting.com/en/posts/20210131-udp-support-1.html>,
  <https://www.hardening-consulting.com/en/posts/20230109-udp-support-2.html>
- FreeRDP server multitransport is bootstrap-only:
  <https://github.com/FreeRDP/FreeRDP/blob/master/libfreerdp/core/multitransport.c>,
  <https://github.com/FreeRDP/FreeRDP/blob/master/libfreerdp/core/peer.c>
- Specs: [MS-RDPEMT] <https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpemt/>,
  [MS-RDPEUDP] <https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpeudp/>,
  [MS-RDPEUDP2] <https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpeudp2/>

## Status

**Exploratory / not started.** No code. Gated on a LAN→WAN use-case shift.
Cross-reference: `docs/usb-redirection-feasibility.md` (the other
"big protocol + new transport layer" scoping doc) and the A/V-contention quirk in
`docs/known-quirks.md` (the single-connection coupling this would partly relieve).
