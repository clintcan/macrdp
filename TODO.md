# TODO / work queue

A living checklist of what's open, deferred, or parked. Detail lives in the
linked docs / vendored `CLAUDE.md`s / commit history — this is just the index of
"what's currently to be made." Keep it pruned: move items to *Done* only briefly,
then delete; promote a parked item to *In flight* when work actually starts.

## In flight (needs an action)

- [ ] **Tier 2.4 — multi-day soak (foundation core PASSED 31 h 2026-07-03; full 48–72 h on
  v0.8.22+ still needed).** The last leg of the production-readiness trio (TLS ✓ #104, auth ✓
  #105). Tooling: `scripts/soak-monitor.sh monitor` samples a resource CSV; `… analyze`
  summarizes. **2026-07-01 run (31 h / 1861 samples, pre-v0.8.22 / pre-ARC build; data recovered
  on a clean re-copy after a first transfer zero-filled):** CLEAN — **no memory leak** (RSS
  bounded 18–88 MB, tracks activity, ended lower than start), no fd/thread/SCStream/NFS/log
  growth, **single process throughout**, **0 panics** (the 55 `Connection error`s are normal
  per-connection client-drop/probe signatures). **v0.8.21 auth-guard fix FIELD-VALIDATED:** the
  17 lockouts of a legit LAN client (escalating to ~239 s) are all **pre-fix** (06-30 + 07-01
  02:xx, before the 18:39 build swap — the false-lockout that *surfaced* v0.8.21, and the "took
  a few tries while I was out" incident); the **post-fix soak window had ZERO lockouts** + 14
  balanced accept/disconnect pairs. **Still open because:** (a) 31 h < the 48–72 h target; (b)
  predates v0.8.22 → **blank-recovery detector** (per-QoE-callback, can drop the connection) +
  **ARC cookie** not exercised. **Next action:** 48–72 h re-soak on **v0.8.22+**, biased toward
  reconnect cycles. Logging fixes for that run: `fsync`/`F_FULLFSYNC` the soak logger (or tee to
  the macOS unified log) so a transfer can't zero-fill it; demote the `multitransport`/`audio_dvc`
  "GREEN" status lines from WARN to INFO/DEBUG. See `docs/production-readiness-roadmap.md` Tier 2.4.

## Deferred — scoped, not started

- [x] **Softer UDP adaptive-bitrate signal over WiFi (retransmit tolerance).** SHIPPED
  2026-06-29 (#100): `MACRDP_UDP_ADAPTIVE_RETX_TOLERANCE` (default 2) so sporadic wireless
  retransmits don't ratchet the bitrate down. BUT the live mstsc/WiFi6 test disproved the
  hypothesis for that link — the degradation was **ack-lag-driven** (tunnel wedging /
  HOL-block, `retransmit_delta=0` on every decrease), not retransmit-driven, so the
  tolerance never engaged and the watchdog de-migrated to TCP. Kept as a correct low-risk
  improvement for links that *do* retransmit; the WiFi6 answer stays `UDP_MIGRATE_EGFX=0`.
  The reliable-tunnel retransmit counter barely fires in practice — ack-lag is the dominant
  signal. (Remove this line next prune.)

- [ ] **Watchdog follow-up: keep the de-migrated UDP tunnel from timing out.** Found
  2026-06-29 during P1 testing: under *sustained* heavy loss the watchdog de-migrates EGFX
  to TCP (video keeps running), but ~60s later **mstsc resets the whole session**
  (`Connection reset by peer`) — its multitransport dead-tunnel timeout, since the UDP
  tunnel it Soft-Synced EGFX onto goes silent after de-migration. Fix: after de-migration,
  either send RDPEUDP **keepalives** on the abandoned tunnel so mstsc doesn't time out, or
  cleanly **close** the multitransport tunnel so mstsc stops expecting it. Must distinguish
  "de-migrated, client still here" from "client gone" (interacts with the 60s idle-GC).
  Still strictly better than the pre-watchdog permanent freeze; only bites under sustained
  loss. See the v0.8.x test log (wedge 23:44:49 → reset 23:45:50, 2026-06-29).

- [ ] **Congestion-responsive encoder rate control + frame dropping** (highest-value
  video-under-loss work — helps BOTH the default TCP path and UDP). **P1 (adaptive bitrate)
  SHIPPED as #94; P2a (IDR backoff + ack-lag signal switch) SHIPPED 2026-06-29;
  P3 (controller runs on the TCP path too) SHIPPED 2026-06-29** (verified on real mstsc:
  with `--bitrate 8` on a pure-TCP session, clean connect via the cold-start guard, gentle
  8M↔5.6M sawtooth backoff/recovery under 5% TCP drop — see the feasibility doc "P3" note;
  ack-lag on TCP is a real but spiky/lower-amplitude signal, tuned via a ¾·max_frame_lag
  threshold). **EWMA smoothing + hysteresis + 3-zone hold SHIPPED 2026-06-29** (verified
  mstsc: per-spike sawtooth → one gentle step-and-recover per episode; A/V more in sync,
  catch-up speed-up cut, "video sometimes stops" gone — see feasibility doc). **P2b
  frame-rate floor SHIPPED 2026-06-29 (#102, verified mstsc):** once bitrate is pinned at
  the floor AND still congested, cap the effective fps (drop captures within `1/floor-fps`,
  default 10 fps, never zero) to shed packet load — on BOTH transports (the only fps lever
  on TCP). Pure `frame_drop_at_floor`; `MACRDP_ADAPTIVE_FLOOR_FPS` tunable. Live log
  confirmed: bitrate AIMD → 750k floor under rising ack-lag → P2b caps to ~10 fps
  (choppy-but-steady, in sync) → recovers to the 6M ceiling + full fps when loss clears.
  Remaining sub-pieces: a **stronger TCP signal** (`TCP_CONNECTION_INFO` RTT+retransmits /
  write-backpressure — less spiky than ack-lag); the CN/RTT/window signals; and tuning the
  control law against a real-Windows-server capture. **Concrete
  manifestation found 2026-06-28:** EGFX-over-UDP reconnect freezes *intermittently*
  because the server never throttles on the client's EGFX `queueDepth` —
  `GfxHandler::on_frame_ack` only records ack timing, so macrdp ships at full rate while
  the client's queue runs away (observed peak ~352k) → frozen display + RDPEUDP ACK
  storm. A focused **queue_depth-aware throttle / frame-drop** (a sub-piece of this item)
  is the targeted fix; note raw `queueDepth` is oddly large even when healthy (~30k–82k),
  so capture a real-Windows-server baseline before picking a threshold. **First sub-piece
  SHIPPED (#89/v0.8.17):** a frame-ack-lag backpressure gate in `submit_bgra` with a
  trickle floor (drop most captures when the client's ack lag is high, but never to zero —
  mstsc needs trailing frames to present/ack) — removes the *load*-induced freeze on a
  clean link; the fuller congestion-responsive controller below remains. Today macrdp ships
  a fixed ~60 fps + a 2 s periodic IDR regardless of the client's congestion signal, so
  under loss the ordered stream HOL-blocks and EGFX **freezes** (finding #3/#4). A real
  Windows server under the same ~8% loss **degrades gracefully** — skips frames / drops
  framerate, never freezes — via **URCP congestion control + encoder-side frame dropping**,
  and *without* FEC (observed 2026-06-28, finding #5). The lever: feed a controller the
  RDPEUDP feedback macrdp **already receives and ignores** → dynamically lower
  VideoToolbox bitrate, skip frames to a lower effective fps, and back off the periodic
  IDR under congestion. Concrete signals already on the wire:
  - `RDPUDP_FLAG_CN` (congestion notification; reply `RDPUDP_FLAG_CWR`) — **the exact
    "CN" mstsc sent in finding #4 before abandoning the tunnel, which we ignored**;
  - loss = gaps in `RDPUDP_ACK_VECTOR_HEADER` (+ retransmit events);
  - RTT / queuing-delay trend from ACK timing (+ RDPEUDP2 ack timestamps / AckOfAcks);
  - flow ceiling = peer's advertised `uReceiveWindowSize`.
  Needs *a* controller on those (delay+loss+CN), **not** a full URCP reimplementation.
  Design shape:
  - **`--bitrate`/`--fps` become the clean-link *ceiling*, not fixed targets** (today
    both are set once at session start). Controller operates in `[floor, ceiling]`; clean
    link = full configured rate (unchanged from today), congestion pulls down, recovery
    climbs back. Needs a **floor** (~0.5–1 Mbps / a few fps) so it degrades to "choppy
    but alive," not dead.
  - **Lever order:** (1) lower bitrate first — `kVTCompressionPropertyKey_AverageBitRate`
    is **live-settable**, no encoder rebuild (softer image, smooth motion); (2) drop
    frames (don't submit captured frames) for lower effective fps when bitrate cuts aren't
    enough — fewer frames + fewer packets, the visible "skipping"; (3) **back off / stretch
    the periodic IDR while congested** (a ~240 KB keyframe is the worst thing to inject —
    it's what wedges macrdp today), force one only on real recovery.
  - **Two feedback adapters, same output.** UDP path reads the RDPEUDP signals above
    (CN / ACK-vector loss / ack-timing RTT). **TCP path can't see per-packet loss** (kernel
    hides it) → read **write backpressure** (SharedWriter blocking / send-buffer full) +
    `TCP_CONNECTION_INFO` (RTT, retransmits via `getsockopt`, macOS). Same controller +
    same VT-bitrate/frame-drop/IDR-backoff levers; only the input source differs per transport.
  Bigger than FEC (dead) or auto-fallback (band-aid). Worth a real-server↔mstsc capture
  first to read the URCP signaling. See finding #5 in `docs/rdp-udp-multitransport-feasibility.md`.
- [ ] **SCReAM-style controller upgrade (replace the AIMD control law)** — scoped
  2026-06-30, the concrete next step of the rate-control item above. Swap the current
  `--adaptive-bitrate` AIMD law for an **open, real-time congestion controller**: SCReAM
  (RFC 8298, self-clocked off acks + one-way-delay + loss — best fit for RDPEUDP feedback)
  or a loss+RTT hybrid (Copa/BBR-lite). **Do NOT implement "URCP" by name** (no public
  reference impl; outside the MS-RDPEUDP2 Open Specifications Promise → patent/licensing
  gray zone) and **do NOT link libwebrtc/GCC** (huge C++ dep; GCC also fits worst — its
  delay controller wants TWCC per-packet arrival timestamps RDPEUDP doesn't natively
  produce). Steps: (1) extract the control law behind the existing controller seam, keeping
  AIMD as an A/B fallback; (2) implement the SCReAM core in Rust (~a few hundred lines):
  target rate / congestion window from OWD trend + loss + `RDPUDP_FLAG_CN`, self-clocked
  off acks; (3) **feed it from the transport-level RDPEUDP datagram acks** (already parsed
  in vendored `ironrdp-rdpeudp`), not today's coarse one-per-frame GFX `FrameAcknowledge`
  lag — TCP path keeps its `TCP_CONNECTION_INFO` / write-backpressure signal; (4) outputs to
  the existing VT levers (live `AverageBitRate`, `frame_drop_at_floor`, IDR backoff — already
  wired); (5) tune + verify on real mstsc under loss, A/B vs AIMD. **Value:** better graceful
  degradation on BOTH the reliable UDP tunnel AND the TCP path. **Caveat (the easy half):**
  this does NOT stop the reliable-ordered-tunnel HOL-block *freeze* — that needs the deferred
  lossy-video substrate (these CCs assume a droppable flow); a better controller on a reliable
  stream still HOL-blocks. So it's a standalone graceful-degradation win, not the freeze cure;
  `UDP_MIGRATE_EGFX=0` + watchdog→TCP stay the robust answer until the substrate exists.
  mstsc-primary for the UDP path (Mac/FreeRDP are TCP-only). See finding #5 (the open-CC
  analysis + signal-mapping table) in `docs/rdp-udp-multitransport-feasibility.md`; refs
  SCReAM RFC 8298, NADA RFC 8698.
- [ ] **A/V desync under packet loss** (user-reported 2026-06-29, after P3). Audio drifts
  from video under drops, most apparent on the TCP path. Root constraint: **RDP has no A/V
  sync primitive** (RDPSND + EGFX are independent channels, no shared clock/PTS) → true
  lip-sync is impossible, only *reduce* drift. The P3 adaptive video catches up (speeds/
  slows) while fixed-rate audio can't, which makes the drift visible. Two partial levers,
  two levers: **A** — EWMA/hysteresis smoothing of the video controller (**DONE
  2026-06-29**, shipped with the rate-control EWMA work above; user confirmed "audio more
  in sync" + less catch-up speed-up); **B** (remaining) — tighten the audio-lag resync
  (vendored `dispatch_audio`, ~300 ms threshold tuned for resize-freezes, not slow drift)
  to keep audio live, at the cost of choppier audio. Lever A substantially improved it;
  B only if the residual drift/skips still bother in daily use. Detail:
  `project_av_sync_under_drops` memory.
- [ ] **EGFX-over-UDP watchdog — ack-lag-pegged secondary trigger** (refinement of the
  shipped watchdog above). The watchdog fires on ~3s of *fully silent* acks; a real wedge
  dribbles a few stray acks before going silent, so it latched at `since_ack_ms≈7.7s` in
  the clumsy soak (vs. 3s ideal). Add a second trigger that fires when the frame-ack *lag*
  (shipped−acked) stays pinned above threshold for N s even if odd acks trickle in, to
  shorten the real-wedge recovery window. Low priority — the freeze already recovers.
- [ ] **UDP multitransport — explicit TCP-close → listener peer-evict signal** (the
  deferred half of M3c; the *correct* fix for dead-peer reclaim). Today the listener only
  reclaims a peer via the activity-based idle-GC, whose timeout had to be raised to 60s
  (2026-06-29) because mstsc goes near-silent on the UDP flow when idle (~15s keepalive)
  and a shorter timeout reaped *live* idle peers → permanent EGFX freeze. With a real
  TCP-session-close signal (server marks the connection's cookie retired in the shared
  `CookieRegistry` → listener drops that peer), a dead peer is reclaimed *immediately* on
  disconnect and the idle-GC can be a long pure backstop (no risk of reaping a live idle
  client regardless of its keepalive interval). Shared-state signal mirrors the existing
  per-cookie tunnel-bound flag. See vendor `listener.rs` `PEER_IDLE_TIMEOUT_MS` + the
  feasibility doc "M3c peer GC".

- [x] **UDP multitransport Phase 2 — lossy `UdpFecL` + DTLS + 1+1 redundancy** (FEC dropped).
  **Lossy AUDIO is DONE + verified.** Phase 1 (reliable EGFX-over-UDP) shipped (v0.8.15,
  clean-link only). Phase 2 loss resilience for *audio* is now shipped and soaked: lossy
  RDPEUDP delivery (deliver-on-arrival, no retransmit) + DTLS-over-lossy + the dual-channel
  topology (MS-RDPEA format handshake on the reliable `AUDIO_PLAYBACK_DVC` over TCP; AAC
  Wave2 data Soft-Synced onto the lossy `AUDIO_PLAYBACK_LOSSY_DVC`) + **1+1 duplicate-send**
  (each datagram sent twice; client DTLS anti-replay dedups → p→p² loss). **Soak-verified on
  real mstsc 2026-06-29:** dup=0 glitches at 5% loss; dup=1 stays smooth at 5/10/15% (trace
  jitter only at 15%). Promoted from the four expert env gates to the single
  **`--enable-lossy-audio`** flag (#101). Back-to-back duplicates were enough — the
  documented staggered-duplicate hardening was NOT needed (deferred unless a future burst
  case defeats 1+1). Remaining Phase-2 piece is *video* over the lossy tunnel (the known
  HOL-block ceiling — separate, lower value); see the lossy-video deferral and the
  watchdog-timeout follow-up above.
- [x] ~~**Reed-Solomon FEC** (RDPEUDP v1 `UDPFECL`)~~ — **CLOSED, structural NO-GO.**
  FEC is a dead/legacy feature across the *whole* RDP ecosystem, not just a macrdp gap
  (2026-06-28 industry survey): the only stack that ever shipped FEC is Microsoft's
  RDP-8.x/RemoteFX-over-UDP server (v1 lossy); **Microsoft removed FEC in RDPEUDP2** and
  modern Windows (RDP 10+, AVD Shortpath) negotiates RDPUDP2 → retransmit-only, **never
  FEC** (confirmed by our own zero-FEC capture). FreeRDP deliberately skipped it
  (RDPUDP2-only). So **no current client would decode macrdp-emitted FEC** — building the
  encoder is pointless. The 1+1 redundancy stand-in (above) is the only loss-resilience
  lever reachable for a modern client. Would only reopen with legacy-Windows-8.x test
  machines, which isn't a realistic target. See the "Industry status" + "P2.3 FEC capture
  RESULT" notes in `docs/rdp-udp-multitransport-feasibility.md` + `vendor/ironrdp-rdpeudp/CLAUDE.md`.

- [ ] **Generic USB redirection (MS-RDPEUSB).**
  Viable path scoped: user-space virtual USB host controller via `IOUSBHostControllerInterface`/
  `AppleUSBUserHCI` (no kext/dext). **Blocked on Apple:** entitlement
  `com.apple.developer.usb.host-controller-interface` submitted 2026-06-24 (FB23363880),
  awaiting grant. Then: MS-RDPEUSB server-direction + UserHCI; gates to a signed build.
  See `docs/usb-redirection-feasibility.md`.

- [ ] **Multi-monitor (client-side multi-display).**
  Extend `--virtual-display` to N monitors. **Blocker:** the git-pinned `ironrdp-acceptor`
  hardcodes a single-monitor `MonitorLayoutPdu`, so true separate monitors needs an acceptor
  change. Open design question: true-monitors vs one-big-desktop vs phased. (PAUSED, no code.)
  Also listed as Tier 3 in the production-readiness roadmap below.

- [ ] **Production-readiness roadmap** (scoped 2026-06-29, in progress). What would lift
  macrdp from "polished v0 daily-driver for trusted LANs" to "reliable, secure, unattended
  single-session server for a LAN/VPN." Full prioritized plan in
  `docs/production-readiness-roadmap.md`. Recommended starting trio status: (1) real
  operator-supplied TLS certs (`--cert`/`--key`) **DONE 2026-06-30 #104**; (2) auth
  rate-limit + lockout + audit log (`src/auth_guard.rs`) **DONE 2026-06-30 #105**; (3) a
  48–72 h **soak to shake out leaks/drift — IN FLIGHT (Tier 2.4, running 2026-07-01; see
  the In flight section above + `scripts/soak-monitor.sh`)**. Also done:
  log rotation + startup reaper (Tier 2.5, #103). Still open beyond the soak: Tier 2.5
  hung-but-alive health-check/bounce; Tier 3 polish. Hard ceiling (NO-GO): multi-user
  concurrent GUI sessions (macOS limit).

## Parked — scoped, low priority

- [ ] **Auto-sized virtual display.** Resize the vdisplay to the client's resolution on
  connect (avoids the mirror-primary scaling lag). Risk: does `CGVirtualDisplay applySettings`
  live-resize cleanly or need recreate? Needs a real mstsc test. (No code.)

- [ ] **`cycle_apps` lock nesting (`CYCLE_SESSION` → `mru`).** Currently safe by consistent
  acquire order; de-nest as a follow-up to the PR #84 hardening if revisiting that area.

- [ ] **Auto-mute on silence (audio-only).** Long-idle YouTube unpause loses audio
  (Windows audiodg suspends after hours of digital silence). Must be audio-only (not the
  shared `display_suppressed` gate, which would freeze the desktop).

- [ ] **AVC444 (4:4:4 chroma).** YUV-pack module + bench scaffold landed; `--avc444` not
  wired. VT hw-encoder serializes → 1080p-comfortable, 4K doesn't fit. Resume only if
  colored-text quality becomes a pain.

## Not planned

- **Printer redirection (RDPDR printer device).** Not implemented; no current demand.

## Upstreaming watch (no action unless a release lands)

- IronRDP forks are effectively permanent (each carries un-upstreamed divergences:
  multitransport, rdpdr server-direction, smartcard, acceptor KLID+MT, audio-lag/resize/dispatch,
  server ARC auto-reconnect cookie). **#1359 (rdpsnd) + #1397 (acceptor keyboard-layout on
  `AcceptorResult`) MERGED 2026-07-01; #1373 (acceptor honor-size) MERGED 2026-07-02 (`d471bd06`).**
  **Two clintcan PRs currently OPEN (both CI-green, MERGEABLE, awaiting review — reactive-only, do
  not poll/nudge):**
  - [ ] **#1404** `feat(acceptor)!: clamp honored client desktop size to an operator maximum` — the
    honor-size resource-hardening CBenoit green-lit in his #1373 approval. Replaces the `bool` with
    `Option<DesktopSize>` = operator max; client request clamped per-dimension. Upstreams the
    hardened form of acceptor divergence (1).
  - [ ] **#1405** `feat(server): send the Server Auto-Reconnect Cookie during logon` — upstreams
    vendored `ironrdp-server` divergence (13). Additive (default `None`): optional ARC_SC cookie
    (MS-RDPBCGR 2.2.4.3) sent as a Save Session Info PDU once per connection, so a client
    auto-reconnects on an ungraceful drop (mstsc won't without it). API mirrors `credential_validator`
    (builder `with_auto_reconnect_cookie` + setter). PR body flags the one open design point:
    send-only, does NOT validate the returning ARC_CS cookie (offered as a follow-up).
  Nothing is currently de-vendorable. **Pin-bump follow-ups (when the git pin bumps past the merges):**
  (a) past `2d3bdef` → `src/audio.rs` adopts the new rdpsnd handler API (`choose_format` + fallible
  `start(&NegotiatedFormat)`), dropping the hand-rolled `wFormatNo` index logic; (b) past `d471bd06`
  → `main.rs` switches `set_honor_client_desktop_size(bool)` to the builder `with_honor_client_desktop_size`
  (and, once #1404 lands, to `Some(max)` capped to the Mac's native res — see the honor-size counterpart below).
- Upstream-ability of the remaining divergences was surveyed 2026-07-01 (don't re-survey; ranking
  in `project_upstream_ironrdp_open_prs` memory). The other "quick" items (RDPDR decode halves,
  AudioWave `duration_ms`, keyboard-layout handle) are **held** — no upstream consumer yet, so they
  belong with their larger feature (server-RDPDR processor, audio-lag model), not standalone PRs.
- [ ] **macrdp-side honor-size counterpart to #1404 (defense-in-depth, deferred).** macrdp sanity-bands
  the client-requested size to [200, 8192] but has no operator max, so the same 8192×8192 (~256 MB/frame)
  allocation vector exists locally. Add a `--max-client-size` flag (or cap to the Mac's native resolution)
  and, when the pin bumps past #1404, pass it through the new `Option<DesktopSize>` honor-size API. Not
  urgent; pairs with pin-bump follow-up (b) above.
