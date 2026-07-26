# TODO / work queue

A living checklist of what's open, deferred, or parked. Detail lives in the
linked docs / vendored `CLAUDE.md`s / commit history — this is just the index of
"what's currently to be made." Keep it pruned: move items to *Done* only briefly,
then delete; promote a parked item to *In flight* when work actually starts.

## In flight (needs an action)

- [x] **SIEM/SOC audit forwarding — Tier 0 (structured JSON audit stream) — SHIPPED (v0.8.33);
  `event="auth"` (div18) + `event="fingerprint"` which-client (PR #163, 2026-07-18, merged, awaits
  next release) added since.** macrdp's `macrdp::audit`
  events (connection accept/reject/disconnect, source IP+port, reason, outcome) are
  additionally written as **one JSON object per line** to a dedicated self-rotating file
  (`--audit-file` / `AUDIT_FILE` / `MACRDP_AUDIT_JSON=1`) for a log collector (Vector /
  Fluent Bit / rsyslog / Splunk UF) to tail → SIEM. macrdp deliberately does **not** speak
  network syslog (the collector owns TLS/buffering; macOS has no `syslogd` — unified logging
  replaced it). **Opt-in, default OFF, byte-identical when off.** Two tracing layers: the
  audit JSON layer's `Targets` filter is **RUST_LOG-independent** (a quiet operational filter
  can't suppress security events); main layer keeps `EnvFilter`. Schema v1 (`AUDIT_SCHEMA_VERSION`
  const, versioned contract; (src_ip,src_port) correlation, conn_id deferred additive). 170
  tests pass incl. a new RUST_LOG-independence test; clippy/fmt clean; **end-to-end `nc` smoke
  produced a correlated accept+disconnect JSON pair.** Files: Cargo.toml (json feat),
  src/{logging,auth_guard,main}.rs, packaging/config.env.example, docs/{cli,configuration}.md,
  NEW docs/siem-forwarding.md (schema + collector configs). Tiers 1 (collector configs) shipped
  as docs; **Tier 2 (native RFC5424/CEF-over-TLS emitter) deferred** — only if a deployment
  can't run a collector. Follow-on (additive, no schema bump): promote cert-expiry /
  health-bounce / active-redirection-features / session-start onto `macrdp::audit`. Plan:
  `~/.claude/plans/tier0-siem-audit-json.md`; [[project_siem_audit_json_tier0]].

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

