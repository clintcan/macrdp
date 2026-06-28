# TODO / work queue

A living checklist of what's open, deferred, or parked. Detail lives in the
linked docs / vendored `CLAUDE.md`s / commit history — this is just the index of
"what's currently to be made." Keep it pruned: move items to *Done* only briefly,
then delete; promote a parked item to *In flight* when work actually starts.

## In flight (needs an action)

- [ ] **EGFX-over-UDP reconnect blank/black — per-connection state reset** (branch
  `fix/udp-egfx-reconnect-state`; built + clippy/fmt green; **awaiting real-mstsc
  verification before merge**). With `--udp-migrate-egfx`, 1st connection rendered but
  reconnect went blank→black; plain-TCP EGFX reconnect was always fine (UDP-specific).
  Two only-set-never-reset bugs on the persistent server+listener: (a) server
  `egfx_on_udp` (+ lossy-audio counters + `egfx_on_lossy_handle`) stayed true → conn 2
  routed EGFX over an unbound UDP tunnel → frames dropped → blank (fix: reset in the
  `run()` accept loop); (b) listener reused a stale established `Peer` on a same-port
  reconnect → new tunnel never bound, acks dropped (fix: replace the peer when a SYN
  arrives on an already-established addr). **Verified on real mstsc** — multi-cycle
  reconnect now renders and stays responsive. Follows the merged idle-GC (#87). See
  feasibility doc "M3c reconnect state-reset".

## Deferred — scoped, not started

- [ ] **Congestion-responsive encoder rate control + frame dropping** (highest-value
  video-under-loss work — helps BOTH the default TCP path and UDP). **Concrete
  manifestation found 2026-06-28:** EGFX-over-UDP reconnect freezes *intermittently*
  because the server never throttles on the client's EGFX `queueDepth` —
  `GfxHandler::on_frame_ack` only records ack timing, so macrdp ships at full rate while
  the client's queue runs away (observed peak ~352k) → frozen display + RDPEUDP ACK
  storm. A focused **queue_depth-aware throttle / frame-drop** (a sub-piece of this item)
  is the targeted fix; note raw `queueDepth` is oddly large even when healthy (~30k–82k),
  so capture a real-Windows-server baseline before picking a threshold. Today macrdp ships
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
- [ ] **EGFX-over-UDP auto-fallback to TCP on tunnel abandonment** (secondary safety net,
  below rate control). When the reliable tunnel is abandoned (window pegged / client
  stopped acking), re-route EGFX to TCP + force an IDR instead of staying frozen. Open
  risk: in-session fallback would be *frozen→recover* (NOT the reconnect-blank quirk —
  that needs a new connection), but only if mstsc accepts the channel's data back on TCP
  after Soft-Sync (no standard reverse Soft-Sync; untested). Spike against real mstsc first.
- [ ] **UDP multitransport Phase 2 — lossy `UdpFecL` + DTLS + 1+1 redundancy** (FEC dropped).
  Phase 1 (reliable EGFX-over-UDP) shipped (v0.8.15, clean-link only). Phase 2 wants
  loss resilience. Status: lossy delivery mode + 1+1 duplicate-send (repetition code)
  exist in `ironrdp-rdpeudp` behind `MACRDP_UDP_LOSSY_AUDIO_DUP`; DTLS via `boring`
  not wired. Remaining: wire DTLS, soak the 1+1 lossy-audio path under loss.
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
  multitransport, rdpdr server-direction, smartcard, acceptor KLID+MT, audio-lag/resize/dispatch).
  Two PRs still open upstream: **#1359** (rdpsnd) and **#1373** (acceptor honor-size).
  Nothing is currently de-vendorable. See `project_upstream_ironrdp_open_prs` memory.
