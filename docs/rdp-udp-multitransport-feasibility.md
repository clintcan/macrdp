# UDP Multitransport on macrdp (extending IronRDP) — Feasibility Notes

*Research notes, 2026-06-25. This is the scoping document for the staged build.
All "what IronRDP/FreeRDP do today" claims were web-verified on the date above;
verify again before acting, code moves.*

> **Status: Phase 1 COMPLETE — EGFX H.264 video renders over the reliable UDP
> tunnel, verified end-to-end on real mstsc (2026-06-26).** Behind the
> `multitransport` cargo feature + `--enable-udp-multitransport` (default OFF); the
> actual EGFX migration is additionally gated by the experimental
> `MACRDP_UDP_MIGRATE_EGFX` env var (default off → EGFX stays on TCP, the proven
> safe spike) until it's soaked. As far as is known this is the **first open-source
> RDP *server* with a working UDP multitransport data path** — FreeRDP, the most
> complete OSS stack, has **no working UDP data path on either side**: its server
> is a TCP-side *bootstrap stub* (emits the Initiate Request PDU, no UDP socket /
> RDPEUDP / RDPEMT) and its client **declines UDP with `E_ABORT`** (and has since
> ~2016). The RDPEUDP/RDPEUDP2 work a FreeRDP maintainer (David Fort) described in
> his 2021/2023 blog posts has only ever been **out-of-tree prototype code — never
> a merged PR or released feature** (re-verified against full FreeRDP git history
> 2026-06-26). xrdp / ogon / gnome-remote-desktop / Weston are TCP-only or ride
> FreeRDP's server lib. ("first" can't be proven exhaustively — read it as "first
> known".)
>
> Milestone history (all landed on `main`):
> - **M1** (PR #15): MS-RDPEMT negotiation + safe TCP fallback. Round-trip CI test
>   for the Initiate Request framing.
> - **M2** (PRs #16/#17/#18): offline `ironrdp-rdpeudp` crate — RDPEUDP v1 PDU
>   codecs, ACK-vector codec, whole-datagram assemble/parse, and the sans-I/O
>   reliable state machine (handshake + in-order dedup delivery + cumulative-ACK +
>   RTO retransmit), proven by a two-instance loss/reorder/dup test.
> - **EUDP2 codec** (PRs #21/#22/#23): RDPEUDP2 (`0x0101`) data framing, byte-exact.
> - **M3a/b/c** (PRs #24/#25/#26): SYN+ACK wire-shaped to real Windows; the
>   `UdpMultitransportListener` (vendored `ironrdp-server`); wired into macrdp's
>   accept path bound on the same address/port as TCP. The negotiation offer was
>   moved into the **acceptor** (it must go out after licensing, before Demand
>   Active — a post-finalization send is rejected by real clients).
> - **KEY mstsc finding:** mstsc negotiates RDPEUDP **V2 carrying plain TLS**, not
>   EUDP2, for the reliable channel — so the v1 SM is the correct codepath and the
>   security cookie rides the (TLS-encrypted) `RDP_TUNNEL_CREATEREQUEST`, not the
>   SYN (the 16-byte SYN `cookieHash` is V3-only). mstsc signals multitransport
>   success by *creating the tunnel*, NOT by a TCP Initiate Response (that's
>   failure-only, E_ABORT).
> - **M4a/b/c** (2026-06-25): reliable data path (SYN consumes a seq; ACKs need a
>   populated ack-vector); **rustls** server TLS over the reliable stream (same
>   cert as the TCP connection); **MS-RDPEMT tunnel** established
>   (`RDP_TUNNEL_CREATERESPONSE(S_OK)`). Interop fix: skip the inbound ACK_OF_ACKS
>   section or it corrupts the TLS records.
> - **M5a** (2026-06-25): **strict cookie binding** — `CookieRegistry` (CSPRNG
>   cookie, one-time `take`) binds the tunnel to a real TCP session.
> - **M5b-2** (PR #34): vendored `ironrdp-dvc` (4th fork) — server-side MS-RDPEDYC
>   **Soft-Sync** codec (request/response PDUs + decode arm). Soft-Sync rides
>   drdynvc on the **main TCP connection**; only channel *data* after the switch
>   rides the UDP tunnel.
> - **M5c step 1+2** (PR #35): Soft-Sync gate (listener-driven, on the EGFX
>   dispatch path) + send, as a safe spike (empty channel list → migrate nothing).
> - **M5c step 3a** (PR #36): name the EGFX DVC in the Soft-Sync + the outbound
>   server→listener handoff (`TunnelSender`/`tunnel_channel`). mstsc **accepted**
>   the migration (`SoftSyncResponse { tunnels: [1] }`).
> - **M5c step 3b** (PR #37): **EGFX renders over UDP.** Two fixes: (1) the tunnel
>   carries the **bare DRDYNVC PDU** (no `CHANNEL_PDU_HEADER`) →
>   `SvcMessage::encode_unframed_pdu`, not `chunkify` (the unlock — wrong framing
>   was the freeze); (2) the **inbound** tunnel→drdynvc path (per-cookie reverse
>   channel → `client_loop` `dispatch_tunnel_inbound` → `DrdynvcServer::process`).
>   Note: macrdp's H.264 throttle is `submitted − shipped` (ack-INDEPENDENT), so
>   dropped frame-acks never stalled it — framing was the only blocker.
>
> **What rides UDP today:** only **EGFX video** (when the env flag is set). Input,
> audio (RDPSND), and clipboard still ride TCP by design for this phase.
>
> **Soaked under loss (2026-06-26) — Phase 1 accepted as a clean-link feature.** A
> lossy-link soak (mstsc, 1–5% loss) found + fixed an idle-retransmit deadlock (the
> periodic timer, PR #46) and then **confirmed a structural limit**: reliable-only
> multitransport does **not** beat TCP for video under loss — EGFX-on-*TCP* froze
> under the same shaping too, because an ordered stream HOL-blocks on its own loss
> regardless of transport. Decision: keep Phase 1 as a clean-link / low-loss feature
> (default-OFF, env-gated), lossy-link video deferred to Phase 2. See "First soak
> findings" below.
>
> **Possible next steps (none started):** **Phase 2** = lossy `UdpFecL` + DTLS (via
> `boring`) + FEC — the *real* loss-resilience win; de-vendor once the IronRDP
> changes are upstreamed + released. Still **not** supported under `--fork-workers`
> (the persistent UDP socket would belong to the supervisor — deferred; warns + falls
> back to TCP). **NB — do NOT route audio over the *reliable* tunnel** (it's a
> downgrade; see "Audio belongs on the lossy transport, not the reliable one"
> below). Audio over a *lossy* tunnel is a Phase-2 thing and is arguably the best
> first payload for it — better than video.

## Landscape — UDP multitransport across open-source RDP servers

Where macrdp sits among open-source RDP **servers** on UDP multitransport
(MS-RDPEMT over MS-RDPEUDP). The distinction that matters is **three separate
things**, often conflated:

1. **Negotiation / bootstrap** — the server sends the *Initiate Multitransport
   Request* over the TCP connection (MS-RDPBCGR). Cheap; just a PDU.
2. **Server UDP *data path*** — the server actually binds a UDP socket, runs the
   RDPEUDP reliability handshake, secures it (TLS/DTLS), establishes the MS-RDPEMT
   tunnel, and **carries channel data over it**. This is the hard part.
3. **Client-side UDP data path** — the *client* counterpart of (2): it accepts
   the server's offer, opens the UDP association, secures it, joins the EMT
   tunnel, and carries channel data over it. A separate (and somewhat
   easier-to-reach) codebase than the server's. Notably, even FreeRDP — the
   most complete OSS stack — has never merged this either: its client declines
   UDP with `E_ABORT`, and the RDPEUDP/RDPEUDP2 work stayed an out-of-tree
   prototype (re-verified against full git history 2026-06-26).

macrdp implements (1) and (2); it is a server, not a client, so (3) is listed
only to keep the "supports UDP" claim unambiguous and to flag the interop
dependency — macrdp's server data path is exercised only when a client with its
own client-side UDP data path (today, only mstsc) connects to it. See "Which
clients actually take macrdp's UDP offer" below.

| RDP **server** | Base / lang | (1) Negotiation | (2) **UDP data path** | Notes |
|---|---|---|---|---|
| **macrdp** | Rust / IronRDP (vendored) | ✅ (via the acceptor) | ✅ **EGFX H.264 over reliable RDPEUDP + rustls TLS + MS-RDPEMT tunnel — verified on mstsc** | First *known* OSS RDP server with a working server-side UDP data path. Opt-in (default OFF). Lossy/DTLS/FEC = Phase 2. |
| FreeRDP (server: `freerdp-shadow`, libfreerdp server) | C | ⚠️ bootstrap only (`multitransport_server_request`) | ❌ no UDP socket / data path (response handler is a no-op) | FreeRDP's **client** also has no UDP path — it declines with `E_ABORT` (`multitransport_no_udp`). RDPEUDP/RDPEUDP2 was prototyped out-of-tree (David Fort, 2021/2023) but never merged. |
| ogon | C / FreeRDP server | ⚠️ inherits FreeRDP | ❌ | TCP-only data path (rides FreeRDP's server stub). |
| gnome-remote-desktop | C / FreeRDP server lib | ⚠️ inherits FreeRDP | ❌ | TCP-only data path. |
| Weston RDP backend | C / FreeRDP server | ⚠️ inherits FreeRDP | ❌ | TCP-only data path. |
| xrdp | C | ❌ | ❌ | TCP-only; no multitransport. |
| IronRDP (upstream Devolutions server) | Rust | ❌ | ❌ | No MS-RDPEUDP/EMT in the tree (the gap macrdp's vendored `ironrdp-rdpeudp` fills). Other IronRDP-based servers (lamco-rdp-server, hypr-rdp, cosmic-ext-rdp-server, ARISU) inherit the same TCP-only model. |
| *(reference)* Microsoft RDS | — (proprietary) | ✅ | ✅ reliable **and** lossy + FEC + DTLS | The spec baseline; not open source. The protocol target macrdp implements against. |

**Bottom line:** every other open-source RDP *server* either has no multitransport
at all (xrdp, IronRDP upstream) or stops at the TCP-side negotiation handshake and
never opens a UDP data path (FreeRDP and everything built on its server library).
macrdp actually carries EGFX video over the tunnel — so, **as far as is known, it is
the first open-source RDP server with a working UDP multitransport data path**
(verified 2026-06-26; "first" can't be proven exhaustively — read it as "first
known"). The reason this gap existed so long is exactly point (2) above: the
server-only glue (UDP listener, cookie→session binding, channel migration via
`drdynvc`, sender-side reliability) is the part FreeRDP left as a stub and that no
spec walks you through — and FreeRDP never finished the *client* UDP path either,
so there was no OSS implementation of any of it to crib from.

### Which clients actually take macrdp's UDP offer

The UDP path only matters for clients that *open a UDP flow* in response to the
Initiate Multitransport Request. Coverage:

| Client | Opens UDP against macrdp? | Notes |
|---|---|---|
| **Windows mstsc** | ✅ yes | The only verified consumer of macrdp's UDP data path (EGFX over reliable RDPEUDP; lossy audio over UDPFECL). All real-client UDP verification is on mstsc. |
| FreeRDP (`sdl-freerdp`) | ❌ no | Consumes the offer but declines UDP (`E_ABORT` / `multitransport_no_udp`) → graceful TCP fallback. |
| **macOS Windows App / Microsoft Remote Desktop** (`com.microsoft.rdc.macos`) | ❌ no | **TCP-only against a generic RDP host.** It *has* a reliable-UDP/multitransport stack, but it's wired exclusively to **RDP Shortpath**, which is **Azure Virtual Desktop / Windows 365 / Dev Box only** (bootstraps over the AVD reverse-connect gateway + STUN/TURN, not a host's MS-RDPEMT offer; added macOS v10.9.0, Aug 2023; unchanged by the "Windows App" rename). So it never opens a UDP flow against macrdp → always TCP fallback. Confirmed via Microsoft's "Compare Remote Desktop client features" table (UDP appears only as RDP Shortpath rows, AVD pivots only). |

**Consequences:** (a) the whole UDP path — and the lossy-audio / 1+1 redundancy
work — is an **mstsc-only win** today; the Mac client and FreeRDP always run over
TCP. (b) You **cannot** capture RDPEUDP FEC from the Mac client (no UDP on the
wire) — the FEC-capture revisit needs a Windows **mstsc** client (against a legacy
Windows *server* for the parity packets; see "P2.3 FEC — future revisit").

## Soak testing the UDP path under loss

EGFX-over-UDP has only been verified on a clean WiFi/LAN so far — which proves it
*works* but not that it delivers the actual win (no head-of-line blocking on a
lossy/high-latency link). This section is the protocol for soaking it under emulated
loss. Loopback can't reproduce real loss, so this needs a **real client** (mstsc on
another machine) and the Mac's built-in `pf`/`dnctl` traffic shaper.

**Tooling:** `scripts/netshape.sh` shapes both directions of TCP+UDP on the macrdp
port (default 3390) via dummynet — `sudo scripts/netshape.sh on --loss 5 --delay 100`
to apply, `sudo scripts/netshape.sh off` to restore, `status` to inspect.
`scripts/soak-lossy.sh` is a turnkey server runner for the **lossy-delivery** soak (sets
the lossy env gates, captures the full log, live-prints the handshake markers) — see the
"P2.2 lossy-delivery soak (runbook)" subsection below.

**Observability:** the reliable transport's RTO retransmits are surfaced (the state
machine is sans-I/O, so `StepOutput.retransmits` / `.syn_retransmit` are counted in
`ironrdp-rdpeudp` and *logged by the listener*). Run the server with:

```
RUST_LOG=info,ironrdp_server::multitransport::listener=debug
```

and watch for:
- `RDPEUDP RTO retransmit` / `… (outbound)` / `… (timer)` — recovery firing
  (inbound-driven / server-data / periodic-timer path). **Zero on a clean link**; a
  steady trickle under loss is the system working as intended, not a fault.
- `RDPEUDP reliable data delivered` — in-order bytes reassembled.
- `RDPEUDP DATA datagram delivered nothing (receive-sequence mismatch)` — an
  inbound gap (loss/reorder) the receiver is holding for.

**A/B protocol — the comparison that shows the win:**
1. Apply the same shaping for both runs, e.g. `--loss 5 --delay 100`.
2. **Run A (EGFX on UDP):** start macrdp `--enable-udp-multitransport --enable-h264`
   with `MACRDP_UDP_MIGRATE_EGFX=1`. Connect mstsc, drive video (scroll a page,
   play a non-DRM clip), and type continuously.
3. **Run B (EGFX on TCP):** same flags but **without** `MACRDP_UDP_MIGRATE_EGFX`
   (EGFX stays on TCP — the safe spike). Same client activity.
4. Compare: in Run B a lost segment on the shared TCP stream stalls video *and*
   input together; Run A should keep video moving and input responsive because the
   video channel is independent of the control TCP. Repeat across loss steps
   (0% → 2% → 5% → 10%) and a couple of latencies (+60ms, +150ms).

**What to record per cell:** subjective video smoothness + typing latency, the
retransmit-log rate, and whether the session ever stalls/disconnects. Known thing to
probe specifically: on a **static screen** under loss (no SCK frames flowing — see
the flush-burst quirk) the state machine is only pumped by inbound datagrams, so if a
trailing video segment *and* the client's follow-up ACKs are both lost, recovery can
be delayed until the next screen change. If that shows up, the fix is a periodic
timer tick driving `step(now, None)` in the listener — deferred until the soak proves
it's needed.

### First soak findings (2026-06-26, mstsc, 5% loss + 100 ms/dir)

Two distinct problems surfaced — the first fixed, the second a real limit of
reliable-only multitransport.

1. **Idle-retransmit deadlock — FIXED (the timer tick above, now landed).** At 5%
   loss the *first run froze with a blank screen*: the SM was only pumped by inbound
   datagrams / new outbound data, so the initial EGFX burst's lost segments were
   never retransmitted, the in-flight window filled, and everything wedged (13 s of
   inbound silence, zero retransmit logs). The deferred periodic timer (¼ RTO,
   pumping every established peer with `step(now, None)`) was implemented in
   response; the full screen then rendered. **So the timer is not optional — any
   lossy link needs it.**

2. **Steady-state freeze under a high-volume stream — a reliable-transport limit,
   not a bug.** After rendering, the session still froze in steady state. Root
   cause is structural: the EGFX channel rides **one reliable, *ordered* RDPEUDP
   stream**, which has the *same head-of-line blocking as TCP* — a single lost video
   segment stalls every byte behind it until it's retransmitted. Compounding it in
   this test: (a) the client requested **1920×1080 on a 1512×982 Mac**, which forces
   **full frames every tick, no damage rects** (the `configured size != native …
   full frames every tick` warning) — a huge data volume that hits 5% loss
   constantly; and (b) the H.264 encoder is throttled to `max_in_flight=2`, and its
   credit is released by the client's **frame-acknowledge** PDUs *riding the same
   reliable tunnel inbound* — which HOL-block under the same loss, starving the
   encoder to a stop (observed: a flood of bare client ACKs, slow 48-byte inbound
   frame-ack deliveries, then no new frames).

   **The takeaway reframes the value prop:** reliable-only UDP multitransport
   *isolates video from other channels' loss*, but the video stream still
   HOL-blocks on its **own** loss — so it does **not** beat TCP for video on a lossy
   link. The genuine loss-resilience win needs **Phase 2: the lossy `UdpFecL`
   transport + FEC**, where video tolerates loss without retransmit-blocking.

3. **CONFIRMED structural, not UDP-specific (isolation runs, 2026-06-26).** The
   isolation A/B settled it: at **1% loss + 100 ms/dir, EGFX-on-*TCP* (flag off)
   froze too** — partial screen then stall, the same as UDP. Since the proven TCP
   path stalls under identical shaping, the freeze is **H.264/EGFX-under-loss
   itself, on either transport**, not a multitransport bug. The mechanism is the
   same on both: a 1080p H.264 keyframe is many segments, so even 1% per-segment
   loss makes it near-certain *some* segment of a keyframe drops (`1 − 0.99^N`),
   and an **ordered** stream (TCP or reliable-RDPEUDP alike) head-of-line-blocks
   every byte behind it; with a ~200 ms RTT and a continuous 60 fps stream,
   retransmit recovery can't keep pace → freeze. The UDP log shows the micro-event
   directly: `delivered nothing (receive-sequence mismatch) expected=…424 got
   425,426,427,428` then recovery on the client's CWR retransmit. **Decision
   (2026-06-26): accept Phase 1 as a clean-link / low-loss feature** (it works, and
   is verified, on a clean link; it remains default-OFF + `MACRDP_UDP_MIGRATE_EGFX`-
   gated). Lossy-link video is explicitly **out of scope for reliable-only** and is
   what Phase 2 (lossy `UdpFecL` + FEC) exists for — pursued only if/when lossy-link
   video becomes a goal. Note this also means the **default TCP** EGFX path has the
   same loss sensitivity (matching client resolution to avoid scaling + a capable
   client are the practical mitigations there).

4. **The under-loss freeze is permanent (no recovery after loss stops) — expected,
   do NOT re-investigate (2026-06-28, mstsc, 8% loss via clumsy, reliable tunnel,
   `MACRDP_UDP_MIGRATE_EGFX=1`).** A clean ~1-minute run then 8% drop reproduced the
   structural HOL-block of item 3, plus a terminal state worth recording so it isn't
   mistaken for a bug later: a loss burst lands on a ~234 KB IDR (~200+ segments);
   the reliable SM's unacked window fills and **pegs at exactly 1024**, resending the
   whole backlog every RTO (`RDPEUDP RTO retransmit (timer) retransmits=1024`), while
   macrdp keeps shipping a fresh 240–270 KB periodic IDR every `--keyframe-interval`
   (2 s) into the jammed tunnel. The client sends **CN (congestion notification)**,
   then **stops acking entirely** and abandons the UDP tunnel (the last inbound
   datagram precedes ~108 s of dead retransmits in the log); **audio keeps playing
   because it rides TCP.** Because a *reliable* ordered stream cannot drop the stale
   backlog (it would break the in-order contract) and the client never re-engages the
   abandoned tunnel, **EGFX stays frozen even after loss stops** — there is no
   server-side recovery path on a reliable stream. This is inherent to reliable-only
   multitransport, not fixable in the transport; the answer is Phase 2 (lossy
   `UdpFecL` + FEC). (Unrelated bug fixed the same day: a lock-order inversion in the
   H.264 ship pipeline that froze EGFX on a *clean* link — see PR #78 / the
   `ship_frames` `ctx`↔`server_handle` ordering. That one was a real deadlock; this
   item is the expected lossy-link behavior.)

5. **Real Windows server ↔ mstsc under the SAME ~8% loss degrades GRACEFULLY
   (frame-skip, not freeze) — observed 2026-06-28; the missing reference data
   point.** Every capture above is mstsc ↔ *macrdp*; this doc had flagged that we
   have **zero** capture of mstsc against a *real Windows server* under loss. An
   informal A/B over a bridged network (clumsy ~8% drop, mstsc → a real Windows RDP
   server) closed that gap: the video **skips frames / drops to a low framerate but
   keeps moving**, audio runs (occasionally rough) — it **never permanently freezes**
   the way macrdp does. Crucially this is on **RDPUDP2-reliable (no FEC)** — so FEC is
   **not** what makes Windows graceful (final nail; see the P2.3 NO-GO). The
   difference is entirely **server-side adaptation macrdp lacks**: (a) **URCP
   congestion control** — the server measures loss+RTT and throttles H.264
   bitrate/framerate to fit the degraded path, so the reliable stream's retransmit
   backlog stays small + recoverable instead of pegging the window at 1024; and (b)
   **encoder-side frame dropping** — it drops frames at the encoder (→ the visible
   "skipping") rather than flooding a fixed ~60 fps + 2 s IDR into the tunnel the way
   macrdp does. Same reliable transport, opposite outcome, purely from rate
   adaptation. mstsc itself does nothing special — no client-side TCP fallback, no
   re-engage (finding #4); the graceful degradation is **all server-side**.
   **Roadmap consequence:** the real Phase-2 loss-resilience lever is
   **congestion-responsive rate control + encoder frame-dropping**, NOT FEC (dead,
   P2.3) and NOT auto-fallback (a band-aid). And because the freeze is ordered-stream
   HOL-block on *either* transport (finding #3), rate-adapting the encoder would make
   EGFX-under-loss degrade to "choppy" on the **default TCP path too**, not just UDP —
   making it the highest-value video-under-loss work, above anything UDP-specific.
   (A proper *capture* of real-server↔mstsc — to read the exact URCP rate-control
   signaling on the wire — is still worth doing before implementing.)

   **SHIPPED 2026-06-29 — EGFX-over-UDP → TCP watchdog (the "auto-fallback band-aid",
   built anyway because it's cheap and removes the *permanent* freeze).** While
   rate control (above) is the real fix, it doesn't help a tunnel that has *already*
   wedged/abandoned — that stays frozen until reconnect. The watchdog catches exactly
   that: when EGFX is on the **reliable** UDP tunnel and frame acks go silent for
   ~3s (`MACRDP_UDP_EGFX_WATCHDOG_MS`) while the server is still actively shipping
   (the #89 trickle floor guarantees it ships even when ack-lag is high), it declares
   the tunnel wedged and routes EGFX back onto **TCP** + forces an IDR (the last UDP
   frames never arrived, so the client's reference is stale). mstsc renders EGFX on
   TCP after a Soft-Sync — established first by a throwaway *timed* de-migration
   ("Spike A", flip `egfx_on_udp` false → TCP, no reverse Soft-Sync needed, verified
   live), then the full ack-stall-driven watchdog. Pure predicate
   `should_demigrate_to_tcp` in `src/h264.rs` (8 unit tests); the server route arm
   reads a shared `demigrate_request` flag and flips routing (one-way per connection;
   reset on reconnect). Default-on but a strict no-op off the reliable UDP tunnel.
   **Verified two ways on real mstsc:** (1) a deterministic clean-link injection
   (drop acks after N s) fired at exactly `since_ack_ms=3004` and recovered; (2) a
   genuine **clumsy UDP-only loss** wedge (UDP dropped, TCP left healthy as the
   fallback) fired at `since_ack_ms≈7736` — *real* wedges dribble a few stray acks
   before going fully silent, so the pure-silence trigger latches later than the
   injection's instant silence — and EGFX recovered on the healthy TCP channel, no
   permanent freeze. Follow-up (deferred): an **ack-lag-pegged secondary trigger**
   (fire when shipped−acked stays pinned for N s even if odd acks trickle in) would
   shorten the ~7–8s real-wedge recovery window toward the 3s ideal. NOTE this is
   complementary to — not a substitute for — congestion-responsive rate control:
   the watchdog *escapes* a dead tunnel; rate control *prevents* the wedge and also
   helps the TCP path.

   **SHIPPED 2026-06-29 — rate control P1: adaptive bitrate (`--adaptive-bitrate`).**
   The first piece of the congestion-responsive controller. Loss signal: the UDP
   listener accumulates reliable-tunnel **retransmits** into a shared counter
   (`Arc<AtomicU64>`, threaded through `bind`/`run_recv_loop`); the H.264 controller
   (`src/h264.rs::adaptive_bitrate_step`, pure `aimd_bitrate`, unit-tested) samples
   the per-interval delta and runs **AIMD**: multiplicative-decrease the VideoToolbox
   target toward a floor on any loss, additive-increase toward the `--bitrate` ceiling
   when clean. Lever: `Encoder::set_bitrate` live-sets `AverageBitRate` on the running
   VT session (no rebuild → reference chain intact). Opt-in, EGFX-over-UDP only (no-op
   on TCP). **Verified on real mstsc under clumsy UDP-only loss: at 8% the bitrate
   dropped and video stayed alive with NO wedge** (the controller kept the tunnel under
   its window — the win); at sustained 12% it still wedged → watchdog → TCP. Uses the
   retransmit signal only; CN/RTT/window and the levers below are P2/P3. **Follow-up
   found in the same test:** under *sustained* heavy loss, ~60s after a watchdog
   de-migration mstsc resets the session (its multitransport dead-tunnel timeout on the
   now-silent UDP tunnel) — fix is to keepalive or cleanly close the abandoned tunnel
   on de-migrate (TODO).

   **SHIPPED 2026-06-29 — rate control P2a: IDR backoff + the ack-lag signal switch.**
   Two changes that ship together. (1) **IDR backoff** (`idr_backoff_decision`, pure +
   unit-tested): a ~240 KB periodic keyframe is the worst thing to inject into a
   congested ordered tunnel (it's what wedges it), so on congestion the controller
   stretches `MaxKeyFrameInterval` to ~10 min (`Encoder::set_keyframe_interval`,
   live-settable, no rebuild) — suppressing the periodic IDR — and restores it +
   forces one recovery IDR on full recovery. Safe on the reliable tunnel: reliable
   delivery means there's no loss-corruption to heal, so the periodic IDR is only a
   decode-glitch safety net that's deferrable while congested. (2) **THE FIX that made
   P1 *and* P2a actually fire on a lossy link:** the original retransmit-counter signal
   only climbs after an RTO (~one RTT), which is **strictly slower than the watchdog's
   3 s ack-silence trigger** — so on every lossy session the watchdog de-migrated to
   TCP (`egfx_on_udp→false`) *before* the first retransmit, and the controller shut off
   having never seen loss (observed live: watchdog at T+11.2 s, first retransmit
   +292 ms *after* it; zero bitrate adjustments all session). Switched the primary
   congestion signal to the client's **frame-ack lag** (`shipped − acked`, the same
   fast signal the watchdog + backpressure gate already compute) — it rises the instant
   the client stops acking, so the controller now reacts **~2.6 s before** the watchdog.
   Pure `controller_congested` (lag > threshold with acks flowing, OR a retransmit) +
   4 unit tests; retransmit delta kept as a secondary late signal. Also restores the
   normal keyframe interval when EGFX leaves the UDP tunnel (watchdog de-migrate) while
   backed off, so the TCP path doesn't inherit the 10 min stretched interval.
   **Verified on real mstsc under clumsy UDP loss:** clean baseline held 11 s at the
   3 Mbps ceiling with no false positives; on loss the controller rode 3 M → 500 k floor
   in ~1.7 s, suppressed the keyframe, and the live `MaxKeyFrameInterval` changes did
   **not** break the stream; video recovered cleanly on the TCP de-migrate.
   **Important scope finding:** on a link *this* lossy (clumsy + WiFi baseline) the
   tunnel still wedged even at the 500 k floor — a reliable **ordered** stream HOL-blocks
   on a single unrecovered packet regardless of bitrate (finding #3/#4), so no bitrate
   is low enough; the controller's win is real only in the **moderate**-loss regime
   (keeps the tunnel under the wedge threshold, as P1 showed at 8 %), and the
   watchdog→TCP backstop covers the rest. Remaining: CN/RTT/window signals, frame-drop
   (P2b), the TCP-path adapter (P3).

   **SHIPPED 2026-06-29 — rate control P3: the controller runs on the TCP path too.**
   The same AIMD + ack-lag signal, now active while EGFX is on TCP (not just the UDP
   tunnel) — so a user can set a high ceiling (`--bitrate 8`) and have it back off only
   when the link struggles, climbing back when clear, on the path every client uses.
   Key realization: the `shipped − acked` ack-lag signal works on TCP because EGFX
   `FrameAcknowledge` flows on the TCP DRDYNVC channel AND the `ServerEvent` ship
   channel is **unbounded** (`shipped` advances immediately; socket backpressure lives
   inside the vendored server), so the lag reflects the real encoder→socket→client
   backlog. **Characterized on real mstsc under clumsy TCP loss** (this took correcting
   a test error — clumsy was first run with a *UDP* filter on a pure-TCP session, which
   dropped nothing): healthy ack-lag is 0–4; under 5 % TCP drop it climbs to **~40 in
   bursts** — a real but *spiky, lower-amplitude* signal vs UDP's 25–33 sustained (TCP's
   own reliability + flow control smooth loss out). Tuning that matters: a transport-
   specific threshold — default **¾·`max_frame_lag` (=12)**, `MACRDP_TCP_ADAPTIVE_LAG_THRESHOLD`
   — catches genuine spikes (14–40) while ignoring the 0–12 jitter. At the UDP default
   (16) it missed real congestion; at a diagnostic 2 it pumped the bitrate 1.3 M↔8 M
   (visibly). At 12 it's an infrequent, gentle **8 M↔5.6 M** sawtooth on real spikes,
   verified smoother on screen. Plus a **cold-start guard** (`ADAPTIVE_WARMUP`, 2 s):
   ignore the ack-lag signal for 2 s after the first ack so the connect-time startup
   backlog (`ack_lag`~25 before the client starts acking) doesn't dip the bitrate at
   session open — verified to give a clean connect. **IDR backoff stays UDP-only** (on
   reliable TCP the periodic keyframe is cheap decode-glitch insurance, not worth the
   false-suppress risk). The UDP→TCP de-migrate snaps to the ceiling once (instant
   recovery) then the TCP controller manages from there. **Residual** (accepted): a
   slight dip + brief catch-up speed-up on a congestion spike — inherent to rate-adaptive
   video draining its buffer; EWMA smoothing/hysteresis is deferred polish. The next
   refinements (CN/RTT/window signals; `TCP_CONNECTION_INFO` for a stronger, less spiky
   TCP signal; frame-drop P2b) remain.

   **SHIPPED 2026-06-29 — EWMA smoothing + hysteresis + 3-zone hold.** The deferred
   polish, built after the user reported A/V drift + a catch-up speed-up under loss (the
   raw ack-lag is spiky — TCP bursts 0↔40 even at moderate loss — so a naive per-interval
   threshold pumped the bitrate, and the video then sped up to catch up while fixed-rate
   audio couldn't, drifting). Three parts, all pure + unit-tested: (1) **EWMA** the
   ack-lag (`α` default 0.3, `MACRDP_ADAPTIVE_EWMA_ALPHA`), fed only real samples (skipped
   during warmup/ack-suspend so the startup backlog can't pre-load it), so single bursts
   don't trip a back-off — only sustained high lag does. (2) **Hysteresis**
   (`congested_hysteresis`): enter at the high mark, stay until below half it — no
   flip-flop straddling one threshold; a retransmit still forces congested. (3)
   **3-zone hold** (`rate_action`): decrease above high, **hold** while the EWMA decays
   through the [low, high] band, increase once cleared — without the hold, a single spike
   kept decreasing every interval as the EWMA decayed back through the band, cratering the
   bitrate toward the floor ("video sometimes stops"); now a single spike = one step down
   then a plateau, while *sustained* congestion still rides to the floor. **Verified on
   real mstsc** (pure-TCP, `--bitrate 8`, 5 % clumsy drop): per-spike sawtooth → one gentle
   step-and-recover per genuine episode (dips bottomed at ~2.7–3.9 M vs the pre-hold
   1.0–1.9 M), **A/V noticeably more in sync, the catch-up speed-up cut, and "video
   sometimes stops" gone**. Applies to both transports. **Residual:** audio still *skips*
   under sustained drop (5 % loss hits RDPSND directly + the audio-lag model drops stale
   waves to keep sync — the skip is the cost of the better sync; lever B / audio-resync
   tuning, deferred — see the `project_av_sync_under_drops` note).

   **SHIPPED 2026-06-29 — proactive de-migrate on minimize/restore (PR #99).** User
   reported: with `--udp-migrate-egfx`, minimizing the mstsc window then restoring it
   froze the video until a disconnect + reconnect to a *fresh* mstsc (audio, on TCP,
   resumed fine); `UDP_MIGRATE_EGFX=0` handled minimize/restore cleanly. Root cause:
   once EGFX is Soft-Sync-migrated onto the reliable tunnel, mstsc's surface doesn't
   survive the SuppressOutput→restore cycle, and the wedge watchdog only de-migrates
   *reactively* — 3–6 s after restore (log: `since_ack_ms=6641`), after we've already
   shipped the restore frames into the now-stale tunnel, too late to heal the surface.
   Fix: on the un-suppress (restore) edge, if EGFX is on the reliable UDP tunnel,
   **proactively de-migrate to TCP before shipping a single frame back into the tunnel**
   (`Gfx::demigrate_on_resume`, called from `capture.rs`'s `was_suppressed` edge), so the
   restore IDR lands over TCP. Mirrors the watchdog's de-migrate steps; one-way per
   connection; no-op on TCP/default/lossy (`egfx_on_udp && !egfx_on_lossy`). **Verified
   on real mstsc** (`--udp-migrate-egfx --bitrate 6 --adaptive-bitrate`): minimize→restore
   → video returns; log shows `client resumed from minimize while EGFX was on the reliable
   UDP tunnel — proactively de-migrating to TCP`. Confirms the freeze was the *transport*,
   not the unfixable reconnect-blank surface bug. See the quirk note in `docs/known-quirks.md`.

   **SHIPPED 2026-06-29 — UDP retransmit tolerance (PR #100) + a disproven hypothesis.**
   User reported EGFX-over-UDP bitrate getting "worse and worse over time" on WiFi6 with
   no recovery. Hypothesis: the controller treated *any* reliable retransmit in a 300ms
   interval as congestion (multiplicative Decrease), while an Increase needed a *zero*-
   retransmit interval — and a wireless link's near-continuous low-level loss rarely gives
   one → one-way ratchet to the floor. Fix: a per-interval retransmit **tolerance**
   (`MACRDP_UDP_ADAPTIVE_RETX_TOLERANCE`, default 2) — only `retransmit_delta > tolerance`
   counts as loss (new pure helper `retransmit_is_lossy`; `congested_hysteresis`/
   `rate_action` now take `retransmit_lossy: bool`). UDP-only; `tolerance=0` restores the
   old behaviour. **But the live mstsc/WiFi6 test DISPROVED the hypothesis for that link:**
   the actual decreases were **ack-lag-driven** (ack_lag climbed 19→34, `retransmit_delta=0`
   on *every* decrease 6.0M→750k) → the tunnel was *wedging* (HOL-block, cause #2), not
   ratcheting on retransmits → watchdog correctly de-migrated to TCP → video recovered. So
   the retransmit signal was ~always 0 on that link and the tolerance never engaged. The fix
   is kept as a correct, low-risk robustness improvement for links that *do* register
   retransmits, NOT as the WiFi6 cure — for a link lossy enough to wedge the ordered tunnel,
   `UDP_MIGRATE_EGFX=0` (TCP) remains the answer; the real loss-resilience fix is Phase 2
   (lossy + FEC). Lesson: the reliable-tunnel retransmit counter barely fires in practice —
   **ack-lag is the dominant (effectively sole) congestion signal** on both transports.

   **SHIPPED 2026-06-29 — rate control P2b: frame-rate floor (PR #102, verified mstsc).**
   Design lever (2) ("drop frames for lower effective fps when bitrate cuts aren't enough").
   Once the controller has cut the bitrate to its floor AND the link is still congested —
   lowering quality can no longer help — the next lever is shedding *frames*. New pure
   `frame_drop_at_floor(at_floor, congested, since_last_pass, min_interval)` (2 unit tests),
   gated in `submit_bgra` on `--adaptive-bitrate` + actually at-floor-under-congestion (so
   the default path is byte-unchanged): it caps the effective fps by dropping captures
   arriving within `1/floor-fps` of the last let-through (default 10 fps,
   `MACRDP_ADAPTIVE_FLOOR_FPS`). **Never zero** — one capture per interval gets through so
   the client keeps trailing frames to present/ack (the same lesson as the EGFX-on-UDP
   trickle floor; dropping to zero pins the ack-lag and freezes). Works on **both
   transports** — it's the only fps lever on TCP (no UDP frame-ack backpressure gate there).
   Dropping before encode keeps the H.264 reference chain valid; `need_keyframe` persists
   across a drop. **Verified on real mstsc** (pure-TCP, `--bitrate 6`, ~8–10% clumsy TCP
   drop), confirmed end-to-end from the log: bitrate AIMD walked down to the 750k floor
   under rising ack-lag (34→53) → P2b engaged (324× `frame-rate floor active — capping fps`)
   → video settled to a controlled ~10 fps (choppy-but-steady, **in sync** with audio, no
   freeze) → when loss cleared, bitrate climbed back to the 6M ceiling and full fps resumed;
   0 de-migrate/wedge (video on TCP). The residual recovery catch-up (rate-adaptive video
   draining its buffer to re-sync to audio) is the separate, deferred audio-resync "lever B".
   Remaining rate-control refinements: a stronger/less-spiky TCP signal
   (`TCP_CONNECTION_INFO` RTT+retransmits / write-backpressure), the CN/RTT/window signals,
   and tuning against a real-Windows-server capture.

   **What "URCP" actually is, and the concrete signals macrdp already ignores.**
   URCP = **Universal Rate Control Protocol** (Microsoft Research, ~2013) — the
   congestion-/rate-control *algorithm* under RDP Shortpath + MS-RDPEUDP2. It's not a
   single wire field; it's a controller that estimates path bandwidth + delay and
   **paces the sender to a target rate** (delay-based, real-time-tuned — it backs off
   *before* loss rather than TCP's loss=halve). The point for macrdp: we do **not**
   need to reimplement URCP — we need *a* controller reading the same feedback that
   **already arrives in the RDPEUDP ACK stream and that macrdp currently throws away**:
   - **Explicit congestion bit** — `RDPUDP_FLAG_CN` (congestion notification; the
     sender replies `RDPUDP_FLAG_CWR`). **This is the exact "CN" mstsc sent in finding
     #4 right before it abandoned the tunnel — macrdp received it and did nothing.**
   - **Loss** — gaps in the `RDPUDP_ACK_VECTOR_HEADER` (+ retransmit events).
   - **RTT / queuing-delay trend** — from ACK round-trip timing (and RDPEUDP2 ack
     timestamps / AckOfAcks); rising RTT = growing queue = back off early.
   - **Flow-control ceiling** — the peer's advertised `uReceiveWindowSize`.
   A simple delay+loss+CN controller driving the VideoToolbox bitrate/fps down (and
   backing off the periodic IDR) already turns the freeze into graceful degradation;
   matching URCP's exact algorithm is a refinement, not a prerequisite. (Refs: MS
   Research "URCP: Universal Rate Control Protocol for Real-Time Communication";
   RDP Shortpath docs.)

   **Can we implement URCP — and what should we actually implement? (scoped 2026-06-30.)**
   Yes in principle, because URCP is a **sender-side rate-control *algorithm*, not a wire
   protocol** — macrdp is the EGFX sender (server→client), so it needs **no client
   cooperation and no interop**: mstsc doesn't have to "speak URCP"; the controller just
   consumes the RDPEUDP feedback we already receive and decides a send rate. Microsoft
   already proved the shape fits — URCP-on-RDPEUDP2 *is* "a real-time media CC bolted onto
   an RDP UDP transport." But two conclusions:
   - **Do NOT implement URCP-the-algorithm.** There is no public URCP reference
     implementation, and the MS-RDPEUDP2 open spec only says it uses "URCP-based rate
     control" *without specifying the algorithm* — so URCP itself is **outside the Open
     Specifications Promise**, a patent/licensing gray zone for an MIT/Apache project.
     Use an open, royalty-free, RFC'd controller that does the same job and has reference
     code: **SCReAM** (RFC 8298, self-clocked, built for RTP video), **NADA** (RFC 8698),
     or **GCC** (Google Congestion Control, libwebrtc).
   - **Which fits RDP best.** These are transport-agnostic; the RTP/RTCP parts are just
     input/output *adapters* we'd swap for RDP equivalents. Fit ranking on RDPEUDP feedback:
     **SCReAM fits best** (self-clocked off acks + one-way-delay + loss — matches the
     ACK-vector + RTT feedback RDPEUDP actually gives); **NADA** similar; a plain
     **loss+RTT hybrid** (Copa/BBR-lite) also natural. **GCC fits *worst*** despite being
     the most battle-tested — its delay controller is built around **transport-wide
     per-packet arrival timestamps (TWCC / RFC 8888)**, which RDPEUDP does not natively
     produce, so its main input would have to be reshaped/approximated.

   Signal-by-signal mapping onto macrdp/RDPEUDP (what's native vs approximated):
   | Controller input | RDPEUDP source | Fit |
   |---|---|---|
   | Loss | `RDPUDP_ACK_VECTOR_HEADER` gaps + retransmits | native ✅ |
   | RTT | ACK round-trip timing (sender-side) | good ✅ |
   | Delay gradient / OWD trend | RDPEUDP2 ack timestamps (our v1 reliable tunnel → RTT-trend only) | partial ⚠️ |
   | Per-packet receiver arrival times (TWCC) | not how RDPEUDP feedback is shaped | approximate ❌ |
   | Explicit congestion | `RDPUDP_FLAG_CN` (reply `RDPUDP_FLAG_CWR`) | native ✅ |
   | Actuation: target bitrate + pacing + drop | live VideoToolbox bitrate + frame-drop + IDR backoff (already wired for `--adaptive-bitrate`) | native ✅ |

   Two implementation notes that matter more than the algorithm choice:
   - **Feed it from the *transport* acks, not the GFX frame-acks.** Today `--adaptive-bitrate`
     reads the EGFX `FrameAcknowledge` lag — coarse (one signal per video frame). A real
     SCReAM/URCP-style controller wants the far-more-frequent **RDPEUDP datagram-level acks**
     (already parsed in vendored `ironrdp-rdpeudp`) for a usable delay/loss signal.
   - **Re-implement the control law in Rust** (a few hundred lines — SCReAM's core
     especially); do **not** link libwebrtc/GCC (a huge C++ dependency). Inputs from the
     RDPEUDP feedback we already parse; outputs to the VT bitrate/frame-drop levers we have.

   **The substrate still gates the payoff (same caveat as the rest of finding #5).** All of
   these CCs assume a **droppable** media flow — you pace, and late packets are simply
   dropped. macrdp's EGFX rides a **reliable, ordered** tunnel today, where a better
   controller **cannot stop the freeze**: you still head-of-line-block on a lost packet, and
   you'd effectively run congestion control twice (over the reliability layer's own
   retransmit/window). They only deliver their benefit once EGFX moves to a **lossy flow +
   encoder frame-drop** (the deferred Phase-2 lift; mstsc lossy-*video* acceptance is
   unverified — the lossy path is proven for audio only, and Mac/FreeRDP clients are
   TCP-only, so the audience is mstsc-only). On the reliable tunnel / TCP path, the upgraded
   controller still improves *graceful degradation*, just not the UDP-under-loss freeze.

   **Verdict:** the controller swap (AIMD → SCReAM/loss+RTT, driven by transport acks) is a
   modest, well-scoped win on both transports and the right thing to do *if* this is picked
   up; "implement URCP by name" is not (licensing + no reference impl). But it is the *easy
   half* — the freeze fix still needs the lossy-video substrate underneath it, so until that
   exists, `UDP_MIGRATE_EGFX=0` + the watchdog→TCP backstop remain the robust answer. (Refs:
   SCReAM RFC 8298; NADA RFC 8698; Google Congestion Control / libwebrtc.)

### M3c peer GC — the listener leaked dead peers (fixed 2026-06-28)

A separate, transport-level correctness bug surfaced from a real mstsc pcap
(EGFX-over-UDP, `--udp-migrate-egfx`, WiFi): on **reconnect the video went blank**,
and the user noticed **"the UDP connection continues even after I close the client"**
(recovery needed a server restart). The pcap confirmed it: after the client's TCP
session RST'd, the server kept shipping UDP (EGFX retransmits) to the gone client for
the rest of the capture (~10 s / 32 packets and still going).

Root cause: the listener's `peers: HashMap<SocketAddr, Peer>` was inserted-but-never
removed — the module doc literally said *"never garbage-collected … GC + idle timeout
come with M3c"*, and M3c's GC half was never built. So a client whose RDP/TCP session
went away kept its peer entry forever, and `pump_peers_on_timer` kept RTO-retransmitting
unacked EGFX to it; over a long-running server dead peers also accumulate unbounded.

Fix (`vendor/ironrdp-server/src/multitransport/listener.rs`, idle-timeout GC):
`Peer` gained `last_seen_ms`, bumped on **every inbound datagram** (only inbound — a
dead peer still *sends* outbound retransmits but receives nothing, so its clock
stops); `gc_idle_peers` runs on the existing `retransmit_tick` (right after
`pump_peers_on_timer`) and evicts any peer idle > `PEER_IDLE_TIMEOUT_MS`,
dropping its `bound_addrs` cookie→addr mapping too. Activity-based, so it covers
graceful / abrupt / crashed disconnects uniformly. Logs `evicted idle UDP peer` at
`ironrdp_server::multitransport=debug`.

**Timeout corrected 2026-06-29: 10 s → 60 s (it was reaping live idle peers).** The
10 s rested on a wrong assumption — that a live client *always* sends frequent RDPEUDP
keepalive/delayed-ACKs (~200/s). That's only true while the picture is **active**. When
the screen goes idle, **mstsc drops to a ~15 s UDP keepalive cadence** (verified live:
inbound datagrams exactly 15 s apart once frame-acks stop). The 10 s GC then reaped the
peer **between** two keepalives — tearing down the UDP tunnel of a *fully live* TCP
session (audio kept flowing), so EGFX froze **permanently until reconnect** (the
client's next keepalive isn't a SYN, so the peer is never recreated). This is a distinct
bug from the load-freeze (#89): triggered by simply going idle for ~15–60 s, and it
happens on mirror-primary too. The trace was unambiguous — inbound floods at 100–450/s
during activity, then exactly one datagram every 15 s once idle, then `evicted idle UDP
peer idle_ms=10049`. Fix: raise to 60 s (4× the observed 15 s keepalive), so an
idle-but-live peer is never reaped while a genuinely dead one still ages out (the leak
becomes ≤60 s, not indefinite). **Verified on real mstsc**: a live peer idle ~45 s
recovered on activity with no eviction; only the *abandoned* peer from a prior reconnect
was reaped (idle_ms≈60042). The fully robust fix is an explicit TCP-session-close →
evict signal (the deferred half of M3c, below); until that lands the activity-based
backstop must stay generous enough not to reap an idle client.

This is the listener-only **backstop** half of M3c; the prompt server→listener
"instant retire-on-disconnect" signal is still deferred (the GC is needed regardless).
The GC was **verified on real mstsc** — UDP to the closed client stops ~10 s after
disconnect. **By itself it did NOT fix the reconnect-blank**, which turned out to be a
*separate* per-connection-state bug (next section).

### M3c reconnect state-reset — EGFX-over-UDP blank/black on the 2nd connection (fixed 2026-06-28)

After the GC landed, real-mstsc retest isolated the reconnect-blank precisely: with
`--udp-migrate-egfx`, the **first** connection rendered, but a **reconnect** showed a
blank desktop that went black (EGFX wedged after a frame or two). Crucially, **plain
TCP** EGFX reconnect (`--udp-migrate-egfx` off) was always clean — so it was
UDP-specific, not the capture-primary/window-relocation gotcha and not (only) the mstsc
surface-retention quirk (it reproduced even on a fresh mstsc process). Two
per-connection-state bugs on the *persistent* server + listener, both "set once on
connection 1, never reset for connection 2":

1. **Server (`server.rs`, the universal cause).** `egfx_on_udp` (set true at Soft-Sync,
   checked to route EGFX over UDP) — plus `lossy_audio_block_no`,
   `lossy_audio_streaming`, and the `egfx_on_lossy_handle` flag — were never cleared
   between connections (the single `RdpServer` is reused across the accept loop). So
   connection 2 started with `egfx_on_udp == true` and routed EGFX over a UDP tunnel
   that **its own** Soft-Sync hadn't bound yet (cookie unbound) → frames dropped, and
   nothing on TCP either → blank/black. Fix: reset these right after
   `self.static_channels = StaticChannelSet::new()` in `run()`. Connection 2 now keeps
   EGFX on TCP until its tunnel binds and re-fires Soft-Sync (clean migration; correct
   TCP fallback if the new tunnel never binds). (`multitransport_migration` /
   `udp_tunnel_bound` / the inbound rx were already refreshed per connection at the
   offer site — only these only-set-never-reset flags needed clearing.)
2. **Listener (`listener.rs`, the same-port case).** On a fast reconnect that reused
   the client's UDP source addr/port (within the 10 s idle-GC window), the
   `peers.entry(addr).or_insert_with` reused the **stale** established `Peer` —
   `tunnel_created` still true, `inbound_sink` still pointing at the gone connection's
   receiver — so `handle_emt_tunnel` skipped the new CREATEREQUEST (gated on
   `!tunnel_created`) and silently dropped connection 2's inbound EGFX acks. Fix:
   before the entry, if a **SYN** arrives on an address whose existing peer is already
   `is_established()`, it's a new flow on a reused port → remove the stale peer (+ its
   `bound_addrs` bindings) so a fresh one is built and the new tunnel binds cleanly.
   (A SYN on a still-handshaking peer is a normal SYN retransmit — `is_established()`
   gates it out.)

The robust WiFi config is still `--udp-migrate-egfx` off (the clean-link limit /
finding #4 stands — under loss the reliable tunnel HOL-blocks regardless); this fix is
about EGFX-over-UDP **reconnect** working at all on a clean link. See vendored server
divergence (12) "M3c reconnect state-reset".

**Verified on real mstsc, plus the residual it leaves.** After the state-reset fix, a
real-mstsc retest confirmed connection 2's EGFX pipeline matches connection 1's
(encoder init, surface created+mapped, frames shipped, **mstsc ACKing over the UDP
tunnel**), and a multi-cycle reconnect test **rendered and stayed responsive** (typing
and clicks work). So the fix works in the normal case. (An earlier mid-investigation
guess that the residual was the documented mstsc *surface-retention* quirk was
**disproven** — it reproduced on a fully fresh mstsc *process*, which has no retained
surface 0.)

**Residual — an intermittent EGFX frame/queue runaway (the rate-control gap, finding
#5).** The reconnect freeze still recurs **intermittently** under stress (rapid
repeated reconnects). The trace shows why: the server **never throttles on the client's
EGFX `queue_depth`** — `GfxHandler::on_frame_ack` only records ack timing; macrdp ships
at full rate regardless of how backed-up the client reports it is. The client's
`queueDepth` is already large even on a healthy first connect (~30k–82k) and on a bad
reconnect it **runs away** (observed peak **352k**) while the RDPEUDP layer floods pure
ACKs — the client falls hopelessly behind and the display freezes on a stale frame
(input still reaches the Mac over TCP, so it only *looks* dead). So `queue_depth` is an
unreliable freeze *predictor* (huge in both healthy and frozen states) but the
**runaway** is the failure mode. This is the same **"server ignores client
congestion/queue feedback"** gap captured in finding #5 / the rate-control TODO — it
bites hardest on reconnect, where the client starts more backed-up.

**FIXED 2026-06-28 — frame-ack-lag backpressure with a TRICKLE FLOOR (the first
focused piece of the finding-#5 rate-control work; verified on real mstsc under the
exact repro that froze: headless `--virtual-display --capture-primary`, YouTube +
rapidly held Cmd+Tab).** `src/h264.rs`'s `submit_bgra` now, on the UDP tunnel only,
tracks per-frame lag = `last_shipped_frame_id − last_acked_frame_id` (the EGFX
FrameAcknowledge id, recorded in `on_frame_ack`) and, when it exceeds
`MACRDP_UDP_EGFX_MAX_FRAME_LAG` (default 16), **drops most captures so the client
catches up** — capping the queue runaway. The load-bearing subtlety (and the bug in
the first cut): it must **NOT drop to zero**. mstsc only *presents* — and therefore
*frame-acks* — an H.264 frame once a couple more arrive behind it (the same
presentation-buffer behaviour the `--flush-frames` burst feeds). With zero trailing
frames the client never acks the in-flight ones, `lag` stays pinned one over the
threshold, every capture is dropped, and the video **freezes permanently** (recovers
only on reconnect — exactly the reported symptom). The fix keeps a low-rate **trickle**
(`UDP_THROTTLE_FLOOR`, ~10 fps) flowing while lag is high, so the client keeps
presenting + acking and the window reopens; dropping *before* encode keeps the H.264
reference chain valid (the next encoded frame is a P-frame from the client's last
reference). Net: under load the video degrades to **choppy-but-live** instead of
freezing.

*(Diagnostic dead-end, recorded so it isn't re-chased: a `sample` of the frozen
process shows the `egfx-ship` thread parked in `_dispatch_semaphore_wait_slow` — this
is NOT a stall. Rust's `std::sync::mpsc` implements its blocking `recv()` via a
libdispatch semaphore on macOS, so an idle ship thread waiting for the next encoded
frame looks exactly like that. The freeze was upstream of it — the capture-side gate
dropping every frame so nothing was ever encoded.)*

A fuller rate controller (continuous queue_depth-aware pacing, ideally informed by a
real-Windows-server capture since the raw `queueDepth` units are oddly large) remains
finding-#5 future work. Everyday robust config stays `--udp-migrate-egfx` off (and
`--fork-workers` gives clean in-process reconnect on the TCP path; it's mutually
exclusive with UDP multitransport).

### P2.2 lossy-delivery soak (runbook)

The first soak (above) shaped the **reliable** EGFX-over-UDP path. This one targets the
**lossy delivery policy** (P2.2 steps 1–2): a `SYN_LOSSY` flow driving
`DeliveryMode::Lossy` (deliver-on-arrival, **send-once / no retransmit**). It's verified
green on a *clean* link (DTLS handshake + MS-RDPEMT tunnel reach established); the soak
exists to find where send-once breaks under real loss.

**What it probes (the known caveats):** because the lossy SM never retransmits, a dropped
handshake datagram isn't recovered by the transport. Specifically:
1. the server's one-shot **`CREATERESPONSE(S_OK)`** — if it's lost, the listener's
   `tunnel_created` guard answers a repeated `CREATEREQUEST` only once, so the tunnel may
   never establish;
2. the **DTLS server flight** (ServerHello/Certificate/Done) — sent once by the SM; DTLS
   has its *own* flight retransmission (client-driven on timeout), so this *may* self-heal,
   but that's exactly the thing to confirm.

**Run it (two terminals, real mstsc on another machine):**
```
# 1) shape both directions of TCP+UDP on the port:
sudo scripts/netshape.sh on --loss 5 --delay 100

# 2) run the server under the lossy-delivery soak config (sets the env, captures the
#    full log to a file, live-prints only the soak markers):
scripts/soak-lossy.sh --enable-udp-multitransport --enable-h264 \
    --username "$USER" --password 'PASS' --bind 0.0.0.0:3390

# …connect mstsc, exercise it, then restore:
sudo scripts/netshape.sh off
```

**Read the markers** (`soak-lossy.sh` highlights them; the full log is in the file):
- `RDPEUDP peer using LOSSY delivery` — the lossy SM is engaged for that flow.
- `P2.1 GREEN` (DTLS complete) → `P2.4 GREEN` (tunnel established) — **both appearing =
  the lossy handshake survived the loss to established.** *Neither/only-one appearing,
  with the client repeatedly sending `CREATEREQUEST`, is the stall the caveats predict.*
- `RTO retransmit` lines should be **zero for the lossy flow** (it never retransmits); any
  you see are the *reliable* (`UdpFecR`) flow doing its job — don't confuse them.

**A/B (isolates "needs retransmission" from "loss simply too high"):** at the *same*
shaping, run once as above (`MACRDP_UDP_LOSSY_DELIVERY=1`, the default in the script) and
once with `MACRDP_UDP_LOSSY_DELIVERY=0 scripts/soak-lossy.sh …` (the lossy flow then rides
the **reliable** SM, which retransmits). If the reliable control reaches `P2.4 GREEN`
where lossy stalls, the stall is the missing handshake retransmission — not the link.
Sweep loss `0 → 2 → 5 → 10 %` and latency `+60 / +150 ms`; record at which cell lossy
first fails to establish and whether the reliable control still does.

**If lossy stalls under loss (expected at some loss level), the fix** is handshake-phase
robustness without giving up steady-state losslessness: make the `CREATERESPONSE`
**idempotent** (re-answer every `CREATEREQUEST` in lossy mode instead of gating on
`tunnel_created`), and/or drive DTLS's `handle_timeout` from the listener's existing
retransmit tick for a lossy peer so the server re-sends its own handshake flight. Both are
handshake-only — once the tunnel is up, data stays send-once. (Deferred until the soak
shows it's needed, per the project's "don't build it until the soak proves it" rule that
landed the reliable-path timer tick.)

### P2.2 lossy-delivery soak result (2026-06-27, mstsc, 5% loss + 100 ms/dir)

First run of the lossy soak. **The lossy handshake survives loss — the send-once caveat
did NOT bite at this level.** The log shows it directly: the client's first DTLS flight
(190-byte ClientHello) was **delivered twice** (`delivered=190 total=190` then
`…total=380`) — a retransmitted flight, because loss delayed our reply — and in lossy mode
we deliver-on-arrival without dedup, DTLS dropped the replay itself, and the handshake
still reached `P2.1 GREEN` → `P2.4 GREEN` (DTLS + MS-RDPEMT tunnel established). So DTLS's
own flight retransmission plus our deliver-on-arrival carry the handshake even though the
RDPEUDP SM never retransmits. The predicted `CREATERESPONSE`-lost stall did not occur at
5% (the response got through first try); whether it bites at higher loss (10%+) is still
open — but the idempotent-`CREATERESPONSE` fix stays **deferred** until a soak actually
shows the stall.

**The session still froze (partial screen) — but that's the TCP path, not a lossy bug.**
`soak-lossy.sh` deliberately does **not** set `MACRDP_UDP_MIGRATE_EGFX`, and the offer is
`UdpFecL`-only, so **nothing rides the lossy tunnel**: EGFX + audio + input all share the
one CredSSP TCP stream, which HOL-blocks a 1080p H.264 keyframe under loss exactly as
finding #3 documents (same structural limit, on TCP). The lossy tunnel established cleanly
but carries no payload yet.

**Takeaway:** the P2.2 lossy *transport* is validated under loss (handshake robust), but
this transport soak structurally **cannot demonstrate a lossy *win* until a real payload
rides the tunnel**. That payload — **lossy audio (P2.4b 2b-iv)** — is now built and
verified on a clean link (audio renders over the lossy DTLS tunnel; see "P2.4b 2b-iv
result" below), so the *win* can finally be soaked: see the **lossy-audio soak (runbook)**
next. (Video-over-lossy stays out of scope — H.264-under-loss needs intra-refresh, P2.6.)

### Lossy-audio soak (runbook)

This is the soak that can show the lossy **win** the P2.2 transport soak couldn't (it had
no payload on the tunnel). It rides AAC audio on the lossy `UdpFecL`/DTLS tunnel and
contrasts it against the same audio on TCP under identical shaping. Harness:
`scripts/soak-lossy-audio.sh` (the audio sibling of `soak-lossy.sh`; it sets
`MACRDP_UDP_OFFER_FECL=1` + `MACRDP_UDP_LOSSY_DELIVERY=1` + `MACRDP_UDP_LOSSY_AUDIO=1`,
requires `--enable-aac`, and live-prints the audio-DVC + tunnel markers).

**The A/B (this is the whole point — listen, don't just read logs):** play steady audio on
the Mac (music / a YouTube tab) and listen on the client while shaping the link.
- **CONTROL** — `MACRDP_UDP_LOSSY_AUDIO=0 scripts/soak-lossy-audio.sh …` → audio on the
  static RDPSND **TCP** channel. Expect audible gaps / desync that worsen as `--loss`
  climbs (the one TCP stream HOL-blocks audio behind every other channel).
- **TREATMENT** — default (`=1`) → audio on the lossy UDP/DTLS tunnel. Expect it to stay
  smooth where the control degraded: a lost wave is simply dropped (correct + free on a
  live stream), no HOL stall, no growing backlog.

```
# 1) shape both directions of TCP+UDP on the port:
sudo scripts/netshape.sh on --loss 5 --delay 100

# 2a) TREATMENT — audio over the lossy tunnel (default):
scripts/soak-lossy-audio.sh --enable-udp-multitransport --enable-aac --enable-h264 \
    --username "$USER" --password 'PASS' --bind 0.0.0.0:3390

# 2b) CONTROL — same everything, audio on TCP:
MACRDP_UDP_LOSSY_AUDIO=0 scripts/soak-lossy-audio.sh --enable-udp-multitransport \
    --enable-aac --enable-h264 --username "$USER" --password 'PASS' --bind 0.0.0.0:3390

# …connect mstsc, play audio, listen, then restore:
sudo scripts/netshape.sh off
```

**Read the markers** (the script highlights them; full log in the file). In treatment they
fire roughly in this order, and all of them appearing = audio is on the tunnel:
- `offering AUDIO_PLAYBACK_LOSSY_DVC` (gate on) → `reliable audio DVC negotiated` (AAC
  handshake done over TCP) → `lossy audio DVC opened` → `DYNVC_SOFT_SYNC_REQUEST` +
  `Soft-Sync Response` (mstsc accepts UDPFECL) → `P2.1 GREEN` / `P2.4 GREEN` (DTLS + tunnel
  up under loss) → **`streaming Wave2 audio over the LOSSY UDP/DTLS tunnel`** (the payoff;
  fires once, static rdpsnd then silent).
- Failure tells: `lossy audio wave route over UDP tunnel failed`, `cookie not recognized`,
  `Broken pipe` / `Connection reset`. `RTO retransmit` should be ~0 for the genuine lossy
  flow (any value = the reliable SM is still carrying it; check `MACRDP_UDP_LOSSY_DELIVERY`).

**Sweep** loss `0 → 2 → 5 → 10 %` × latency `+60 / +150 ms`, in both modes. Record the cell
where the control becomes unacceptable and confirm the treatment is still smooth there —
that gap is the user-noticeable result that justifies the phase. Also watch (per the P2.2
caveat) whether, at higher loss, the lossy handshake stalls before `P2.4 GREEN` (one-shot
`CREATERESPONSE`/DTLS flight lost); if it does, the idempotent-`CREATERESPONSE` /
DTLS-timeout-driven-retransmit fix becomes warranted (still handshake-only — data stays
send-once). A useful tie-breaker if the lossy handshake is the limiter: run the treatment
with `MACRDP_UDP_LOSSY_DELIVERY=0` (audio still on the tunnel, but the flow rides the
**reliable** SM that retransmits) — if audio is smooth there but the genuine lossy mode
stalls to establish, the gap is purely handshake retransmission, not the steady-state path.

### Lossy-audio soak result (2026-06-27, mstsc, 5% loss + 100 ms/dir) — needs FEC

First real soak of audio-over-tunnel. **Clean link: audio plays fine** (re-confirms 2b-iv-B /
PR #61 through the harness). **At 5% loss both delivery modes fail the same way — choppy then
silence** — which is exactly the design thesis, now demonstrated:

| Link | Delivery | Result |
|---|---|---|
| clean | send-once (lossy) | **plays fine** |
| 5% loss | send-once (lossy) | choppy → silence |
| 5% loss | reliable (`MACRDP_UDP_LOSSY_DELIVERY=0`) | choppy → silence |

The server side was healthy in every run (lossy DVC negotiated, Wave2 streaming over the
tunnel, client acking, graceful disconnect — no route-fail). The two failure mechanisms:
- **Send-once:** ~5% of AAC access units are dropped on the wire; mstsc's AAC decoder stalls on
  the gaps. (Each lost AU ≈ 23 ms hole; ~1 in 20.)
- **Reliable:** the tunnel send (`route_dvc_over_udp` → `TunnelSender`) is **non-blocking**, so
  the drop-stale audio-lag model (vendor divergences (2)/(3)) is **blind to the tunnel's
  backlog** — on TCP, `write_all` blocking is what makes `audio_shipped_ms` reflect real
  progress; on the tunnel it races ahead and the model never drops. Audio is produced at
  realtime, the latency-bound reliable tunnel can't drain it, the backlog grows, and audio
  arrives later and later until it's effectively silence. (Reliable-tunnel audio HOL-blocks like
  TCP — the very thing "Audio belongs on the lossy transport" predicts below; this is the live
  proof.) The reliable run logged **0 RTO retransmits** over its short window — it didn't fail by
  losing packets, it failed by backing up.

**Conclusion: the lossy-audio win requires FEC (P2.3).** FEC recovers the ~5% AU loss without
retransmit or HOL, so the AAC stream has neither gaps (the send-once killer) nor backlog (the
reliable killer). Raw send-once + DTLS is necessary but not sufficient at 5%. **Open follow-ups
before building full FEC:** (a) a loss-threshold sweep (`--loss 1/2/3`) to find where raw
send-once is already acceptable — typical WiFi loss is ≪5%, so lossy audio may already deliver a
real-world win for mild loss with FEC only needed for harsh links; (b) a cheap stepping-stone
worth trying before a full Reed-Solomon/XOR FEC — **duplicate-AU redundancy** (send each AAC AU
twice over the lossy tunnel): at 5% independent loss the chance both copies drop is ~0.25%, a 20×
cut in effective AU loss, for ~2× of a ~130 kbps stream (negligible). (c) Separately, the
**lag-model blindness to the tunnel backlog** is a real latent issue for any future
reliable-tunnel payload, though moot for send-once (which never backlogs).

### Lossy-audio soak RESULT (2026-06-29, mstsc) — 1+1 redundancy WORKS; SHIPPED as `--enable-lossy-audio`

Follow-up (b) above — duplicate-AU redundancy — was built (`MACRDP_UDP_LOSSY_AUDIO_DUP`:
each lossy datagram sent twice; the client's DTLS anti-replay window dedups, so audio never
double-plays) and **soaked under real loss on mstsc. It holds where send-once fails:**

| Link | Delivery | Result |
|---|---|---|
| 5% loss | send-once (dup=0) | glitchy (baseline — confirms loss hits the lossy path) |
| 5% loss | **1+1 (dup=1)** | **smooth** |
| 10% loss | **1+1 (dup=1)** | **smooth** |
| 15% loss | **1+1 (dup=1)** | **smooth** (trace jitter only) |

No teardown, no RTO retransmits (1+1 *is* the recovery — there's nothing to retransmit), and
audio was genuinely on the lossy UDP tunnel (TCP fallback ruled out: `streaming Wave2 … LOSSY
UDP/DTLS tunnel`, one continuous session, `dup=0` glitched on the same path). So the earlier
"needs FEC" conclusion is **superseded** — the p→p² math (5%→0.25%) bears out in practice, and
FEC proper is a structural NO-GO anyway (see the P2.3 FEC RESULT). **The back-to-back duplicate
was enough — the documented time-staggered-duplicate hardening (Contingency A, for bursts
defeating both copies) was NOT needed** and stays deferred unless a future burst-loss case
defeats 1+1.

**REAL-LINK PERCEPTUAL VALIDATION (2026-06-29, live mstsc/WiFi6, clumsy on the port):**
beyond the loss-rate soak above, a live drive across the loss range confirmed the A/V-sync
thesis perceptually with **audio on lossy UDP + video on TCP** (the robust config —
`ENABLE_LOSSY_AUDIO=1` + `UDP_MIGRATE_EGFX=0`):
- **15% UDP-only drop:** audio stayed smooth (1+1 covered it); video untouched (loss wasn't
  on its transport).
- **15% TCP (video) drop:** video went **jittery→smooth** — the TCP-path adaptive controller
  backed the bitrate off the 6 Mbps ceiling (P3/EWMA); video got blocky-but-moving, no freeze.
- **5% on both:** graceful — occasional skips, video sometimes blocky, **but A/V stayed in
  sync**, with video gradually skipping to re-align to audio. Crucially the user observed
  **"audio jumps, but it's really in sync"** — i.e. audio skips *forward* to the current
  position (drop-stale, never-replay-late) rather than drifting behind as it does on pure TCP.
  This is the whole point: audio is the real-time anchor, video adapts to it.
The residual skips at 5% are the known unbuilt pieces (encoder frame-drop/fps at the floor =
"P2b"; tighter audio-lag resync = "lever B"); sync itself holds. Note: a UDP-only clumsy
filter stresses ONLY audio (video on TCP sees no loss) — use a TCP-inclusive filter to also
exercise the video adaptive path.

**SHIPPED as the single `--enable-lossy-audio` flag (PR #101, 2026-06-29).** It implies the
UDP listener and bridges the four expert env gates
(`MACRDP_UDP_{OFFER_FECL,LOSSY_DELIVERY,LOSSY_AUDIO,LOSSY_AUDIO_DUP}`, which still work
standalone); requires `--enable-aac` (MS-RDPEA) + `--enable-h264` (the lossy-audio Soft-Sync
rides the EGFX dispatch path) and warns if either is missing. This is the first user-noticeable
Phase-2 *win* — loss-resilient audio. Lossy *video* over the tunnel remains the open ceiling
(reliable-tunnel HOL-block; separate, lower value).

## Audio belongs on the lossy transport, not the reliable one

A natural-looking "next step" is to also route **audio (RDPSND)** over the UDP
tunnel. **On the reliable tunnel we have today, don't — it's a downgrade.** The
same ordered-stream HOL blocking that limits video applies to audio, and it's
*worse* for audio because of how macrdp already handles audio loss:

- **Clean link:** no benefit — audio-over-reliable-UDP ≈ audio-over-TCP.
- **Lossy link:** a lost audio packet stalls everything behind it until retransmit.
  But on a live stream **late audio is worthless**, so the vendored server
  deliberately **drops stale waves + resyncs on stall** (the audio-lag divergences
  (2)/(3)/(8)) rather than replay them late. A reliable *ordered* tunnel can't drop
  anything — it must deliver in order — so moving audio there **defeats the existing
  drop-based lag model** and trades graceful degradation (audible gaps) for hard HOL
  stalls + backlog. Strictly worse than the TCP path it would replace.

**But audio over a *lossy* tunnel (Phase 2) is arguably the BEST use of UDP
multitransport — better than video** — and the soak is exactly why. Audio is:

1. **Drop-tolerant** — late packets are discarded anyway, so a lossy transport (no
   retransmit, no HOL block) matches audio's nature exactly, sidestepping the very
   thing that doomed reliable-UDP video.
2. **Low-bandwidth** (~128 kbit/s AAC, ~1.4 Mbit/s PCM) — so **FEC is cheap** to add
   (a few % overhead protects it against loss), where FEC-protecting H.264 is
   expensive.
3. **Latency-critical** — UDP's no-HOL-blocking helps audio directly, and the TCP
   failure mode under loss (backlog → progressive desync; see the audio quirks) is
   precisely what a lossy transport avoids.

So the priority order for Phase 2 flips the intuition: **once the lossy `UdpFecL`
transport + FEC exists, audio is plausibly the first channel to migrate, ahead of
video** — reliable-video-under-loss is a dead end, but lossy-*audio* is where UDP
multitransport could deliver a real, user-noticeable win on a bad link. (Audio's
drop-tolerance is the property video lacks.) Until Phase 2, audio stays on TCP with
its existing lag model — the right call.

## Phase 2 — scope (staged): lossy `UdpFecL` + DTLS + FEC + lossy audio

*Scoped 2026-06-26 after the Phase-1 soak. This is a **plan**, not built. Read the
"Audio belongs on the lossy transport" section above first — it sets the priority order
(audio before video). Four protocol/library unknowns were researched up front; the
findings change the shape materially and are folded in below.*

### The strategic caveat — read before committing any code

**The lossy transport is a *legacy* codepath, and it is not yet proven that modern
mstsc will even use it.** Two facts:

1. **RDPEUDP2 dropped lossy transport entirely.** The v2 framing (`RDPUDP2_*`) is
   reliable-only / TLS-only. Lossy (`UdpFecL`) + DTLS + FEC live only in the older
   **RDPEUDP v1** codepath. Modern mstsc negotiates **RDPEUDP2-reliable** for its UDP
   flow — which is the path Phase 1 already ships and verified.
2. **In Phase 1 we only ever *offered* `UdpFecR` (reliable).** We have never offered
   `UdpFecL`, so we have **zero evidence** about what current mstsc does when a lossy
   transport is on the table. It may open a lossy DTLS flow; it may decline and stay on
   the reliable RDPEUDP2 flow it already prefers; it may ignore the offer entirely.

So the entire Phase 2 lossy investment (a DTLS dependency + a lossy state machine + a
FEC encoder + a new audio DVC) buys nothing if the client we care about won't open a
lossy flow. **That question is cheap to answer and must be answered first.**

### P2.0 — Go/No-Go spike (do this FIRST; ~1 day; gates everything else)

Offer `UdpFecL` and watch a real mstsc (Win11/WiFi, the Phase-1 rig). Concretely:

- Acceptor: advertise `TRANSPORT_TYPE_UDP_FECL` in the SC multitransport block and emit
  a **second** Server Initiate Multitransport Request with
  `RequestedProtocol::UdpFecL` (in addition to, or instead of, the existing `UdpFecR`
  offer — try both orderings). All the offer/emit machinery already exists from M3c
  (acceptor divergence (3)); this is a flag + a second request, not new infrastructure.
- Listener: log, on the lossy flow, whether mstsc (a) opens a second UDP flow at all,
  (b) sends a **DTLS ClientHello** (record type 22, version `0xFEFF` = DTLS 1.0) rather
  than a TLS ClientHello, and (c) which RDPEUDP version it negotiates on it.
- **No DTLS implementation needed for the spike** — we only need to *observe* the
  ClientHello arriving (or not). A Wireshark capture is the definitive artifact, same
  method that cracked Phase 1.

**Decision gate:**
- **mstsc opens a lossy DTLS flow → GREEN.** Proceed to P2.1. We now know the payoff is
  reachable by the primary client.
- **mstsc declines / stays reliable → RED.** Park Phase 2. Document that lossy UDP is
  unreachable by modern mstsc; the only consumers would be older clients / a
  future UDP-capable FreeRDP (which today has *no* UDP data path on either side — see
  the Landscape section). Revisit only if a concrete client demands it.

This one spike converts "should we build a multi-week lossy stack?" into an empirical
yes/no for the cost of a flag and a capture. **Do not skip it.**

#### Result (2026-06-26): **GREEN** — verified live on real mstsc (Win11/WiFi)

Ran with `MACRDP_UDP_OFFER_FECL=1` (the spike toggle in `src/multitransport.rs`).
The log answered every sub-question, and decisively in favor of building Phase 2:

- **mstsc advertises lossy and *prefers* UDP.** Its CS multitransport block was
  `TRANSPORT_TYPE_UDP_FECR | TRANSPORT_TYPE_UDP_FECL | TRANSPORT_TYPE_UDP_PREFERRED |
  SOFT_SYNC_TCP_TO_UDP` — so the "RDPEUDP2 dropped lossy, mstsc won't use it" worry was
  unfounded for *this* mstsc build: it not only supports `UDP_FECL`, it sets
  `UDP_PREFERRED`.
- **It opened a lossy flow.** Once we emitted the `UdpFecL` Initiate Request, mstsc sent
  a SYN with the **`SYN_LOSSY`** flag set (`FecFlags(SYN | SYN_LOSSY | CORRELATION_ID |
  SYNEX)`), RDPEUDP version **V2** — the same version family as the reliable path, just
  in lossy mode. So the lossy flow is *not* a different/older RDPEUDP; it's V2 with the
  lossy bit.
- **It started a DTLS handshake — DTLS 1.2, not 1.0.** The listener observed a
  **DTLS 1.2** ClientHello (`0x16 0xFE 0xFD`), correcting the up-front research's
  "DTLS 1.0" assumption. **This simplifies P2.1:** `boring`'s safe DTLS API covers 1.2
  natively, so the one reason to prefer `openssl` (explicit DTLS-1.0 version pinning) is
  moot — **`boring` is the clear choice.** (Implement 1.2; allow 1.0 fallback only if a
  later client demands it.)
- **Graceful throughout.** We implement no DTLS, so the handshake never completed; mstsc
  simply retried its ClientHello periodically while the **session ran normally over TCP**
  the entire time. Confirms the lossy offer is safe to ship behind a flag — a
  non-responsive lossy flow costs nothing.

**Verdict: proceed to P2.1 when ready.** The lossy payoff is reachable by the primary
client. The spike code stays in-tree, env-gated and default-off (zero effect on the
proven Phase-1 reliable path), as the harness for the P2.1+ DTLS work.

### If GREEN — the staged build (mirrors Phase 1's M1→M5 discipline)

Each milestone is its own gated PR, real-client-verified, feature-flagged
(`multitransport` cargo feature already exists; reuse it), zero-cost when off.

- **P2.1 — DTLS layer (the hard new dependency).** Add `boring` — the P2.0 capture
  showed mstsc offers **DTLS 1.2**, which `boring`'s safe API does natively
  (`SslMethod::dtls()` + `Ssl::setup_accept()` over a memory BIO; no DTLS 1.3 via the
  safe API, which is fine). The one reason to have considered `openssl` (explicit
  DTLS-1.0 version pinning) is moot now that the client is 1.2. Quarantine **all** of it behind one
  maintenance-boundary file `vendor/ironrdp-server/src/multitransport/dtls.rs`, the same
  way Phase 1's rustls layer is isolated. **Wire layering (researched):** the DTLS record
  sits *inside* the cleartext RDPUDP framing, not around it —
  `UDP datagram → RDPUDP header (cleartext seq/ACK/FEC) → DTLS record → RDP_TUNNEL_* (EMT)
  → DVC data`. DTLS encrypts only the RDPUDP *payload*; the reliability header stays in
  the clear so the state machine can ACK/retransmit without decrypting. Reuse the same
  self-signed cert as TCP/the reliable flow. **Risk:** a second C-crypto stack in the
  signed/notarized macOS binary — verify the build + notarization still pass (we already
  link aws-lc-rs/ring, so the toolchain exists, but `boring` is a distinct BoringSSL
  vendored build). Spike the mstsc DTLS handshake interop against a capture.

  **Result (P2.1a, 2026-06-26): GREEN — verified live on real mstsc.** The DTLS
  1.2 server handshake **completes** over the lossy (`UdpFecL`) RDPEUDP flow,
  sans-I/O via `boring`'s custom-BIO `SslStream` (`multitransport/dtls.rs`),
  driven datagram-by-datagram from the listener. Timeline from the log: client
  DTLS 1.2 ClientHello → server flight (ServerHello/Certificate/Done) → client
  ClientKeyExchange+ChangeCipherSpec+Finished → **handshake complete in ~13 ms /
  ~2 RTT**, no errors. Build facts confirmed: `boring`/BoringSSL builds with the
  local Go+cmake+libclang toolchain and coexists with rustls's aws-lc-rs; **no
  DTLS cookie exchange needed** (BoringSSL dropped `DTLSv1_listen`); `set_mtu(1100)`
  + `SslOptions::NO_DTLSV1` pin 1.2 (boring has no DTLS `SslVersion` constants).
  Two follow-ups observed: (1) post-handshake the client retransmits an encrypted
  MS-RDPEMT `CREATEREQUEST` (~5/s) we don't answer yet — that's **P2.4**
  (EMT-over-DTLS tunnel); (2) the lossy flow is still driven through the
  *reliable* RDPEUDP SM (fine on a clean link; proper lossy delivery is P2.2).
  Env-gated (`MACRDP_UDP_OFFER_FECL=1`) + default-off; the reliable path is
  byte-unchanged.

- **P2.2 — lossy RDPEUDP state machine.** Extend `vendor/ironrdp-rdpeudp` with a lossy
  mode alongside the existing reliable one: source packets sent **without retransmit**,
  loss-tolerant delivery (deliver-on-arrival, no in-order HOL block — *this is the whole
  point*), and the lossy ACK semantics. Most PDU codecs already exist from Phase 1; this
  is a second delivery policy in `state.rs`, not a new crate. Sans-I/O, unit-tested under
  injected loss/reorder/dup like the reliable machine.

  **Step 1 done (2026-06-27): the lossy delivery policy, SM-only.** `Config.mode`
  (`DeliveryMode { Reliable, Lossy }`, default `Reliable` → existing callers
  byte-identical). Lossy delivers source payloads on arrival (no reorder buffer, no
  HOL) and sends our data once (no RTO retransmit; the `unacked` push is skipped).
  Inbound is not de-duplicated in lossy mode (DTLS/audio self-dedup). Unit-tested
  (deliver-on-arrival/no-HOL with a reliable control + send-once/no-retransmit).

  **Step 2 done (2026-06-27, verified on real mstsc): the lossy flow uses lossy
  delivery.** Behind the experimental env `MACRDP_UDP_LOSSY_DELIVERY` (default off →
  the lossy flow keeps riding the reliable SM, a clean one-var A/B), the listener
  classifies each flow at its opening SYN (`SYN_LOSSY` ⇒ `UdpFecL`) and builds that
  peer's SM with `DeliveryMode::Lossy`; the reliable (`UdpFecR`) flow is never touched.
  **Verified live:** with the env set the lossy peer logs `RDPEUDP peer using LOSSY
  delivery`, and the **DTLS 1.2 handshake + MS-RDPEMT tunnel both reach established over
  send-once/no-retransmit delivery** (`P2.1 GREEN` → `P2.4 GREEN`), H.264 video + audio
  stable. DTLS's own record-layer reordering plus a clean link (everything arrives once)
  is what carries the handshake without transport retransmission. **Known under-loss
  caveats (soak-phase, not hit on a clean link):** the one-shot `CREATERESPONSE(S_OK)`
  and the DTLS server flight aren't resent by the SM, and the tunnel's `tunnel_created`
  guard answers a repeated `CREATEREQUEST` only once — so under real loss the handshake
  may need the response re-sent idempotently (or a handshake-phase retransmit). That's
  the next thing the netshape soak should probe.

- **P2.3 — FEC encoder — CAPTURE-BLOCKED (spec research 2026-06-27, [MS-RDPEUDP] rev 19.0).**
  A deeper read of the spec turned the lossy-audio soak's "needs FEC" conclusion into four
  hard blockers, so before any encoder we do a **capture-first go/no-go spike** (see the
  "P2.3 FEC capture runbook" below):
  1. **Receiver decode is OPTIONAL.** The spec is explicit — "the receiver … can ignore the
     FEC Packet and not use it for any decoding operations"; **retransmission (ARQ) is the
     actual reliability mechanism**, FEC is only an opportunistic latency-saver. So a perfect
     encoder may be *ignored* by mstsc. (§ "UDP Data Transfer".)
  2. **The GF(256) coefficient table is UNDOCUMENTED.** It's a Reed–Solomon / Vandermonde
     code over GF(2^8), NOT XOR parity (spec example coefficients `[1 142 244 71 167]`), but
     the generator polynomial, primitive element, and `uFecIndex`→coefficient-row mapping are
     **not published**. Emitting packets a Windows receiver would decode requires
     reverse-engineering the table **from a packet capture of a real Windows RDP *server*
     sending FEC**. (§2.2.2.2/2.2.2.3.)
  3. **FEC is v1/v2-only — RDPEUDP2 (v3+) has no FEC.** [MS-RDPEUDP2] (Overview) states it
     verbatim: RDPUDP2 "only supports 'Reliable' UDP mode ... does not support 'Best-Efforts'
     mode or RDP-UDP-L, [so] it does not include a forward error correction (FEC) mechanism" —
     loss is recovered by retransmission. mstsc prefers v2-carrying-TLS / RDPEUDP2, so even on a
     lossy flow mstsc may negotiate a version where FEC never applies.
  4. **No OSS reference** — FreeRDP's prototype explicitly skipped FEC, never merged.

  Wire facts (confirmed, for when/if we build it): an FEC packet is an ACK datagram with
  `uFlags = FEC|DATA|ACK (0x1C)`, layout `RDPUDP_FEC_HEADER(8) + AckVector + RDPUDP_FEC_PAYLOAD_HEADER(12) + parity`.
  `RDPUDP_FEC_PAYLOAD_HEADER` = `snCoded u32`, `snSourceStart u32`, `uRange u8` (covers
  `[snSourceStart .. snSourceStart+uRange]`, ≤255), `uFecIndex u8` (selects the parity/coefficient
  row — multiple FEC packets w/ distinct `uFecIndex` recover >1 loss; one packet = one loss),
  `uPadding u16` (=0). Each source packet enters the FEC math prefixed with a 2-byte
  `RDPUDP_PAYLOAD_PREFIX` (`cbPayloadSize`) and zero-padded to the longest member of the range.
  `snCoded` advances for **every** datagram (source AND FEC); `snSourceStart` only for source.

  **Decision (user, 2026-06-27): capture-first.** Build NO encoder until a capture proves
  (a) a real Windows server actually sends FEC to a client on a lossy flow under loss, and
  (b) yields the coefficient table. If the capture shows FEC isn't used in practice (likely,
  given #1/#3), lossy-audio-over-mstsc has no clean FEC win and the conclusion is documented
  as such — the retransmission-ARQ path (fix server→client RTO + the tunnel-backlog-blind lag
  model) becomes the alternative.

### P2.3 FEC capture runbook (the go/no-go spike)

Goal: a `.pcap` of a **real Windows RDP server** sending RDPEUDP **FEC packets** to a client
over a **lossy** link, to answer (a) *does it happen at all?* and (b) *what are the GF(256)
coefficients?* Only build the encoder if (a) is yes.

**Topology (use what's on hand — one Windows box + the Mac):**
- **Server:** enable Remote Desktop on the Windows box (System → Remote Desktop). Ensure UDP
  multitransport is on (default; GPO "Select RDP transport protocols" = Use both / leave default).
- **Client + capture host:** the Mac, via **Microsoft Remote Desktop.app** (it can open a UDP
  flow). Capture on the Mac with Wireshark/tcpdump on the RDP UDP flow.
- **Loss:** `sudo scripts/netshape.sh on --loss 8 --delay 80 --port 3389` on the Mac (shape the
  *server's* RDP port so the path is lossy → the server's RDPEUDP reacts; 8% to make FEC, if
  used, frequent). Higher loss than the audio soak on purpose — we want to *provoke* FEC.
- If the Mac client never triggers FEC, fall back to a **second Windows box running mstsc** to
  the same server (Windows↔Windows is the most likely to negotiate a FEC-capable version), with
  a lossy router/`clumsy` in between, captured on either end.

**Capture + first look:**
```
# on the Mac, capture the UDP flow (RDP UDP rides the same 3389 by default):
sudo tcpdump -i any -w /tmp/rdpfec.pcap 'udp and port 3389'
# …connect MS Remote Desktop to the Windows server, exercise it ~30s under loss, stop tcpdump.
scripts/fec-scan.sh /tmp/rdpfec.pcap     # summarizes RDPUDP flags; flags FEC datagrams
```
`scripts/fec-scan.sh` (added with this) tallies the RDPUDP `uFlags` per datagram and prints any
with the **FEC bit (0x10)** set, with `snCoded`/`snSourceStart`/`uRange`/`uFecIndex` and the
parity bytes. **Go/no-go:** any FEC datagrams at all → (a) is YES, capture-spike green, proceed
to coefficient extraction (paste the pcap and I'll work the GF(256) table from the covered
source packets + parity). **Zero FEC datagrams over a lossy link** (only retransmits) → (a) is
NO: a real Windows server doesn't use FEC here either, so we don't build the encoder, and the
ARQ-fix path is the only lever. Either outcome is a decisive result.

### P2.3 FEC capture RESULT (2026-06-27) — NO-GO (structural): modern Windows uses RDPUDP2, no FEC

Captured a **real Windows RDP server** (a VirtualBox Windows VM; host `127.0.0.1:3390` NAT-forwarded to
the guest's `3389`) ↔ **mstsc on the host**, on the host loopback. Analyzed with the rewritten
`scripts/fec-scan.sh` driving Wireshark's RDPUDP **dissector** (authoritative — version-aware framing):

- **Negotiated version = `0x0101` (RDPUDP2 / Wireshark "UDPv2")** — read from the SYNEX of both the client
  SYN and the server SYN+ACK.
- **`syn-lossy flows = 0`**, **`FEC packets = 0`** of 1912 RDPUDP datagrams (data=1747, ack=634).

**Structural NO-GO, and the no-loss capture is sufficient to prove it.** FEC exists only in RDPEUDP
**v1/v2** (the FEC structures — `RDPUDP_FLAG_FEC 0x0010`, `RDPUDP_FEC_PAYLOAD_HEADER` — live in
[MS-RDPEUDP] §2.2.1/§2.2.2.2). **RDPUDP2 (`0x0101`) has no FEC at all**, and this is now *normatively
confirmed*, not merely capture-inferred — the [MS-RDPEUDP2] Overview states it verbatim:

> "the RDP-UDP2 transport only supports 'Reliable' UDP mode. In this mode, the endpoint retransmits
> datagrams that have been lost... Because RDP-UDP2 transport does not support 'Best-Efforts' mode or
> RDP-UDP-L, it does not include a forward error correction (FEC) mechanism."

So RDPUDP2 is reliable-only: no lossy (RDP-UDP-L) mode at all, loss recovered purely by **retransmission**
(ARQ), no FEC. Version negotiation is capability-based at SYN time — *before* any data/loss — so a lossy
link would **not** downgrade to a FEC-capable version; it'd still be RDPUDP2, still zero FEC. **So mstsc
would never decode FEC we emit — building the encoder is pointless. P2.3 is closed NO-GO**, confirming the
spec research (blocker #3) on real Windows wire *and* in the RDPUDP2 spec text itself.

**Industry status (2026-06-28 web survey) — FEC is a dead/legacy feature across the whole RDP ecosystem,
not just a macrdp gap.** The only RDP stack that *ever* shipped FEC is **Microsoft's own server in the
RDP 8.x / RemoteFX-over-UDP era** (RDPEUDP **v1** lossy, `UDP-FEC-L`, Reed–Solomon). Microsoft then
**removed FEC entirely in RDPEUDP2** ([MS-RDPEUDP2] quoted above) and modern Windows (RDP 10+, incl. AVD
RDP Shortpath) negotiates RDPUDP2 → **retransmit-only, never FEC** (matches our zero-FEC capture).
**FreeRDP** deliberately implemented **only RDPUDP2**, explicitly to avoid FEC (its UDP author: *"annoyed
by the FEC in RDPEUDP"*) — so it has no FEC either, on top of having no working server UDP path at all.
xrdp/ogon/gnome-remote-desktop/Weston are TCP-only. **Net:** both Microsoft (by removing it) and FreeRDP
(by skipping it) voted for reliable retransmit over FEC, so there is **no current client that would accept
macrdp-emitted FEC** — reinforcing the NO-GO and validating the **1+1 lossy-audio redundancy** stand-in
(P2.3 below) as the only loss-resilience lever reachable for a modern client. Sources: [MS-RDPEUDP2
Transition](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpeudp2/c1ff35b9-fdb4-474b-ba32-a91ebf047561),
[UDP support in FreeRDP pt.2](https://www.hardening-consulting.com/en/posts/20230109-udp-support-2.html),
[RemoteFX deprecation (Wikipedia)](https://en.wikipedia.org/wiki/RemoteFX).

(Aside: RDPUDP2 being reliable-only does **not** affect macrdp's lossy-audio / 1+1 work — that rides
RDPEUDP **v2 with RDP-UDP-L**, which mstsc opens because macrdp offers `UdpFecL` and negotiates v1/v2, not
RDPUDP2. RDPUDP2's reliable-only constraint just means modern *Windows-to-Windows* never opens that lossy
flow; mstsc still opens one when the **server** offers it.)

Tooling note: the first `fec-scan.sh` hand-parsed uFlags at a fixed offset and misread RDPUDP2 data
packets as "495 FEC datagrams" (a false GO). Rewritten to use the dissector's `rdpudp.flags.fec` +
`rdpudp.synex.version`; it now reports the negotiated version and emits the structural NO-GO for `0x0101`.

**Remaining lossy-audio levers (neither depends on RDPEUDP FEC):** (1) **1+1 transport-level
redundancy** — built, see P2.3 below; else (2) accept that lossy-audio-over-mstsc has no clean win
(reliable HOL-blocks; send-once loses AUs) and document it. The ARQ path does **not** rescue *audio*
(reliable = HOL-block = the very problem), so "fix ARQ" is not a lossy-audio fix — it's only relevant if a
*reliable* tunnel payload is ever wanted.

### FEC spec survey (2026-06-27) — every Microsoft media-FEC spec, and why none is reachable on the RDP path

After the RDPUDP2 finding we swept the Microsoft Open Specs (+ the RFC base) for *any* FEC / parity /
redundant-packet / loss-recovery mechanism an RDP client would decode on the **audio** or **H.264** path —
not just the transport. The specs exist; they sort into two buckets, and **neither is usable by macrdp.**

**Bucket 1 — RDP transport FEC (the one scheme already known):**
- [MS-RDPEUDP] **v1** — `RDPUDP_FLAG_FEC` + `RDPUDP_FEC_PAYLOAD_HEADER`, GF(256), **coefficients
  undocumented**. The only FEC any RDP client decodes.
- [MS-RDPEUDP2] — **no FEC** (reliable-only, retransmit), spec-stated.
- [MS-RDPEMT] — tunnel only selects reliable/lossy + TLS/DTLS; defines no FEC.
- [MS-RDPBCGR] `UDPFECR`/`UDPFECL` — just *names* for the two RDPEUDP transport modes; the "FEC" is the
  transport-family label, **not** a second scheme.

**Bucket 2 — Microsoft media-payload FEC (all RTP, all off the RDP path):**
- [MS-H264PF] (H.264 FEC, RFC 6190/5109 lineage) — RTP, UCC (Lync/Skype/Teams).
- [MS-RTVPF] (RTVideo FEC, XOR) — RTP, UCC.
- [MS-RTP] (drives audio FEC via RTCP "FEC distance"/healer signaling) — RTP, UCC.
- [MS-RTSP] (`audio|video|application/x-wms-fec`, ≤24-source-packet XOR FEC) — RTP under Windows Media.
- [MS-RTPRAD]/[MS-RTPRADEX] (RFC 2198 audio redundancy) — RTP, UCC.
- RFC 5109 / RFC 2733 — the IETF base (generic XOR/parity); an **RTP payload format by construction**.

**The decisive fact ruling out all of Bucket 2: RDP never carries RTP.** Every FEC-bearing media spec is in
`office_protocols` (the UCC/RTP or Windows Media stack); **no RDP spec references RTP, RFC 5109, or any of
these payload formats.** macrdp's media rides RDP virtual channels — H.264 over [MS-RDPEGFX] (explicitly a
*"non-lossy dynamic virtual channel"*; its only loss handling is `RDPGFX_FRAME_ACKNOWLEDGE_PDU` flow
control + IDR/retransmit) and audio over [MS-RDPEA]/RDPSND (lossy mode is just the `AUDIO_PLAYBACK_LOSSY_DVC`
channel-name selector, zero payload protection). So mstsc-as-an-RDP-client has **no decoder** for any of
these on those channels. [MS-RDPEVOR], [MS-RDPRFX], [MS-RDPNSC], [MS-RDPEGDI] likewise carry no codec-level
FEC (all reliable-channel specs).

**Conclusion:** the only client-decodable FEC on the RDP path is RDPEUDP v1's GF(256) transport FEC
(undocumented coefficients; legacy v1/v2 only; superseded by RDPUDP2's no-FEC). There is **no** audio-payload
FEC, **no** H.264/EGFX-payload FEC, and **no** RTP-based FEC reachable from RDP. This is exactly why the
**1+1 transport-level redundancy** below — which needs no client FEC decoder (DTLS dedups the duplicate) — is
the only loss-recovery lever that works for *any* current RDP client. Don't re-survey these specs; the
result is comprehensive.

### P2.3 FEC — future revisit (needs legacy-Windows machines)

The NO-GO above is structural **for modern Windows** — it's a property of the *negotiated transport
version*, not of the link. So the one thing that could reopen FEC is a session that negotiates **RDPEUDP
v1/v2** (`0x0001`/`0x0002`) instead of **RDPUDP2** (`0x0101`), because FEC's `RDPUDP_FEC_PAYLOAD_HEADER`
only exists in v1/v2. The version is chosen at SYN time by capability, and modern Win10/11 both prefer
RDPUDP2, which is why the VM capture never had a chance. **What changes the answer is the OS, not the
hardware:** a Windows old enough to predate RDPUDP2 will fall back to RDPEUDP v1/v2.

**Machines to acquire for the revisit** (this is the "appropriate machines" gap):
- A **legacy Windows endpoint that negotiates RDPEUDP v1/v2** — roughly the RemoteFX-over-UDP era:
  **Windows 8 / 8.1 mstsc** or **Server 2012 / 2012 R2** (as the RDP *server*). Either side being legacy
  may be enough to drop the negotiated version below RDPUDP2; both legacy is safest. (Exact
  version→OS mapping is unconfirmed — establish it empirically with `fec-scan.sh`, which prints the
  negotiated `rdpudp.synex.version`.)
- A way to **open a lossy (`SYN_LOSSY`) flow** at all — that needs UDP multitransport enabled and a
  real-time graphics channel (RemoteFX / H.264) carried over it; confirm `syn-lossy flows > 0` in the scan
  (the modern capture had `0`).
- A **lossy link** (clumsy on Windows, or `netshape.sh`/a real WAN) to make the server actually emit
  parity packets — Windows' FEC is loss-driven, so a clean link may show none even on v1/v2.

**Decision procedure (unchanged capture-first gate — do NOT build an encoder before this):**
1. Capture a legacy-Windows lossy session under induced loss.
2. `scripts/fec-scan.sh <pcap>` — looking for `negotiated version 0x0001/0x0002` **and** `FEC packets > 0`
   (the tool already dumps `snCoded / snSourceStart / uRange / uFecIndex` for each FEC datagram on a GO).
3. If GO: the captured parity payloads + the source packets they cover are what let us reverse-engineer the
   **undocumented GF(256) coefficient table** (the spec defines the header but not the matrix). Only then is
   an encoder worth writing.

**Caveat that gates the value even if GO:** FEC would help **only legacy clients** — modern mstsc/Win11
(RDPUDP2) would still never use it. So a revisit's payoff is narrow (and the 1+1 redundancy stand-in below
already covers the lossy-link case for *any* client without reverse-engineering anything). Weigh that before
committing to the GF(256) work. The tooling (`fec-scan.sh`, `netshape.sh`, the capture-first methodology) is
ready, so the revisit is "get legacy machines → capture → re-run the gate," not "start from scratch."

### P2.3 redundancy stand-in — 1+1 lossy duplicate sends (built 2026-06-27, soak pending)

With real FEC ruled out, the protocol-safe redundancy is a **1+1 repetition code at the RDPEUDP
transport**: on a lossy flow, ship each source datagram **twice** — byte-identical, same sequence number.
On an independent-loss link of rate `p`, a payload is then lost only at `p²` (5% → 0.25%).

- **Why transport-level, not "send each Wave2 PDU twice" (app-level).** App-level duplication would put
  two Wave2 PDUs with the **same `block_no`** on the wire and *rely on mstsc deduping by `block_no`* at the
  RDPSND layer — which MS-RDPEA never specifies, so it risks **double-play**. The transport copy is instead
  de-duplicated by **DTLS anti-replay**: the lossy tunnel is DTLS-encrypted (P2.4a), the duplicate datagram
  carries the *identical encrypted bytes* = the same DTLS record sequence number, and mstsc's DTLS replay
  window drops it before it ever reaches RDPSND. So the audio layer sees each AU exactly once, guaranteed
  by a standard DTLS property rather than an unspecified client behavior. (The RDPEUDP layer itself does
  **not** dedup in lossy mode — deliver-on-arrival is the whole point — so dedup *must* live above it; DTLS
  is exactly that layer.)
- **Implementation.** `ironrdp-rdpeudp` `Config` gained `duplicate_lossy_sends: bool` (default false); when
  set and `mode == Lossy`, `pump()` emits each new source datagram twice. The listener
  (`vendor/ironrdp-server/src/multitransport/listener.rs`) sets it on a lossy peer behind the experimental
  env **`MACRDP_UDP_LOSSY_AUDIO_DUP`** (default OFF; needs `MACRDP_UDP_LOSSY_DELIVERY` so the flow is on the
  lossy SM). Reliable flow + default build are byte-unchanged. Unit-tested in `ironrdp-rdpeudp` (duplicate
  emitted byte-identical; controls for flag-off and reliable-mode; a test documenting that the receiver
  does not dedup, i.e. why DTLS must).
- **Cost / caveat.** Doubles the lossy flow's egress bandwidth (audio only today — AAC ~128 kbit/s, so the
  doubling is cheap). It protects against *independent* single-packet loss; it does **not** help against a
  burst that takes out both copies (they ship back-to-back, so a burst longer than the inter-copy gap can
  still drop both — staggering the copies in time would harden that, deferred). It is a stand-in, not FEC:
  no cross-packet recovery, just repetition.
- **Status: built + unit-tested, real-link soak PENDING.** `scripts/soak-lossy-audio.sh` exposes
  `MACRDP_UDP_LOSSY_AUDIO_DUP` as a second A/B axis (dup vs no-dup at a fixed `--loss`). The open question
  the soak answers: does 1+1 redundancy actually close the 5%-loss audio gap on mstsc (audio stays smooth),
  or does the residual `p²` loss / burst loss still stall the AAC decoder? If yes → keep it (gated); if no →
  fall back to lever (2), document lossy-audio-over-mstsc as having no clean win.

### Ack-driven IDR recovery (EGFX-on-lossy video) — MVP detection spec (scoped 2026-06-27)

The video analogue of the lossy-audio problem: when EGFX H.264 rides the **lossy** tunnel
(`MACRDP_UDP_MIGRATE_EGFX_LOSSY`), a dropped frame makes every subsequent P-frame undecodable (they
reference the lost frame) → freeze/corruption until the next periodic IDR (≤ `--keyframe-interval`, default
2 s). RDP has **no NACK** and the EGFX decoder expects whole frames, so codec-internal concealment isn't an
option — but RDP *does* have `RDPGFX_FRAME_ACKNOWLEDGE`, so the server can **infer** loss from ack-staleness
and force an IDR early (≈ RTT + timeout instead of up to 2 s). This is a *partial* mitigation, not graceful
loss-tolerance — see caveats. (The real fixes — transport FEC, or LTR+NACK — are blocked/architecturally
hard; see "P2.3 FEC capture RESULT" and the H.264-under-loss discussion.)

**Why ack-staleness infers loss.** The client only acks frames it successfully decoded+presented. A
slow-but-healthy client either keeps acking (slowly) or suspends acks under load
(`queueDepth == 0xFFFFFFFF`). **True loss** is the one case where we keep shipping but acks stop *entirely*
(the client is stuck on the gap, nothing new to ack). So "actively shipping + acks went silent" ≈ loss.

**MVP detection (wall-clock ack-staleness; no frame-id matching, no timer thread).**
Per-connection state in `ConnectionContext` (`src/h264.rs`), all reset on reconnect, init to `now`:
`last_ack_at` (set in `on_frame_ack`), `acks_suspended` (= `queueDepth == 0xFFFFFFFF`, set in
`on_frame_ack`), `last_ship_at` (set in the ship loop), `last_recovery_at`, and `egfx_on_lossy` (a shared
`Arc<AtomicBool>` the vendored server sets true when it migrates EGFX onto the **lossy** tunnel — mirrors
the `udp_tunnel_bound` shared-flag pattern). The pure, unit-testable predicate (takes `Duration`s so tests
need no real clock):

```
should_force_recovery_idr(since_ship, since_ack, since_recovery, acks_suspended, egfx_on_lossy, p) =
       egfx_on_lossy                         // ❶ lossy tunnel only (reliable/TCP: missing ack = congestion)
    && !acks_suspended                       // ❷ acks must be on to infer loss
    && since_ship      <= p.active_window     // ❸ we're actively shipping frames
    && since_ack       >= p.ack_stall         // ❹ but acks went silent → inferred loss
    && since_recovery  >= p.min_recovery_interval  // ❺ rate-limit IDR storms
```

Checked in `submit_bgra` (runs per capture tick **and** per flush-burst re-submit — so a loss just before
the screen goes static still heals during the flush window); on true, set the existing `need_keyframe = true`
(one-shot, already consumed by the next encode) and `last_recovery_at = now`. No new IDR path or timer
thread; the periodic `--keyframe-interval` IDR backstops the rare tail.

MVP params (60 fps defaults, env-tunable): `ack_stall` 200 ms (`MACRDP_UDP_EGFX_ACK_STALL_MS`),
`active_window` 500 ms (`MACRDP_UDP_EGFX_ACK_ACTIVE_MS` — covers the flush window),
`min_recovery_interval` 1000 ms (`MACRDP_UDP_EGFX_ACK_RECOVERY_MS`). Whole feature behind
`MACRDP_UDP_EGFX_ACK_RECOVERY` (default OFF) **and** the runtime `egfx_on_lossy` gate; feature-off keeps
`on_frame_ack` trace-only and the path byte-identical.

**Caveats (why it's a narrow, partial mitigation):** (1) lossy-EGFX-only — on TCP / the reliable tunnel a
missing ack means congestion and an IDR would *worsen* it, so the `egfx_on_lossy` gate is load-bearing;
(2) the recovery IDR is itself loss-vulnerable (a big frame on a lossy link), so it can fail and retrigger
— `min_recovery_interval` bounds the storm; (3) acks ride the same lossy tunnel, so a lost ack is a
false-positive source — the staleness *window* + rate-limit absorb the occasional one; (4) incremental over
the periodic IDR (≤2 s → ~RTT+`ack_stall`); (5) mstsc-only. **Build status (2026-06-27):** IMPLEMENTED
behind the env gate, default OFF. `src/h264.rs` has the pure predicate `should_force_recovery_idr` +
`recovery_config_from_env` (7 unit tests covering each gate, threshold-inclusivity, and the lossy/reliable
split), the `ConnectionContext` state (`last_ack_at`/`acks_suspended`/`last_ship_at`/`last_recovery_at`),
the `on_frame_ack` + `ship_frames` stamping, and the `submit_bgra` check (logs `EGFX ack-stall on lossy
tunnel — forcing recovery IDR`). The vendored `ironrdp-server` flips the shared `egfx_on_lossy` flag at the
EGFX→UDPFECL Soft-Sync site (`set_egfx_on_lossy_handle`, server.rs); `main.rs` creates the `Arc<AtomicBool>`
and wires both ends. Feature-off path is byte-identical (only cheap `Instant::now()` stamps run). **Verify
status:** the recovery **trigger** was observed firing correctly on the lossy tunnel on real mstsc
(`forcing recovery IDR` under ack-stall, with EGFX migrated onto UDPFECL); the full A/B quantification (heal
time vs the feature off under induced loss) is still the user's call. Stays default-OFF.

### LTR (Long-Term Reference) recovery — TRIED AND REMOVED (negative result, 2026-06-28)

Briefly shipped (PR #76) then **reverted** after live testing. The idea was the standard RTC fix — code the
recovery frame as a P-frame against the last *acknowledged* LTR instead of a full IDR (cheaper, more
loss-survivable). The **encoder side worked end-to-end**: VideoToolbox emitted LTR frames (`ltr=Some` on the
wire), the `frame_id → token` map + the `RDPGFX_FRAME_ACKNOWLEDGE → acknowledge_ltr_tokens` feedback loop
closed (`LTR frame acknowledged to encoder` fired repeatedly), and the ack-stall trigger drove
`ForceLTRRefresh`. **But no RDP client could decode the result, so it was removed.** Two hard findings from
real clients (2026-06-28):

1. **VideoToolbox only emits LTR frames under the low-latency rate controller.** With the default RealTime
   encoder, `EnableLTR` is *accepted* but VT codes **zero** LTR frames (all `ltr=None` across 86 frames) — so
   the LTR refresh permanently degraded to an IDR (== the #75 IDR recovery, no benefit). Tokens only appeared
   once we co-enabled `EnableLowLatencyRateControl`.
2. **The resulting long-term-reference bitstream is rejected by RDP's AVC420 decoders.** With low-latency+LTR
   on, a **fresh mstsc rendered cleanly for ~1 s then abruptly `Connection reset by peer`** the moment
   LTR-*referencing* frames appeared (it tolerated LTR-*marked* frames, but `ref_pic_list_modification` to a
   long-term index is fatal to its decoder). **FreeRDP stayed up but rendered fully blank with 0 frame acks**
   on the same stream; the **control run (same FreeRDP, plain H.264, no LTR) rendered clean** — isolating the
   blank to the LTR bitstream, not a FreeRDP H.264-decode gap. So both available AVC420 clients reject it.

**Conclusion:** VideoToolbox H.264 LTR is not usable on the EGFX/AVC420 path — the only configuration that
makes VT emit LTR (low-latency RC) produces a long-term-reference bitstream RDP clients can't decode. The
recovery mechanism stays the **ack-driven IDR recovery (PR #75)**, which both clients accept (plain H.264 over
the lossy tunnel renders stable on mstsc). **Do NOT re-attempt VT-LTR for this path** unless a future
client/VT combination is shown to decode long-term-reference AVC420. The encoder-side LTR plumbing
(`EncodedFrame.ltr_token`, `Encoder::{acknowledge_ltr_tokens,request_ltr_refresh}`, the low-latency encoder
spec, the h264 token map, `MACRDP_UDP_EGFX_LTR*`) was reverted with PR #76's revert.

- **P2.4a — MS-RDPEMT tunnel over DTLS (DONE, GREEN 2026-06-26).** The prerequisite
  for any lossy channel: decrypt the client's DTLS application records, answer its
  `RDP_TUNNEL_CREATEREQUEST` with a `CREATERESPONSE(S_OK)` re-encrypted through DTLS,
  binding the tunnel to the TCP session via the same cookie registry as the reliable
  flow. **Verified live on real mstsc:** handshake → CREATEREQUEST (cookie matched the
  issued cookie) → CREATERESPONSE → **tunnel established, and the client stopped
  retransmitting** (the definitive "it accepted us" signal). Implementation: added
  `DtlsConn::recv`/`send` (post-handshake `ssl_read`/`ssl_write`) and made
  `handle_emt_tunnel` transport-agnostic (returns the response plaintext; the caller
  encrypts via rustls *or* DTLS) — the reliable path is unchanged. This is the gateway
  to lossy audio (P2.4b below).

- **P2.4b — lossy audio DVC (the actual payoff).** Researched and important: audio over
  UDP is **NOT** a redirect of the static `rdpsnd` SVC. It requires a **dynamic virtual
  channel**, `AUDIO_PLAYBACK_LOSSY_DVC` (MS-RDPEA §2.1; FreeRDP calls it
  `RDPSND_LOSSY_DVC_CHANNEL_NAME`), migrated onto the tunnel via Soft-Sync. The good news:
  the **PDU payloads are reusable** — the Wave2 / AAC access-unit encoders from
  `src/audio.rs` + `src/aac.rs` are unchanged; only the channel *envelope* changes (DVC
  instead of SVC). Windows **gates** this channel on **AAC + protocol v8 + a UDP transport
  being present**, so `--enable-aac` becomes a prerequisite for lossy audio. Reuse the
  Phase-1 Soft-Sync codec (`vendor/ironrdp-dvc`) to migrate the new DVC onto the lossy
  tunnel. This is the **substantial, genuinely-new** piece of Phase 2 — a new audio
  channel, not a wiring change.

  **P2.4b-1 spike result (2026-06-27): the DVC audio handshake works — but the EGFX
  "negotiate-on-TCP-then-migrate" pattern does NOT carry over to a *lossy*-named channel.**
  Verified on real mstsc. Built a `DvcProcessor` for the audio DVC
  (`vendor/ironrdp-server/src/multitransport/audio_dvc.rs`, gated behind the experimental
  `MACRDP_UDP_LOSSY_AUDIO` env, default off) that, on channel open, sends Server Audio
  Formats (v8) and runs the MS-RDPEA handshake, reusing `ironrdp-rdpsnd`'s
  `ServerAudioOutputPdu`/`ClientAudioOutputPdu` codecs verbatim (the **SNDPROLOG header is
  kept** — byte-identical to the static path). Three concrete findings:
  - **A channel literally named `AUDIO_PLAYBACK_LOSSY_DVC` is rejected if the server pushes
    anything on it over TCP/DRDYNVC.** mstsc accepts the DVC Create (it can't refuse), then
    on receiving our Server Audio Formats over TCP it goes **silent and stops reading the
    whole TCP socket** (broken pipe ~3–4 s later — it kills EGFX/everything, not just
    audio). So the lossy DVC must be **Soft-Synced onto the lossy tunnel *before* any data**,
    with the handshake running over the tunnel — the opposite of EGFX, where the channel
    negotiates over TCP and is migrated to the *reliable* tunnel afterward. (This contradicts
    a naive reading of MS-RDPEDYC Soft-Sync — "all DVC data rides DRDYNVC until Soft-Sync
    completes" — which is true *in general* but the `_LOSSY_`-named channel is the exception:
    mstsc treats data on it over TCP as a violation.)
  - **The reliable `AUDIO_PLAYBACK_DVC` name works perfectly over TCP** (diagnostic env
    `MACRDP_AUDIO_DVC_RELIABLE=1`): Server Audio Formats → Client Audio Formats (AAC chosen)
    → Client Quality Mode → Server Training → Client Training Confirm all round-trip, audio
    plays, EGFX stays healthy. This is the discriminator that proved the channel *name* is
    the blocker — not the PDU framing, and not coexistence with static rdpsnd.
  - **Dual negotiation is fine.** Running the static `rdpsnd` SVC and the audio DVC
    simultaneously does NOT confuse mstsc (it negotiated AAC/v8 on both with no conflict) —
    so a server can keep static rdpsnd as the fallback while offering the DVC.
  - **Sequence note (spec-confirmed live):** for v6+, the client sends a **Quality Mode PDU
    immediately after Client Audio Formats**, and the server sends **Training only after
    that** (MS-RDPEA "Initialization Sequence"). The handler waits for Quality Mode before
    replying with Training.

  **P2.4b-2 spike result (2026-06-27, the linchpin de-risk): mstsc ACCEPTS a *lossy*
  Soft-Sync of the audio DVC without tearing down.** This was the open question P2.4b-1 left
  ("does a lossy Soft-Sync itself trip mstsc, or only format-data-over-TCP?"). Built it:
  `audio_dvc.rs::start()` now returns **no formats** for the lossy name (defer over TCP —
  finding above), and the server Soft-Syncs the channel onto the lossy
  (`TUNNELTYPE_UDPFECL=0x03`) tunnel via the Phase-1 Soft-Sync codec
  (`send_soft_sync_request(.., TUNNELTYPE_UDPFECL, vec![audio_id])`, triggered off the same
  `maybe_soft_sync_on_egfx` machinery that migrates EGFX). Verified live on mstsc:
  `lossy audio DVC opened — deferring Server Audio Formats` → `Sent DYNVC_SOFT_SYNC_REQUEST` →
  **`SoftSyncResponsePdu { tunnels: [3] }`** (UDPFECL accepted) → session stayed alive to a
  graceful disconnect ~13 s later, EGFX (on TCP) acking frames the whole time. **So the #54
  blocker was specifically format data over TCP, NOT the lossy Soft-Sync** — the migration
  primitive is clear. (Note: this run requires `--enable-h264` because the Soft-Sync trigger
  is driven from the EGFX dispatch arm; EGFX stayed on TCP — `MACRDP_UDP_MIGRATE_EGFX` off.)

  **Implication for the build:** the migration topology is now proven end-to-end (defer +
  lossy Soft-Sync accepted). What remains is the *data over the tunnel*: run the MS-RDPEA
  handshake (formats → quality → training) over the lossy/DTLS tunnel for the migrated peer
  (2b-iii), then stream AAC Wave2 over it + reconcile the drop-stale lag model (2b-iv). That
  depends on a solid lossy *data path* (P2.2/P2.3) underneath. And note the reliable
  `AUDIO_PLAYBACK_DVC` path that works over TCP is **not worth landing on its own**: a
  reliable tunnel HOL-blocks under loss exactly like TCP (Phase 1 soak), so reliable
  audio-over-UDP delivers no loss-resilience over TCP audio. **Status: linchpin de-risked;
  data path next.** The verified groundwork (`audio_dvc.rs` + defer + lossy Soft-Sync) is
  kept in-tree, gated off by default (`MACRDP_UDP_OFFER_FECL` + `MACRDP_UDP_LOSSY_AUDIO`).

  **P2.4b 2b-iv result (2026-06-27): audio RENDERS over the lossy UDP/DTLS tunnel —
  VERIFIED end-to-end on real mstsc.** This closes the data path the 2b-ii de-risk pointed
  to. Two sub-steps:
  - **2b-iv-A — dual audio-DVC topology.** Rather than run the MS-RDPEA handshake over the
    tunnel (which would need 2b-iii's tunnel-side handshake plumbing), the build uses **both**
    audio DVCs at once: the RELIABLE `AUDIO_PLAYBACK_DVC` runs the full format/quality/training
    handshake over TCP (the only thing mstsc tolerates — see the P2.4b-1 finding), and the LOSSY
    `AUDIO_PLAYBACK_LOSSY_DVC` is data-only and Soft-Synced onto the UDPFECL/DTLS tunnel. The
    lossy channel **inherits the reliable channel's negotiated `wFormatNo`** via a shared
    `NegotiatedAudioFormat(Arc<AtomicU32>)` (reliable publishes it on TrainingConfirm; the
    server reads it back to stamp Wave2 PDUs). This sidesteps 2b-iii entirely — the handshake
    stays on the channel mstsc accepts it on, and only Wave2 data rides the tunnel. (A "video
    freezes on connect" seen once here was transient mstsc state — a byte-identical retry
    rendered fine; the documented "mstsc caches bad RDP state until reboot" behavior, not a
    topology bug.)
  - **2b-iv-B — AAC Wave2 over the tunnel.** `dispatch_audio` ships each wave as a `Wave2Pdu`
    on `AUDIO_PLAYBACK_LOSSY_DVC` (bare DRDYNVC PDU → `RDP_TUNNEL_DATA`, DTLS-encrypted) once
    all preconditions hold (reliable handshake done + tunnel bound + lossy DVC open), and
    **skips the static rdpsnd TCP write** — exactly one playback path at a time, clean handover,
    no double-play. **Verified live on mstsc:** reliable handshake GREEN → lossy Soft-Sync
    accepted (`tunnels: [3]`) → one-shot marker `streaming Wave2 audio over the LOSSY UDP/DTLS
    tunnel … static rdpsnd now silent` (format_no=0, channel 5) → **audio plays**, lossy UDP
    flow shows continuous client ACKs (growing ACK-vector = steady Wave2 traffic acked), EGFX
    (TCP) keeps rendering, session alive to a graceful disconnect, no teardown. **As far as is
    known this is the first open-source RDP server streaming audio over a UDP multitransport
    tunnel.**
  - **Lag-model reconciliation (the P2.5 item below): no code change needed.** The tunnel
    branch sits after the cross-batch lag model (vendor divergence (2)/(3)/(8)). The drop-stale
    guard still rightly trims stale audio before it floods the tunnel (correct + free on a lossy
    transport — no retransmit to cancel, exactly the property TCP audio lacks); the
    resync-on-stall is moot but harmless (the tunnel send is non-blocking); and
    `audio_shipped_ms += wave_ms` runs on both paths so the model stays coherent. Verified clean
    audio in the live run. The remaining work is the loss soak (P2.2/P2.3 underneath) to prove
    audio stays smooth where TCP audio desyncs — the user-noticeable win.

- **P2.5 — route audio to the lossy tunnel + reconcile the lag model.** Point the audio
  path at the lossy tunnel and reconcile with the existing audio-lag / drop-stale model
  (vendor server divergence (2)/(3)/(8)) — on a lossy transport, dropping a stale wave is
  *correct and free* (no retransmit to cancel), which is exactly the property TCP audio
  lacks. Verify on the netshape soak harness (`scripts/netshape.sh`) that audio stays
  smooth on a lossy link where TCP audio desyncs — the user-noticeable win that justifies
  the whole phase.

- **P2.6 (later, maybe never) — lossy video.** H.264 over a lossy transport needs
  loss-tolerant encoding (periodic intra-refresh / slice-based recovery) so a dropped
  packet doesn't smear until the next IDR. Harder than audio and lower-value (the soak
  proved reliable-video-under-loss is a dead end; lossy-video needs encoder work, not just
  transport). Deferred; revisit only after lossy audio ships and proves the transport.

### Reuse from Phase 1 (most of the infrastructure is already built)

The UDP listener, cookie registry, server↔listener tunnel handoff, MS-RDPEMT tunnel
PDUs, the Soft-Sync codec, the acceptor offer/emit machinery (M3c), the
`ironrdp-rdpeudp` crate, and the netshape soak harness + retransmit observability **all
carry over**. Phase 2 adds: the lossy *offer* (P2.0), one DTLS boundary file (P2.1), a
second delivery policy in the state machine (P2.2), an optional RS encoder (P2.3), and a
new audio DVC (P2.4) — on top of, not instead of, Phase 1.

### Bottom line

Phase 2 was **gated on a one-day go/no-go spike (P2.0)** because the lossy path is a
legacy codepath and it was unproven that modern mstsc would use it at all. **P2.0 ran
GREEN (2026-06-26):** real mstsc advertises `UDP_FECL` + `UDP_PREFERRED`, opens a
`SYN_LOSSY` flow, and starts a **DTLS 1.2** handshake — so the lossy payoff is reachable.
Building it: the highest-value payload is **audio** (a new `AUDIO_PLAYBACK_LOSSY_DVC`,
reusing our AAC encoder), not video; FEC is encoder-only and even then optional; and
DTLS 1.2 via `boring` is the one hard new dependency, quarantined behind a single file.
Phase 1 (reliable EGFX over UDP) remains the proven clean-link feature; Phase 2 lossy
audio is the loss-resilience win, now greenlit to build when prioritized.

## TL;DR

*Original feasibility framing (2026-06-25), kept for the record. The "don't / not
done / multi-month" tone predates the build — Phase 1 has since shipped (EGFX over UDP,
verified on mstsc); see the status block at the top. Outcomes noted inline.*

- **It's possible to extend IronRDP for UDP multitransport (MS-RDPEMT over
  MS-RDPEUDP/EUDP2), and IronRDP's sans-I/O design is arguably a *better*
  foundation for it than FreeRDP's** — the PDU codecs and the reliability/FEC
  engine fit the sans-I/O idiom and stay unit-testable. (Borne out — though the
  feared "invasive `ironrdp-server` I/O refactor" wasn't needed: EGFX rides an mpsc
  handoff to the listener via a flag-gated branch, so the hot path is unchanged when
  the feature is off.)
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
  migration via `drdynvc`). **Server-side UDP is pioneering** — even FreeRDP only
  finished the *client*; its server path is a bootstrap stub. (Phase 1 did the server
  glue — listener, cookie→session binding, drdynvc Soft-Sync migration — as a
  flag-gated mpsc handoff rather than "a second writer in the dispatch loop"; DTLS is
  still untouched, deferred to Phase 2.)
- **Recommendation (as it played out):** it was built — Phase 1 (reliable UDP, EGFX
  migration) ships **default-OFF**, there for the off-LAN use case when wanted, not the
  LAN default. Phase 2 (lossy + DTLS + FEC) stays gated on the LAN→WAN use-case shift.

## Context — what macrdp / IronRDP do today

*(This section describes the pre-multitransport baseline that motivated the work; as
of Phase 1, macrdp can additionally serve EGFX over a UDP tunnel — see the status
block at the top.)*

macrdp served **everything over one TLS-over-TCP connection** — EGFX video,
RDPSND audio, input, clipboard, RDPDR — multiplexed through a single
`SharedWriter` (`Rc<Mutex<&mut W>>`) in the vendored `ironrdp-server`'s
`client_loop`. **Upstream** IronRDP is **TCP-only**: no MS-RDPEUDP / MS-RDPEMT, no UDP
transport crate (confirmed 2026-06-25: no such crate in the upstream tree) — which is
why macrdp built the vendored `ironrdp-rdpeudp` crate + server-side glue. With Phase 1,
EGFX (when migrated) now rides a UDP tunnel; input, audio, clipboard, and RDPDR still
ride the TCP `SharedWriter`.

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
   shape IronRDP favors, so it can be built and tested without sockets. (Built as
   `ironrdp-rdpeudp`'s `state.rs`, proven by a two-instance in-memory
   loss/reorder/dup test — the idiom held.) Because the UDP data path is
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

4. **Server I/O integration (`ironrdp-server`).** *(Predicted "architecturally
   invasive"; the build came in lighter — see the inline corrections.)* The
   server model assumes one `Framed` byte-stream writer. UDP needs:
   - A **UDP listener** that accepts arbitrary inbound flows and **associates
     each with an existing TCP session** by generating + **validating the cookie**
     in the Tunnel Create Request. This server-only glue is *not* in FreeRDP's
     client and is **stubbed** in FreeRDP's server (`multitransport_server_request`
     only sends the bootstrap PDU; the response handler is a no-op).
   - **Channel migration**: steering DVC traffic (EGFX video) off the TCP writer
     onto the UDP transport, which requires `drdynvc` (only channel data rides
     UDP). *(As-built: this was an **additive** flag-gated branch in the existing
     `ServerEvent::Egfx` arm — `egfx_on_udp` → ship via an mpsc `TunnelSender` to the
     listener — plus a new inbound `dispatch_tunnel_inbound` select arm; NOT the
     feared `client_loop` writer refactor. The hot path is byte-identical with the
     feature off.)*
   - **Server-grade sender behavior**: congestion window, RTT estimation, FEC
     ratios, retransmit timers — as the *bulk* sender (video). The FreeRDP client
     shows the mechanism but not the tuning (its sender is lightly exercised).

## Modular integration design (making the server hook elegant)

> **As-built (2026-06-26) — the shipped Phase 1 differs from the sketch below; the
> *reasoning* holds but the type/file names don't.** No `TransportRouter` / per-channel
> writer handles / `Channel` enum were built. EGFX is shipped over the tunnel by
> `RdpServer::route_egfx_over_udp` — it encodes each message via
> `SvcMessage::encode_unframed_pdu` (bare DRDYNVC PDU; the tunnel provides framing) and
> hands it to a `TunnelSender` mpsc the listener drains — gated by an `egfx_on_udp`
> bool that the **listener-driven Soft-Sync path** sets, so the `ServerEvent::Egfx`
> dispatch arm just branches on that flag (and a symmetric inbound path drains
> `RDP_TUNNEL_DATA` back into the drdynvc processor). The `MultitransportProvider`
> trait is a single method (`requested_protocol`), NOT `offer`/`start` — the
> negotiation **offer moved into the acceptor** (it must go out after licensing, before
> Demand Active; a post-finalization send is rejected by real clients). The server
> `src/multitransport/` tree is just `mod.rs` (trait + `CookieRegistry` + `TunnelSender`)
> + `listener.rs` (UDP socket + RDPEUDP SM + rustls + MS-RDPEMT tunnel); there is no
> separate `session.rs`/`router.rs`/`migration.rs`/`dtls.rs` (DTLS is Phase 2). The
> `ironrdp-rdpeudp` crate's files are `pdu.rs`/`eudp2.rs`/`emt.rs`/`datagram.rs`/
> `state.rs`, not `reliability.rs`. Soft-Sync needed a 4th vendored crate
> (`ironrdp-dvc`). The original sketch is kept below for the design rationale.

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

## A cheaper middle path (this is what Phase 1 built)

> **This section's "middle path" is exactly what shipped as Phase 1** — reliable UDP +
> ordinary rustls TLS, no DTLS, no new crypto dep. (One spec nuance found in the build:
> with **mstsc** the reliable channel is plain RDPEUDP **v1/v2 carrying TLS**, not
> EUDP2 — so the v1 reliability SM is the live codepath; the EUDP2 codecs exist but are
> off mstsc's reliable critical path.)

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

This is **far bigger than macrdp's existing vendored divergences** (small, targeted
patches): a whole new transport stack (`ironrdp-rdpeudp`) + server-side glue in
vendored `ironrdp-server` + a Soft-Sync codec in vendored `ironrdp-dvc`.

**What was actually done (and why):** the user explicitly accepted **building it
vendored first, upstreaming later once proven** — so Phase 1 shipped as vendored
divergences, deliberately forgoing the "don't carry big vendor forks" preference for
now. The server-side data path turned out to be **less invasive than feared**: no
`client_loop` writer refactor was needed (EGFX rides an mpsc `TunnelSender` to the
listener via a flag-gated branch in the existing `ServerEvent::Egfx` arm), so the hot
path stays byte-identical when the feature/flag is off. This **pioneered the server
side** — as far as is known the first OSS RDP server with a working UDP data path
(FreeRDP's server is a bootstrap stub; verified 2026-06-26). **Still to do:** upstream
the `MultitransportProvider` extension point + the transport crate to Devolutions once
the data path is soaked — until then it rides as a vendor divergence. Implementation
tracked the MS-RDPEUDP/EMT specs, with David Fort's out-of-tree FreeRDP RDPEUDP
prototype + the `rdp-udp` Wireshark dissector as partial references (FreeRDP's
*merged* client has no UDP path), and mstsc as the gate (which caught the
v2-carrying-TLS reality and the bare-DRDYNVC tunnel framing).

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

*Original recommendation, with outcomes noted inline — the original framing
("possible / multi-month / not started") is kept for the record.*

- **Gate on the use case, not on feasibility.** It's *possible* and IronRDP is a
  decent host; it's just multi-month and only pays off off-LAN. → **DONE for Phase 1**
  (default OFF; it's there for the off-LAN use case when wanted, not the LAN default).
- **Phase 1 = the PDU crate + a reliable-UDP-only (RDPEUDP2/TLS) path** (no DTLS, no
  new crypto dep, smaller refactor) to prove the transport-migration plumbing in
  `ironrdp-server`. → **DONE** — `ironrdp-rdpeudp` + the listener + rustls + MS-RDPEMT
  tunnel + Soft-Sync EGFX migration; EGFX renders over UDP on mstsc. (Note: mstsc's
  reliable channel is RDPEUDP **v2 carrying TLS**, not EUDP2 — the v1 SM is the live
  codepath; the EUDP2 codecs are built but off mstsc's reliable critical path.)
- **Phase 2 = lossy UDP + DTLS + FEC** for the real video win, securing the lossy
  transport with **`boring`** (or `openssl`). → **NOT started** (deferred; the
  known-reachable follow-up).
- **Upstream** the extension point + transport crate to Devolutions once soaked. →
  built **vendored first** (the accepted approach); upstreaming is the open follow-up.
- Validate against FreeRDP *and* mstsc; treat the specs as authoritative. → did both.

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

**Phase 1 BUILT and verified — see the status block at the top of this doc for the
milestone-by-milestone detail.** (This section originally read "exploratory / not
started, no code"; that's long obsolete — EGFX H.264 renders over the reliable UDP
tunnel on real mstsc, behind `--enable-udp-multitransport` + `MACRDP_UDP_MIGRATE_EGFX`,
default OFF.) Phase 2 (lossy `UdpFecL` + DTLS + FEC) remains exploratory / not started.
Cross-reference: `docs/usb-redirection-feasibility.md` (the other
"big protocol + new transport layer" scoping doc) and the A/V-contention quirk in
`docs/known-quirks.md` (the single-connection coupling this partly relieves for video).
