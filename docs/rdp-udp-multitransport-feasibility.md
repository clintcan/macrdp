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
3. **Client UDP support** — a *client* that connects out over UDP. A different
   (and easier-to-reach) codebase than the server. Notably, even FreeRDP — the
   most complete OSS stack — has never merged this either: its client declines
   UDP with `E_ABORT`, and the RDPEUDP/RDPEUDP2 work stayed an out-of-tree
   prototype (re-verified against full git history 2026-06-26).

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

- **P2.3 — FEC encoder (optional, deferrable).** Researched: MS-RDPEUDP FEC is
  **GF(256) Reed–Solomon**, not XOR parity; each FEC packet recovers exactly **one** lost
  source packet within its range; the wire header is `RDPUDP_FEC_PAYLOAD_HEADER`
  (`snCoded`, `snSourceStart`, `uRange`, `uFecIndex`). A **send-only server needs only the
  *encoder***, and **even that is optional** — it's a loss-recovery *optimization*, not
  required for the lossy transport to function. So **ship lossy-without-FEC first**
  (P2.4/P2.5 below work without it), then add the RS encoder as a measurable
  loss-resilience improvement. This de-risks: we get a working lossy audio path before
  taking on a Reed–Solomon implementation.

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
