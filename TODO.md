# TODO / work queue

A living checklist of what's open, deferred, or parked. Detail lives in the
linked docs / vendored `CLAUDE.md`s / commit history — this is just the index of
"what's currently to be made." Keep it pruned: move items to *Done* only briefly,
then delete; promote a parked item to *In flight* when work actually starts.

## In flight (needs an action)

- [ ] **Tier 2.4 — multi-day soak (foundation core PASSED 31 h 2026-07-03; full 48–72 h on
  v0.8.24 still needed).** The last leg of the production-readiness trio (TLS ✓ #104, auth ✓
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
  **ARC cookie** not exercised (and now also the **v0.8.23 health-check watchdog**). **Next
  action:** 48–72 h re-soak on a **build from main ≥ d8ecec9** (v0.8.25 + the post-tag #136/#137/#138 link-aware + tunnel-death hardening; running on a separate Mac since 2026-07-06), biased toward reconnect
  cycles. Logging fixes for that run **DONE 2026-07-03 (#124):** the soak logger now `sync`s
  after every CSV append (can't zero-fill on transfer), and the `multitransport`/`audio_dvc`
  "GREEN" status lines are demoted WARN→DEBUG. See `docs/production-readiness-roadmap.md` Tier 2.4.

## Deferred — scoped, not started

- [~] **Perf: eliminate the per-capture full-frame `last_frame` memcpy — ASSESSED 2026-07-07,
  DEPRIORITIZED (don't re-propose as a win).** From the 2026-07-03 audit's one HIGH finding:
  `capture.rs` copies the whole BGRA frame into the flush-burst stash on every EGFX-accepted
  capture (~8 MB @1080p / ~24 MB @HiDPI × 60 fps ≈ 1.4 GB/s), read only when SCK goes idle.
  Fix shape (retain the SCK `CVPixelBuffer`, release the prior, re-lock during the flush burst)
  is FEASIBLE and refcount-correct — confirmed `CMSampleBuffer.image_buffer()` returns an
  independent retained ref (screencapturekit 2.1 Swift shim `Unmanaged.passRetained`),
  `CVPixelBuffer` is `Send+Sync` with `lock()`/`Drop`. **But the risk/reward doesn't justify it:**
  (1) reward is marginal — 1.4 GB/s is <1% of Apple-Silicon bandwidth; it removes ~200 µs
  (1080p) to ~600 µs (HiDPI) of capture-thread memcpy/frame (~1–4% of the 60 fps budget), and
  the capture thread isn't the bottleneck; (2) its sibling (CVPixelBufferPool for encoder input,
  [[project_cvpixelbufferpool_no_win_reverted]]) A/B'd at PARITY and was reverted — same
  fill-bound path, likely the same result here; (3) real risk — holding the retained buffer pins
  one IOSurface from SCK's recycle pool (macrdp doesn't set `queueDepth`, Apple default 3), the
  "blank after N frames" starvation class, mitigable only by a `queueDepth` bump (+~24 MB/slot);
  (4) it's a video hot-path (`next_update`) change, which project guidance says not to touch
  without a verified payoff. Revisit only if a HiDPI-under-contention smoothness problem is
  actually observed and traced to this copy. **Do NOT re-list as a "clean win."**
- [ ] **Perf (upstream candidates, vendored server — do NOT land as new divergences):** from
  the same audit: (a) `SharedWriter`/dispatch write coalescing — every fragment/event is its
  own `write_all` = 2 boxed futures + syscall + flush (`server.rs:2643` + git-pinned
  `ironrdp-tokio`), dozens–hundreds per EGFX batch; coalescing a batch into one write is a
  med-high win under load but touches the most sensitive path → propose upstream. (b) legacy
  bitmap encoder: fresh `vec![0; len*2]` per diff-rect + one `spawn_blocking` round-trip per
  rect (`encoder/mod.rs:520,337`) — reuse a scratch buffer + one `spawn_blocking` per update;
  legacy-clients-only, clean upstream PRs. NOT worth doing: RDPEUDP per-datagram allocs
  (inherent to sans-I/O design), NFS readdir pagination cache, UDP idle tick suppression.
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

- [ ] **Generic USB redirection (MS-RDPEUSB) — FreeRDP: DRIVE MOUNTS ✅✅ (Phase 3.2 bulk). mstsc: ENUMERATES + CONFIGURES + negotiates FORMAT ✅ (2026-07-07); only gap = client doesn't deliver bulk frames (mstsc-side). Remaining: camera-redirection channel, device-class streaming (isoch/interrupt), retract/multi-device.**
  **mstsc now enumerates, configures, and negotiates format end-to-end** (verified camera `09da:2692`
  + audio/HID `0573:1573`; FreeRDP mass storage regression-verified — mounts + read/write). Fixes,
  all FreeRDP-safe: (handshake) per-device channel needs the FULL caps→CHANNEL_CREATED→RIMCALL_RELEASE,
  accept `UsbDevice=0`, route interface-0 completions by function id; (SelectConfiguration) one
  interface-info per interface NUMBER at alt 0 (not per alt-setting → dup interface numbers) + carry
  the FULL config descriptor (`ironrdp-rdpeusb` div 3), was `0x80070057`; (control) map each SETUP to
  the TYPED URB real Windows uses (`CLASS_INTERFACE`/`GET_DESCRIPTOR_FROM_INTERFACE`/…) not the generic
  `CONTROL_TRANSFER_EX` mstsc rejects — 135+ transfers succeed, UVC probe/commit completes; (bulk)
  `USBD_SHORT_TRANSFER_OK`; `RIMCALL_RELEASE` recognized+ignored.
  **Remaining gap (client/mstsc-side, NOT a server bug):** a camera enumerates + negotiates format but
  mstsc never returns bulk video frames — webcams stream over mstsc's **separate "Video capture devices"
  camera-redirection channel** (a different high-level protocol macrdp doesn't implement). True webcam
  support = implement that channel (separate feature). isoch (camera/audio) + interrupt (HID) endpoints
  also unimplemented. mstsc's RemoteFX list EXCLUDES mass storage (rides Drives/RDPDR), so the verified
  bulk path can't be exercised from mstsc. Detail: [[project_usb_redirection_feasibility]].
  Path: user-space virtual USB host controller via `IOUSBHostControllerInterface` (a **public,
  headered** IOUSBHost.framework API — NOT private SPI as first assumed; its doc says it
  "create[s] synthetic USB devices"). Entitlement `com.apple.developer.usb.host-controller-
  interface` **GRANTED** to QGLA89KHM7 (FB23363880). All on branch `feat/usb-redirect-spike`.
  - **Phase 1 GO** (2026-07-01, `--usb-spike`): entitled signed+provisioned build creates the
    controller and the kernel begins the command exchange → the route works.
  - **Phase 2 GO** (2026-07-06, commit `ab91a63`): `usb_spike.m` drives the full UserHCI
    command/doorbell loop and a **hardcoded synthetic device enumerates LIVE in `ioreg`**
    (VID 0x1209/PID 0x0001, full EP0 GET_DESCRIPTOR flow, clean teardown) — the whole macOS
    presenting path proven.
  - **Phase 3.0 GO** (2026-07-06, commit `3a435c9`): URBDRC server-direction DVC observe-only
    spike GREEN — `--enable-usb-redirection` advertises URBDRC + runs the MS-RDPEUSB capability
    exchange; **verified locally with a plain `cargo build`** (observe-only never touches the
    UserHCI controller, so no entitled build needed) via `sdl-freerdp /usb:auto` → channel
    opens (Create status 0) + caps exchange completes (S_OK). No `AddDevice` only because this
    Mac has no attachable USB device to redirect. Vendored ironrdp-server **divergence 16**
    (`src/rdpeusb.rs`, `UrbdrcServer` + `UrbdrcServerFactory`); `ironrdp-rdpeusb` added as a
    git dep (PDU-only, pinned rev).
  - **Phase 3.1a GO** (2026-07-06, commit `38a360f`): the server opens a **per-device DVC** on
    the client's `ADD_VIRTUAL_CHANNEL` (via `ServerEvent::Urbdrc(OpenDeviceChannel)` → the
    `client_loop` dispatch arm → `DrdynvcServer::create_channel(UrbdrcDeviceProcessor)`), which
    sends `RIMCALL_RELEASE` on the new channel so the client sends `ADD_DEVICE` with the real
    descriptors. **Verified live** with a USB-3 flash drive (full handshake → per-device channel
    → `ADD_DEVICE` = GO, session stays up). Both `process()` impls now **tolerate decode errors**
    (never tear down the session) — the pinned `ironrdp-rdpeusb` `SupportedUsbVer` enum stops at
    USB 2.0 and rejects the SSD's USB-3.2 `0x320` caps; adversarially code-reviewed clean.
  - **Phase 3.1b(1) GO** (2026-07-06, commit `357624f`): the client's `ADD_DEVICE` now **fully
    parses** (real descriptors, `usb_version=Usb32`), not just header-recognized. **Vendored
    `ironrdp-rdpeusb`** (leaf crate → clean one-sided path-dep; `ironrdp-str` pinned via
    `[patch.crates-io]`) with a **lenient `UsbDeviceCaps` decode**: `SupportedUsbVer`/`UsbdiVer`/
    `UsbBusIfaceVer`/`DeviceSpeed` are data-carrying enums with the named values + an `Other(u32)`
    fallback (named `Usb30/31/32` added), so a modern USB-3 device's `0x320` caps parse instead of
    erroring. Verified live with a USB-3.2 flash drive. New vendored crate divergence.
  - **Phase 3.1b(2a) GO** (2026-07-06, commit `5ef8e4b`): a server-initiated **`GET_DESCRIPTOR`
    control transfer round-trips REAL device data** — proven observe-only (plain `cargo build`).
    On `ADD_DEVICE` the device processor reactively sends `RegisterRequestCallback` +
    `TransferInRequest(GetDescriptorFromDevice)` and decodes the `URB_COMPLETION`. Verified live:
    `hresult=0x0 descriptor_len=18 vid=0x2174 pid=0x2100` — the flash drive's real VID/PID read
    from the physical device. macOS libusb kernel-detach was not a blocker after unmount. This
    de-risks the whole transfer path.
  - **Phase 3.1b(2b) part 1 GO** (2026-07-06, commit `e79fa85`): the transfer path is now a
    reusable **async `UsbHandle`/`UsbRouter`** seam (the RdpdrHandle pattern: 31-bit req-id router +
    `ServerEvent::Urbdrc(SendMessages)` DVC-framed dispatch + `DeviceDescriptor::parse`), and
    `UrbdrcDeviceProcessor::process()` shrank to decode-and-route. The descriptor fetch is driven
    through the handle. Verified live: `vid=0x2174 pid=0x2100 usb_version=0x0320`. (Elegance
    refactor of the 2a spike.)
  - **Phase 3.1b(2b) part 2-i GO** (2026-07-06, commit `c464df0`): **device-callback seam** — the
    vendored server exposes the `UsbHandle` via `UrbdrcServerFactory::device_callback()`; the
    per-device processor invokes it on `ADD_DEVICE`, and the transfer **driver moved into macrdp**
    (`src/usb_redirect/mod.rs::drive_device`). `UrbdrcDeviceProcessor::process()` is now purely
    decode-and-route. Verified live: macrdp's driver fetched `vid=0x2174 pid=0x2100`. (Elegance +
    the exact hook the UserHCI integration needs.)
  - **Phase 3.1b(2b) part 2-ii GO ✅✅ — the Phase-3.1 milestone: a REAL client device enumerates
    locally** (2026-07-06, commits `c1f27e9` async boundary + `0ffc6ce` presenting side). 2-ii-a
    restructured `usb_spike.m` to async out-of-band EP0 completion (raise via C callback → leave
    outstanding → `macrdp_usb_complete_control_in` `dispatch_async`es onto `self.interface.queue`),
    with a bidirectional C ABI; `--usb-spike` still enumerates the synthetic device through it.
    2-ii-b wired `imp::present_device`: create the controller with a Rust control-IN callback that
    services each EP0 `GET_DESCRIPTOR` by awaiting `handle.get_descriptor()` (client-sourced over
    URBDRC). **Verified entitled + FreeRDP `/usb` (flash drive): ESD310C, idVendor=0x2174 enumerates
    on macrdp's UserHCI controller** (ioreg `@80100000`), device + string descriptors (product
    "ESD310C") sourced live from the client. Non-entitled path degrades gracefully.
  - **Phase 3.2 STARTED — teardown/lifecycle first** (2026-07-06, commit `3c85845`): the UserHCI
    controller is now destroyed on disconnect instead of leaking until process exit.
    `UrbdrcDeviceProcessor` holds a `watch::Sender`; each `UsbHandle` carries a subscriber +
    `closed()`; the server's `static_channels` reset (`server.rs:1244`) drops the processor on
    disconnect → `closed()` resolves → `present_device` breaks + destroys the controller. Verified
    entitled + FreeRDP: the presented ESD310C disappears from `ioreg` on disconnect (server stays up).
  - **Phase 3.2 bulk — SelectConfiguration done + a test-environment blocker found** (2026-07-06,
    commit `ed735d5`): implemented the config-descriptor parse + `SelectConfiguration` URB (opens the
    device's pipe handles — the prerequisite for bulk). **The URB is verified correct** (FreeRDP
    parses + attempts it), **but the macOS-client loopback CANNOT complete bulk**: FreeRDP's libusb
    can't detach the mass-storage kernel driver to claim the interface (`LIBUSB_ERROR_ACCESS`), which
    every bulk transfer needs. EP0 descriptor reads work; bulk needs a **claimable-interface client**
    (a real Windows client, or a **Linux FreeRDP** client where kernel-detach works — i.e. a second
    machine). Degrades to enumerate-only (5s timeout).
  - **Phase 3.2 dedup — done, then hardened** (2026-07-06, commits `e11eed3` then the bulk commit):
    one presenting controller per physical device. Initially keyed on the client's
    `device_instance_id`, but that FAILED live — FreeRDP announces one drive twice with instance ids
    differing by a byte (`…d31`/`…d32`), so both presented and the two virtual drives **dueled over
    the single client device** (10 s SCSI timeouts, failed mount). Re-keyed on the device's **stable
    hardware identity** (`VID:PID:bcdDevice`, fetched before claiming). Unit-tested; verified live
    (one controller, clean mount). (`UsbHandle` still exposes `device_instance_id` for logging.)
  - **Phase 3.2 bulk IN/OUT forwarding GO ✅✅ — the redirected DRIVE MOUNTS** (2026-07-06): 
    `UsbHandle::bulk_transfer_in/out` (`TsUrb::BulkInterruptTransfer` on the SelectConfiguration pipe
    handles, direction-flag matched) + the Obj-C ring walk generalized to the bulk endpoints (async
    out-of-band completion, same pattern as EP0 control-IN). **Verified end-to-end on a real Linux
    FreeRDP client** (UTM-QEMU Ubuntu 24.04 ARM64 + a **USB-2.0 hub** to force high-speed so the
    USB-3.2 SSD enumerates in QEMU; guest `udev MODE=0666` + `xfreerdp /usb:dbg,id:2174:2100`): the
    ESD310C **mounts on the Mac and stays mounted**, 1300+ steady bulk transfers, no resets/timeouts.
    Two load-bearing fixes: (1) the hardware-identity dedup above; (2) an Obj-C **endpoint-object
    identity guard** on completion — a device reset destroys+recreates the endpoint at the same key
    (new Active object, but `p.msg` points into the old freed ring), so the completion is dropped
    unless `endpoints[key]` is still the same object it was raised on (fixes a reset-during-bulk
    SIGSEGV the liveness check alone missed).
  - **Phase 3.2 control-OUT forwarding — done** (2026-07-06): EP0 host→device requests the local
    kernel issues (mass-storage Bulk-Only Reset `bReq=0xff` / Clear-Feature(HALT)) now forward to the
    real device via `UsbHandle::control_transfer_out` (generic `URB_FUNCTION_CONTROL_TRANSFER_EX`,
    pipe 0 = default control EP); the standard requests the host controller / SelectConfiguration own
    (SET_ADDRESS/CONFIGURATION/INTERFACE) stay a local ACK. Obj-C forwards on the control STATUS stage
    (no-data requests are SETUP→STATUS) and stashes any DATA-OUT payload. **Regression-verified live**
    (clean mount + file copy + remove/reattach unaffected; the one SET_CONFIGURATION seen was correctly
    NOT forwarded). The forward path itself only fires under a SCSI error/stall, which didn't occur, so
    it's implemented + regression-safe but not yet observed firing.
  - **Phase 3.2 remaining** — generic control-IN forwarding (currently GET_DESCRIPTOR-only; a class/
    vendor control-IN like Get Max LUN is stalled → assumed 1 LUN, wrong for a multi-LUN device),
    mid-session retract/hot-unplug, true multi-device (needs iSerialNumber to distinguish identical
    models), dispatch-priority tier. Test rig proven: UTM-QEMU Linux FreeRDP + USB-2.0 hub. Plan:
    `~/.claude/plans/wobbly-honking-minsky.md` §3.2.
  Gates to the official signed+provisioned build for the *presenting* side (entitlement baked
  into the signature). See `docs/usb-redirection-feasibility.md` +
  [[project_usb_redirection_feasibility]].

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
  log rotation + startup reaper (Tier 2.5, #103) and the **hung-but-alive health-check
  watchdog (`src/health.rs`) — DONE 2026-07-03** (probes the tokio runtime from a
  dedicated OS thread; exits code 70 on a sustained wedge → launchd/supervisor restarts;
  on by default when headless, env-tunable). **Tier 2.5 now complete.** Tier 2.6
  (`--fork-workers` as default) **DECIDED 2026-07-04: NO** — single-process +
  blank-recovery + ARC stays the default (field-proven, composes with everything);
  fork-workers stays the documented opt-in for mstsc-heavy no-UDP profiles (see the
  roadmap for the full rationale). **Reinforced 2026-07-07 (v0.8.27):** the
  reconnect-blank now self-heals in place via core reactivation, so fork-workers'
  reconnect-freshness rationale is moot — single-process is the clear default. Still open beyond the soak: Tier 3 polish. Hard
  ceiling (NO-GO): multi-user concurrent GUI sessions (macOS limit).

## Parked — scoped, low priority

- [ ] **Crash-report watch (post NSPasteboard-mutex fix).** The rare churn-time
  NSPasteboard use-after-free SIGSEGV was fixed 2026-07-07 via a process-global
  pasteboard mutex (`clipboard::pasteboard_guard()`, released in v0.8.29) — but it was
  rare, so it's unproven-by-repro. Keep the `.ips` files and watch new crash reports for
  a DIFFERENT signature. The 2026-06-28 `UNKNOWN_0x32` pthread crash is a separate,
  still-open one-off.

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
  **Three clintcan PRs currently OPEN (all MERGEABLE, awaiting review — reactive-only, do
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
  - [ ] **#1418** `feat(rdpeusb)!: tolerate unrecognized device-reported USB capability values` —
    opened 2026-07-08. Upstreams vendored `ironrdp-rdpeusb` divergence (1) (the lenient USB-caps
    decode): the 4 device-reported enums become data-carrying with `Other(u32)` + named
    Usb30/31/32; framing consts (CbSize/HcdCapabilities) stay strict. Fixes a real USB-3.2 device
    (SupportedUsbVersion `0x320`) tearing down the URBDRC channel on decode. QA'd for regression
    (byte-identical encode; blast radius = rdpeusb + testsuite only). Scoped to (1) only — HELD
    rdpeusb (2) `UsbDevice=0` (CONFLICTS with merged #1321, which deliberately rejects that range;
    needs a "mstsc really sends 0 + capture" argument) and (3) full config descriptor. On
    merge+release, most of rdpeusb divergence (1) drops.
  - [ ] **rdpeusb (3) full config descriptor — DRAFTED 2026-07-08, not yet filed.** Branch
    `feat/rdpeusb-full-config-descriptor` (commit `60e50232`) in the IronRDP clone: `UsbConfigDesc`
    gains `trailing: Vec<u8>` (bytes 9..wTotalLength) so `TS_URB_SELECT_CONFIGURATION` carries the
    full configuration descriptor (real Windows rejects header-only with `0x80070057`). QA'd
    ship-ready (decode clamp is safe by construction — the URB decodes from a length-delimited
    sub-cursor). **File after #1418 gets its first review** (same young crate); flag the one design
    point in the PR body: `total_length` not auto-derived from `trailing` — offer an
    invariant-enforcing constructor as an option. Both rdpeusb PRs touch `tests/rdpeusb/mod.rs`
    (one line) — rebase whichever merges second.
  Nothing whole-vendor-dir is currently de-vendorable (each fork keeps ≥1 macrdp-specific
  server-direction divergence). **Divergence logs reconciled vs upstream/master 2026-07-08**
  (de-drift committed `27c5a84`): six divergences are already merged upstream and become deletions
  on the next pin bump — dvc (2) close-hook (#1302), acceptor (1)/(2) + server (9)
  honor-size/keyboard-layout (#1373/#1397), server SuppressOutput (#1319), NSCodec (#1332), QOI
  (#1335/#1341). Full ranked list: `project_upstream_ironrdp_open_prs` memory.
  **Pin-bump follow-ups (when the git pin bumps past the merges):**
  (a) past `2d3bdef` → `src/audio.rs` adopts the new rdpsnd handler API (`choose_format` + fallible
  `start(&NegotiatedFormat)`), dropping the hand-rolled `wFormatNo` index logic; (b) past `d471bd06`
  → `main.rs` switches `set_honor_client_desktop_size(bool)` to the builder `with_honor_client_desktop_size`
  (and, once #1404 lands, re-route the shipped `--max-client-size` clamp — currently a local acceptor/server divergence extension, 2026-07-09 — through the upstream `Option<DesktopSize>` honor-size API).
- [ ] **THE PIN BUMP — scoped 2026-07-08, harvest-triggered, DECIDED: hold for now (do NOT bump
  opportunistically).** Current pin `879ffed` (2026-05-25, ~6 wk stale); a bump is all-or-nothing
  (15 git pins + all 6 vendor forks are version-coupled; breaking `core 0.1→0.2` / `pdu 0.7→0.8` /
  `dvc 0.5→0.7` / `server 0.10→0.12`) and churns every vendored crate, so it runs as its OWN
  dedicated effort + release, never a side task.
  **Trigger (whichever first):** (i) the small-PR wave merges — the rdpeusb pair (#1418 + the
  drafted config-descriptor) and #1405 (+#1415/#1404 if they land) — maximizing the harvest to
  ~11–12 divergence deletions in ONE migration instead of two; or (ii) a **~6-week staleness cap
  (early Aug 2026)** — upstream is refactoring code our divergences sit on (e.g. #1407 restructured
  rdpeusb), so waiting past the cap makes the re-vendor diff hairier; bump anyway if reviews stall.
  **Precondition:** the Tier 2.4 48–72 h soak has signed off the current baseline (don't churn the
  base mid-soak / before sign-off).
  **Execution checklist:** dedicated branch → re-copy the 6 forks at the new rev → DELETE the
  upstreamed divergences (the six reconciled 2026-07-08 + whatever the trigger wave adds) →
  re-apply the ~14 surviving divergences onto the refactored upstream code → adopt new APIs:
  follow-ups (a)+(b) above, plus EVALUATE upstream's new `autodetect_rtt` (builder,
  `Option<Arc<AtomicU32>>`) as a replacement for server divergence (15) RTT-cell → full gates
  (fmt/clippy/tests both OSes) → **live re-verification on real mstsc + FreeRDP** (H.264, audio,
  clipboard, RDPDR, blank-recovery, USB if entitled) → ship as its own release with nothing else
  in it. **Watch items:** issue #1352 (pdu spec-line split would rename macrdp's direct
  `ironrdp-pdu` dep) and egfx breaking changes. Est. 1–2 focused days + verification.
- Upstream-ability of the remaining divergences was surveyed 2026-07-01 (don't re-survey; ranking
  in `project_upstream_ironrdp_open_prs` memory). The other "quick" items (RDPDR decode halves,
  AudioWave `duration_ms`, keyboard-layout handle) are **held** — no upstream consumer yet, so they
  belong with their larger feature (server-RDPDR processor, audio-lag model), not standalone PRs.