- [ ] **Microphone / audio-input redirection (MS-RDPEAI, the `AUDIO_INPUT` DVC) — present the CLIENT's mic as a macOS input device.**
  Scoped 2026-07-27, prompted by the A4Tech FHD webcam (`09da:2692`) having a built-in mic:
  none of the three existing channels can carry it — **USB redirection** can't (USB audio streams over
  **isochronous** endpoints; macrdp's URBDRC path is bulk+interrupt only — the isoch gap; this cam's
  *video* only works because it's an unusual **bulk** UVC device), **camera redirection (MS-RDPECAM)**
  is **video-only**, and macrdp's audio today is **output-only** (RDPSND, Mac→client; `src/audio.rs`).
  The RDP-native answer is **MS-RDPEAI** — a **separate** DVC that captures the client's *default
  recording device at the OS audio layer* (so it sidesteps isoch-USB entirely: the client OS already
  drives the webcam mic and just forwards PCM). Together with camera redirection (video, shipped v0.9.0)
  this gives a **full remote webcam + mic**; they're independent channels. **Goal:** with
  `--enable-microphone-redirection` (opt-in, default OFF, byte-identical when off), the connecting
  client redirects its mic and **"macrdp Microphone"** appears as a real macOS *input* device that
  QuickTime / Zoom / Voice Memos / dictation can record from.
  - **Protocol side (server-direction MS-RDPEAI).** Dynamic virtual channel **`AUDIO_INPUT`** over
    DRDYNVC. macrdp is the **receiver/sink** (the client's mic is the source). Handshake mirrors RDPSND
    but reversed: `MSG_SNDIN_VERSION` (both ways) → `MSG_SNDIN_FORMATS` (server advertises the formats it
    accepts; client replies its subset) → `MSG_SNDIN_OPEN` / `MSG_SNDIN_OPEN_REPLY` (pick a format, open
    the recording) → client streams `MSG_SNDIN_DATA_INCOMING` + `MSG_SNDIN_DATA` (the PCM), with
    `MSG_SNDIN_FORMATCHANGE` as needed. **ironrdp has no server-side RDPEAI** (it has `ironrdp-rdpsnd`
    = output only), so this is new PDU work — a new vendored crate or an `ironrdp-server` module, wired
    through a **new factory seam** (`audin_factory`, mirroring the existing `sound_factory` / `gfx_factory`
    / `camera_factory` `Option<Box<dyn …>>` pattern — zero-overhead/no-op when the flag is off). Start with
    **PCM** (like the RDPSND default), add the client's compressed format later.
  - **macOS side (the hard, novel part — a virtual INPUT device).** There is **no self-serviceable CoreMediaIO
    equivalent for audio**, so the camera playbook does NOT copy 1:1. Two routes: **(A) `AudioServerPlugIn`**
    — a userspace Core Audio HAL plug-in (`.driver` bundle in `/Library/Audio/Plug-Ins/HAL/`, loaded by
    `coreaudiod`), **needs NO entitlement** but installs into a system dir (one privileged admin step — the
    same install pattern as the `ifd-handler`); reference impl is **BlackHole (GPL)** → write from scratch
    MIT/Apache like the IFD handler. **(B) `AudioDriverKit` System Extension** — modern, ships in the
    controller `.app` and activates like the camera extension, BUT needs the **restricted DriverKit
    entitlement** (Apple-granted, not self-serviceable — heavier than the camera's CMIO entitlement).
    **Recommend route A** (no entitlement; deployment cost is a privileged install, not an Apple grant).
    macrdp feeds received PCM into the plug-in via a **shared-memory ring** (the audio analogue of the CMIO
    sink stream), with **clock/drift handling** — the client mic clock vs the Mac audio clock will drift, so
    resample like `audio.rs` already does 48→44.1 (`rubato`), plus a small jitter buffer (RDPEAI over TCP).
  - **Phasing (mirror the camera feature):** **P0** — the `AUDIO_INPUT` handshake behind the flag, inert:
    negotiate + log the client streaming its mic, drop the audio (proves mstsc/FreeRDP will hand macrdp a mic;
    the camera Phase-0 gate is the template). **P1** — accept the Data PDUs, produce a PCM stream, dump to
    WAV under `MACRDP_MIC_DUMP=1` (like `MACRDP_CAMERA_DUMP`) to verify content. **P2** — the `AudioServerPlugIn`
    virtual mic + the shared-memory feed → present "macrdp Microphone". **P3** — format/clock robustness,
    disconnect cleanup, silence/mute handling.
  - **Module placement:** new `src/audio_input/` (`mod.rs` = the `AUDIO_INPUT` DVC backend + factory/policy,
    `feed.rs` = the shared-memory producer into the HAL plug-in), mirroring `src/camera/`. The plug-in bundle:
    `gui/Sources/macrdpmic` (an `AudioServerPlugIn` `.driver`) + a `packaging/make-audio-plugin.sh` +
    `install-audio-plugin.sh` (privileged, like `install-ifd-handler.sh`).
  - **Client + verification:** **FreeRDP `/microphone`** (the `audin` channel) first — easiest to script,
    like the USB/camera FreeRDP-first bring-up; then **mstsc** "Remote audio → Settings → Record → *Record from
    this computer*" for the real-Windows path. Acceptance: select the **A4Tech webcam mic** as the client's
    default recording device → it shows as **"macrdp Microphone"** on the Mac → record in QuickTime → hear it.
  - **Effort:** comparable to the camera feature (medium–large: new protocol channel + new macOS virtual
    device + install/signing). The **`AudioServerPlugIn` + shared-memory feed + clock-drift** piece is the
    novel/risky part; the protocol side is well-trodden (RDPSND in reverse). **Biggest unknowns to resolve
    first:** confirm mstsc/FreeRDP actually stream the mic to a server-direction `AUDIO_INPUT` DVC (P0 GO/NO-GO),
    and that a from-scratch `AudioServerPlugIn` fed from an external process works without licensing the
    BlackHole source. Natural next capability after the camera work — a full webcam **and** mic remote setup.

- [x] **Camera redirection Phases 1–3 — SHIPPED in v0.9.0 (2026-07-20). The client's webcam IS a macOS camera.**
  Phase 0 protocol gate is LANDED + LIVE-VERIFIED GREEN (v0.8.39-era, f2d54e5: vendored server
  divergence (19) `rdcamera.rs` + `camera_factory` seam + `src/camera/MacCamera` +
  `--enable-camera-redirection`; real mstsc SelectVersion v2 → DEVICE_ADDED over MS-RDPECAM —
  proving a modern mstsc/Win11 hands us the webcam over the RDCamera DVC, the channel USB
  redirection can't reach). **Remaining = the real presentation work, ~multi-week:** (1) open the
  per-device stream channel; (2) media-type negotiation (the client offers H.264/MJPEG/uncompressed
  formats); (3) VideoToolbox H.264 decode of the incoming samples; (4) surface it as a macOS
  camera via a **CoreMediaIO Camera Extension** (self-serviceable entitlement; VT-decoded frames
  → CMIO). This is the standout next capability — gives mstsc webcam support the raw-USB path
  can't (mstsc refuses macrdp's bulk-video reads with 0x8007001f and routes real video over
  MS-RDPECAM instead). **Phase 1 DONE — LIVE-VERIFIED GREEN 2026-07-20** (on branch
  `feat/camera-redirection-phase1`, not yet merged to main): real mstsc streamed a redirected
  A4Tech webcam as **H.264 1080p over plain TCP DRDYNVC** (350+ frames, steady ~20 fps,
  ~18 KB/frame) — the full MS-RDPECAM server state machine (enumerate → open the client-named
  per-device channel → ActivateDevice → StreamList → media-type negotiation picking H.264 →
  StartStreams → the SampleRequest↔SampleResponse pull loop), mirroring FreeRDP's `rdpecam`
  server + the URBDRC per-device model (divergence 16). **GO/NO-GO answered GREEN: samples flow
  over TCP, so UDP (Phase 4) is NOT a prerequisite.** First known OSS RDP *server* to receive a
  webcam over MS-RDPECAM. **Phase 2 DONE — LIVE-VERIFIED GREEN 2026-07-20** (same branch):
  VideoToolbox decodes the webcam end-to-end — 500+ frames, zero errors, real color
  `CVPixelBuffer` at 1080p (the decoded grayscale-Y dump is the user's real camera view;
  color CVPixelBuffer, luma-only PNG). Both technical unknowns now GREEN (protocol over TCP +
  VT decode). **Phase 3 COMPLETE — LIVE-VERIFIED GREEN 2026-07-20**: a client webcam redirected over MS-RDPECAM now
  presents as a **live macOS camera** (Photo Booth, ~30 fps, zero dropped frames) via a hand-assembled
  (no-Xcode) CoreMediaIO Camera system extension — 3a activation + 3b sink feed + 3c 420v format, all green.
  As far as is known the first OSS RDP *server* to present a client-redirected webcam as a native OS camera.
  **Four silent CMIO failure modes were found and are documented in `docs/camera-extension-setup.md` — read
  it before touching this**: bundle filename must == bundle id; `signingID` is literally "unknown" (so sink
  producer auth is impossible and a rejecting hook surfaces as a bogus `-4`); `kCMIOStreamPropertyDirection`
  is INVERTED (pick the sink by NAME — starting the wrong stream returns SUCCESS and silently never drains);
  and macOS won't replace a same-CFBundleVersion extension (monotonic build number now). Extension `os_log`
  needs `sudo` to read. Remaining follow-ups (non-blocking): env-gate the Phase-2 $TMPDIR dumps; a controller
  "disable camera" menu item. **RELEASED as v0.9.0** (tag `efbe3d0`, published latest — the first minor bump since the 0.8 series began). Decode diagnostics are now opt-in behind `MACRDP_CAMERA_DUMP=1`; `--enable-camera-redirection` is documented in `docs/cli.md`. **Release gotcha worth remembering: the first tag failed CI** because a helper used from cross-platform code was `#[cfg(target_os = "macos")]` — a macOS-green clippy/test run cannot catch a Linux-stub break (no local Linux C toolchain). **Phase 4** (UDP migration) still scoped + deferred, low priority. Full plan +
  the live-debugged wire lessons: `~/.claude/plans/camera-redirection-phase1.md` +
  `docs/rdp-camera-redirection-feasibility.md` + [[project_camera_redirection_feasibility]].

- [x] **FreeRDP audio smoothness — server-side render-latency estimator — DROPPED 2026-07-20
  (built + tested, no perceptible benefit).** From the 2026-07-18 audio research: FreeRDP sends
  TWO WaveConfirms per wave and the server can measure the client play-out depth from them (mstsc
  sends one, so it can't). Phase 1 (observe-only harness, vendored `ironrdp-rdpsnd` fork) was GREEN
  — the signal tracked a 6%-loss stress link (depth 0.3ms→380ms, ship→play 4ms→15.5s) and proved
  the send-side drop-stale model MISSES a backlog living in the kernel socket buffer. Phase 2 (act:
  drop a wave when measured depth > threshold) was built + live-tested on Thincast + the shaper and
  **fired once the whole run** (the client's own buffering keeps the smoothed depth under threshold
  for the common PCM/self-managing case) and **sounded identical** to the no-drop run. Only
  FreeRDP-with-AAC has real value (its client overrun-dropper exempts AAC) — narrow, untested, not
  what's run here (mstsc can't use it). Not worth a vendored fork + hot-path divergence for zero
  audible benefit. **Branch + fork DELETED; main untouched (git pin intact).** The RESEARCH is the
  durable win and is retained (docs/known-quirks.md "Why mstsc audio is mostly smooth" +
  [[project_waveconfirm_not_playback_position]] + [[project_av_choppiness_contention]]).
  **Don't re-attempt without a specific "FreeRDP-AAC choppy under loss" complaint; even then
  `/sound:latency:<ms>` (client-side, zero divergence) is the first try.** Plan FINAL OUTCOME:
  `~/.claude/plans/freerdp-render-latency-estimator.md`.

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
    erroring. Verified live with a USB-3.2 flash drive. New vendored crate divergence. **NOTE: the
    upstream form (#1418) was reshaped to newtype-over-`u32` + consts at CBenoit's request — so at
    pin-bump this vendored `Other(u32)` shape is REPLACED and call sites shift
    `SupportedUsbVer::Usb32`→`::USB_32`; see the Upstreaming-watch #1418 entry.**
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
  - **Phase 3.2 remaining** (generic control-IN forwarding is DONE — the 2026-07-07 hardening pass
    added `UsbHandle::control_transfer_in`; Get Max LUN verified forwarded+answered live). Left:
    **(a)** explicit `RETRACT_DEVICE` PDU — SMALL but low value, the client channel-close path already
    covers detach/reset live-verified; **(b)** true multi-device — MEDIUM, needs the iSerialNumber
    string descriptor to distinguish identical models + two identical devices to test; **(c)**
    dispatch-priority tier. Test rig proven: UTM-QEMU Linux FreeRDP + USB-2.0 hub. Plan:
    `~/.claude/plans/wobbly-honking-minsky.md` §3.2.
  - **Phase 3.3 ISOCHRONOUS transfer spike — M0 (observe-only) LANDED, live go/no-go DEFERRED**
    (2026-07-10, user away from the USB rig). Isoch is the last missing transfer type
    (control/bulk/interrupt all work); it blocks isoch webcams + **USB audio** (mic/speaker).
    Staged M0 (observe) → M1 (forward isoch-IN), **IN-only**, a USB **mic** as the lowest-bandwidth
    go/no-go device. **Key finding: the vendored `ironrdp-rdpeusb` PDU layer is already isoch-ready**
    (`UrbFunction::IsochTransfer`(10) + `TsUrb::IsochTransfer` encode/decode/size; rides the existing
    TransferIn/Out envelope; `UrbCompletion::decode` auto-synthesizes the per-packet IN result) —
    **NO ironrdp-rdpeusb change needed for an IN spike.** **M0 code is written + compiles
    warning-free + an entitled `.app` is built/installed to `/Applications`** (uncommitted
    working-tree changes on `main`, ALL in `src/usb_redirect/usb_spike.m` only; default path
    byte-identical since USB redir is opt-in): (1) `macrdp_ep_transfer_type()` reads the
    EndpointCreate descriptor ptr (`data1`) → `bmAttributes&0x03`; create log tags `(ISOCHRONOUS)`.
    (2) `walkEndpoint:` catches `IOUSBHostCIMessageTypeIsochronousTransfer` (0x3b) before the
    default "unexpected type" path. (3) `observeIsochTransfer:` logs each TRB's frame#/ASAP/len/buf
    + inter-TRB **gap (ms)** for cadence and completes it **zero-length success** (NOT forwarded);
    throttled first-32-then-every-256th; counters cleaned on EndpointDestroy.
    **PENDING LIVE TEST:** `launchctl bootout gui/$(id -u)/com.clintcan.macrdp`, run foreground
    `/Applications/macrdp.app/Contents/MacOS/macrdp --keychain --enable-usb-redirection 2>&1 |
    grep -E 'usb2|ISOCH'`, FreeRDP `/usb:id,<vid>:<pid>` a mic (release the client's own audio
    driver first), open **Audio MIDI Setup**/QuickTime to open the isoch EP. **GREEN** = steady
    `ISOCH observe … gap≈1.000ms` lines → build **M1** (mirror the bulk path across all 3 layers:
    `isoch_transfer_in` UsbHandle method + `TransferReq::Isoch` arm + isoch endpoint classification
    out of `interrupt_eps`; expect to need DEPTH like the bulk read-ahead engine). **RED** = no
    lines / immediate teardown / sub-ms bursts we can't meet over the network → document the ceiling
    and stop (like the UDP client-support finding). Core risk M0 answers: can isoch meet macOS's
    timing over a network redirect (no retransmit + tight service intervals) at all. OUT/speakers
    would need the full `UsbdIsoPacketDesc` back (out of scope; IN-only). Plan:
    `~/.claude/plans/wobbly-honking-minsky.md`.
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
  dedicated OS thread; exits code 70 on a sustained wedge → launchd restarts;
  on by default when headless, env-tunable). **Tier 2.5 now complete.** Tier 2.6
  (per-connection worker processes) — **the `--fork-workers` model was REMOVED
  2026-07-17**: the reconnect-blank now self-heals in place via core reactivation
  (v0.8.27), so its reconnect-freshness rationale was moot and single-process is the
  only model. Still open beyond the soak: Tier 3 polish. Hard
  ceiling (NO-GO): multi-user concurrent GUI sessions (macOS limit).

## Parked — scoped, low priority

- [x] **Headless-lock arc — LANDED on `main` 2026-07-23 (pending a release tag).** Merged this
  session: **`--shield-primary`** opt-in lockable headless blanking (#170; single-panel engage
  #171, @antonmos), the **`Ctrl+Alt+G` majority-area gather** (mostly-off windows now swept),
  the **`--detach-primary` launchd-restart stopgap for #168** (#169, @antonmos-verified on
  26.5.2), the **blank-recovery established tier** (#172, @antonmos), plus #165 vd
  client-resolution auto-adopt and #166 iOS touch-tap fix. All adversarially security-scanned
  clean. **To run any of it live: signed rebuild+reinstall** (installed v0.9.0 predates them —
  build the shield helper via `gui/make-shield-helper.sh`; Team ID QGLA89KHM7). Two ship
  decisions remain the user's: (a) cut a release for the unreleased pile; (b) whether shield
  should ever become the default over `--capture-primary` (currently opt-in, capture unchanged).

- [ ] **#168 — `--detach-primary` panel-re-enable ROOT FIX (macOS 26.x).** The stopgap (#169,
  above) restarts the process so the CGS disable reverts, but the real fix — re-enabling the
  panel *in-process* — is not achievable via display-config transactions on 26.5 (@antonmos
  tried 5 tx structures; only process exit reverts the app-scoped disable). Issue #168 stays
  **open**. No known in-process path today; revisit if a future macOS restores it or a new
  CGS/SkyLight lever surfaces. `--shield-primary` sidesteps it entirely (no disable), so it may
  simply be the long-term headless direction on 26.x+ rather than something to fix in detach.

- [ ] **#167 — Dock invisible over RDP when a detach/capture vd is re-moded SMALLER than the
  still-online physical panel.** The Dock strip sizes to the largest *online* display, so a vd
  smaller than the (online-but-blanked) physical puts the Dock off the captured region. Repro'd
  on bare `main` — PRE-EXISTING (via #155 live-resize), not a regression. Fix = `ConfigureForSession`
  on the detach/capture disable tx, but that trades away crash-safety auto-revert → parked behind
  a future opt-in **`--persist-detach`**. Documented in `docs/known-quirks.md`, not shippable as-is.

- [ ] **Crash-report watch (post NSPasteboard-mutex fix).** The rare churn-time
  NSPasteboard use-after-free SIGSEGV was fixed 2026-07-07 via a process-global
  pasteboard mutex (`clipboard::pasteboard_guard()`, released in v0.8.29) — but it was
  rare, so it's unproven-by-repro. Keep the `.ips` files and watch new crash reports for
  a DIFFERENT signature. The 2026-06-28 `UNKNOWN_0x32` pthread crash is a separate,
  still-open one-off.

- [x] **Auto-sized virtual display — SUPERSEDED/ANSWERED by #155 + v0.8.39.** The open
  question ("does `CGVirtualDisplay applySettings` live-resize cleanly or need recreate?") is
  answered: **it live-resizes cleanly** (`VirtualDisplay::resize` re-applies a single mode via
  the shared `apply_single_mode`; the descriptor's 8192×8192 max is a lifetime cap). The
  feature itself shipped as live client-driven resize (MS-RDPEDISP, #155/v0.8.37, antonmos) —
  the vdisplay re-modes to the client's requested size on connect AND on every window
  drag/maximize — then v0.8.39 (#160 + #162) made it smooth on the headless path (see below).

- [x] **Smooth-resize on the headless path — DONE v0.8.39 (#160 + #162, closes #161,
  2026-07-18).** A live resize needs a core reactivation, which on
  `--capture-primary`/`--detach-primary` cascaded into a visible session re-cycle (gamma
  flicker + audio restart) and stranded windows/Dock. Fixes: **(1)** the overlay watcher polls
  through the reactivation's transient 1→0→1 session flap (2.5 s grace; the gap is variable
  ~0.5–0.9 s — a fixed sleep was proven insufficient) instead of tearing down the headless
  capture (#160); **(2) ROOT CAUSE of the Dock/window churn:** the vd's `(0,0)`/main
  placement was `ConfigureForAppOnly` — process-scoped, never persisted — so **every
  `applySettings` re-mode re-derived the arrangement from the WindowServer's session store**
  ("physical is main") and snapped the vd off `(0,0)`: Dock jumped to the blanked panel, and
  a variable-timing relayout kept re-stranding windows AFTER the post-resize gather placed
  them (both sweeps at +0.7 s/+1.7 s re-finding all 8 — sweep-retry is whack-a-mole, don't
  extend it). Fixed with **`ConfigureForSession`** (the store agrees → nothing to snap back
  to); crash-safety unchanged. Defense-in-depth kept: a **synchronous** `reanchor_as_main`
  after each re-mode (off-thread is too late — the Dock has already settled and won't
  re-follow) + a two-sweep post-resize auto-gather (#162). Live-verified consistent across
  maximize + drag-between-monitors on real mstsc. Full lesson: the vd-arrangement quirk note
  in `docs/known-quirks.md`. `DetachedPrimary` keeps `ForAppOnly` deliberately (its disable
  tx must auto-revert on SIGKILL); revisit only with live evidence of the same bug on detach.

- [ ] **`cycle_apps` lock nesting (`CYCLE_SESSION` → `mru`).** Currently safe by consistent
  acquire order; de-nest as a follow-up to the PR #84 hardening if revisiting that area.

- [x] **On-demand A/V resync hotkey (`Ctrl+Alt+Shift+R`) — DONE v0.8.38 (#159, 2026-07-17).**
  Manual recovery of a session gone stale after a long idle — a blanked screen and/or drifted
  audio (mstsc; the audiodg drift is client-side and un-observable from the server, so
  auto-detection is a dead end — this is a lever the user presses when they *see* it). Video
  forces a clean IDR keyframe (`Gfx::force_keyframe`, repaint the stale presentation); audio
  rebuilds its SCK stream via the existing self-heal `'reconnect` (a brief gap drains the
  client's backlog + re-baselines timing, like a minimize→unminimize). No disconnect;
  always-on like `Ctrl+Alt+G`; Win-key-free so mstsc forwards it. **Load-bearing: video uses a
  forced IDR, NOT the core reactivation** — the reactivation un-blanks too but on the
  `--virtual-display`/`--capture-primary` headless path cascades into a visible ~1–2 s session
  re-cycle (vd re-mod → #155 live-resize surface reset → SessionTracker teardown + headless
  re-capture); the IDR is flicker-free. `Gfx::request_reactivation` kept `#[allow(dead_code)]`
  as the escalation for a surface-retention blank an IDR can't clear. Live-verified on real
  mstsc (un-blanked smoothly, audio resynced, zero flicker). See the quirk note in
  `docs/known-quirks.md`.

- [ ] **Auto-mute on silence (audio-only).** Long-idle YouTube unpause loses audio
  (Windows audiodg suspends after hours of digital silence). Must be audio-only (not the
  shared `display_suppressed` gate, which would freeze the desktop). Note: the
  `Ctrl+Alt+Shift+R` resync hotkey (above, DONE) is the *manual* answer to this idle-audio
  drift; an *automatic* audio-only mute-on-silence is the still-parked hands-free version.

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
  **Four clintcan PRs currently OPEN (all MERGEABLE, awaiting review — reactive-only, do
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
    decode): fixes a real USB-3.2 device (SupportedUsbVersion `0x320`) tearing down the URBDRC
    channel on decode. **REVIEW ADDRESSED 2026-07-10 (commit `80c7c8b9`):** CBenoit REQUIRED
    dropping the `Other(u32)` fallback (it can alias a named value → breaks round-trip/Eq, a pattern
    they're purging codebase-wide), so the 4 device-reported fields are now **newtype structs over
    `u32` with named associated consts** (the `http::StatusCode` shape; matches the crate's own
    `UrbFunction`) — one representation per wire value, no aliasing. Added his requested
    decode-from-raw-bytes test + rstest round-trip. **Double-checked 2026-07-10: build + `clippy -D`
    + tests + fmt all clean, no external consumers.** **2026-07-15: uchouT (community) CHANGES_REQUESTED
    a `check_device_speed` edge — the constraint compared `== HIGH_SPEED` but per MS-RDPEUSB 2.2.11
    `DeviceIsHighSpeed` MUST be 0 at bus-iface-version 0, so a lenient newtype could wrongly accept a
    non-`0x1` non-zero speed. FIXED 2026-07-16 (`04c76e7a`): compare `!= FULL_SPEED` (any non-zero) +
    a 4-case raw-decode rstest. 2026-07-17: uchouT ENDORSED merge (pinged CBenoit "can we just merge
    this?") — community review done, awaiting only CBenoit's merge click. Do NOT double-nudge.**
    Scoped to (1) only — HELD
    rdpeusb (2) `UsbDevice=0` (CONFLICTS with merged #1321, which deliberately rejects that range;
    needs a "mstsc really sends 0 + capture" argument). On merge+release, most of rdpeusb divergence
    (1) drops. **Pin-bump churn: macrdp-side call sites shift `SupportedUsbVer::Usb32`→`::USB_32`
    consts (mechanical, only at bump).**
  - [ ] **#1420** `feat(rdpeusb)!: carry the full configuration descriptor in UsbConfigDesc` —
    FILED 2026-07-09 (was the drafted rdpeusb div (3)). `UsbConfigDesc` gains `trailing: Vec<u8>`
    (bytes 9..wTotalLength) so `TS_URB_SELECT_CONFIGURATION` carries the full configuration
    descriptor (real Windows rejects header-only with `0x80070057`). **REVIEW ADDRESSED 2026-07-10
    (commit `2cde264f`):** added Copilot's asked-for encode-time validation — `bLength`==9-byte
    header AND `wTotalLength`==header+trailing, so a caller can't emit a descriptor whose
    `wTotalLength` disagrees with the payload (+ `inconsistent_header_fails_to_encode` test).
    **Double-checked 2026-07-10: build/clippy-D/tests/fmt clean; amendment is purely additive
    validation.** Only Copilot has reviewed (no human yet). Independent of #1418 (different
    file/type); both touch `tests/rdpeusb/mod.rs` (one line) → rebase whichever merges second.
  - [ ] **#1453** `feat(acceptor): expose client multitransport flags on AcceptorResult` — opened
    2026-07-17. Upstreams the READ-side of vendored `ironrdp-acceptor` divergence (3): surfaces
    `pub multitransport_flags: gcc::MultiTransportFlags` (the client's GCC `MultiTransportChannelData`,
    MS-RDPBCGR §2.2.1.3.8, which the acceptor already parses then discards) so a UDP-capable server
    can decide whether to send a Server Initiate Multitransport Request. **Purely additive, zero
    behavior change** — a line-for-line twin of the merged `keyboard_layout` (#1397) / desktop-size
    (#1373) surfacing at all six touch points. No test (matches #1397/#1373 precedent; acceptor lib is
    `test=false`). clippy/fmt clean. The bulky M3c advertise/emit/offer HALF of divergence (3) stays
    vendored as macrdp UDP policy, so even merged+released this does NOT de-vendor the acceptor.
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
  **Trigger (whichever first):** (i) the small-PR wave merges — the rdpeusb pair (#1418 + #1420)
  and #1405 (+#1415/#1404 if they land) — maximizing the harvest to
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
