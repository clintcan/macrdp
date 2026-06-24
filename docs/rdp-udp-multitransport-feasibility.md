# UDP Multitransport on macrdp (extending IronRDP) — Feasibility Notes

*Research notes, 2026-06-25. Exploratory — macrdp does **not** implement UDP
multitransport today (it serves everything over one TCP+TLS connection), and
nothing here is committed work. This is a scoping document for if/when
remote-over-WAN ever becomes a goal. All "what IronRDP/FreeRDP do today" claims
were web-verified on the date above; verify again before acting, code moves.*

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
- **Two genuinely hard, new pieces:** (1) a DTLS implementation (rustls has
  **no** DTLS — [issue #40, open since 2016](https://github.com/rustls/rustls/issues/40);
  Rust DTLS crates are immature), and (2) the server-side transport glue
  (UDP listener, cookie→session mapping, channel migration via `drdynvc`, a
  second writer in the dispatch loop). **Server-side UDP is pioneering** — even
  FreeRDP only finished the *client*; its server path is a bootstrap stub.
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
   reliable transport uses normal TLS. macrdp/IronRDP use `rustls`, which has
   **no DTLS** (open since 2016). Options today are all weak: `webrtc-dtls`
   (community pure-Rust, WebRTC-oriented), **DusTLS** (reuses rustls primitives
   but is a DTLS 1.2 PoC / WIP), or OpenSSL DTLS bindings (a heavy new dep + a new
   security surface). **This is a real dependency problem with no clean answer.**
   (Reliable-UDP-only via RDPEUDP2 + normal TLS would sidestep DTLS — see below.)

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
reliable-only, so it rides ordinary TLS — **eliminating the DTLS dependency
problem entirely**. You'd lose the "drop-stale lossy video" benefit (the biggest
WAN win for video) but still gain RDPEUDP's own congestion control and avoid
TCP's loss=congestion throttling. Lower risk, smaller blast radius, and a
sensible Phase-1 if the feature is ever scoped. (macrdp already drops stale
*audio* at the app layer, so a reliable transport isn't as costly for audio as it
sounds.)

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
- **New dependency** (DTLS) unless the reliable-only path is chosen.

## Recommendation

- **Gate on the use case, not on feasibility.** It's *possible* and IronRDP is a
  decent host; it's just multi-month and only pays off off-LAN.
- If pursued: **Phase 1 = the PDU crate + a reliable-UDP-only (RDPEUDP2/TLS) path**
  (no DTLS, smaller refactor) to prove the transport-migration plumbing in
  `ironrdp-server`. **Phase 2 = lossy UDP + DTLS + FEC** for the real video win,
  once a viable Rust DTLS story exists.
- **Do it upstream**, not as a vendor fork.
- Validate against FreeRDP *and* mstsc; treat the specs as authoritative.

## Sources

- IronRDP architecture (sans-I/O, core vs extra tiers):
  <https://github.com/Devolutions/IronRDP/blob/master/ARCHITECTURE.md>
- rustls has no DTLS (open since 2016): <https://github.com/rustls/rustls/issues/40>
- DusTLS (WIP pure-Rust DTLS reusing rustls): <https://github.com/ShadowJonathan/dustls>
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
