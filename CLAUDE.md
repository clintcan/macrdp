# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

> Layout note: this file is a lean index. The reference sections live in separate
> files under `docs/` and are pulled in via `@import` below, so the full context
> still loads every session — keep each topic file self-contained and add new
> long-form material to the matching file rather than growing this one.
> - `@docs/features.md` — what works today (the capability list / status)
> - `@docs/architecture.md` — module map + cross-cutting design
> - `@docs/macos-gotchas.md` — TCC, CGVirtualDisplay, QoS, activation
> - `@docs/known-quirks.md` — hard-won client/codec/audio behavioural notes
> - `@docs/cli.md` — build/run/test commands + the full CLI flag reference
> - `@docs/conventions.md` — conventions worth keeping when adding code
> - `docs/oss-rdp-server-comparison.md` — the verified evidence behind the "first OSS
>   RDP server to…" claims (and what NOT to claim). Read before repeating any of them.
>
> The `vendor/ironrdp-*/` forks each have their own nested `CLAUDE.md` (the
> divergence logs) that load only when you work inside those directories.

## Status

Functional v0 — daily-driver usable on a trusted LAN and over the internet
(VPN/ZeroTier). **Latest release: v0.9.2** (blank-recovery clean-presentation latch — a
point release fixing a v0.9.1 regression in the mstsc/Windows-App reconnect-blank
auto-recovery; **default runtime path unchanged**, only the blank detector's disarm logic
changed. v0.9.1's #172 made the disarm *revocable* (to catch a client reporting nonzero-EDR
*while black*), which regressed the mirror client — one that **presents fine but reports
`timeDiffEDR == 0` mid-session**, whose short nonzero runs never reach the "established" bar —
so the aggressive path force-dropped its working ~50 s sessions and the ARC cookie reconnected
each time = a connect/disconnect loop on a session in active use. QoE decode+render-time is
*bidirectionally* unreliable, so no EDR threshold separates the two clients; the signal that
does is *when* the nonzero run occurs. Fix (`src/h264.rs`): a durable **`presented_clean`**
latch — a sustained nonzero-EDR run seen **before any recovery attempt** (`attempts == 0`)
proves the client painted the desktop at connect (not the reconnect-blank, which is black from
frame one), so the detector latches off for the connection = the v0.9.0 behavior, restored.
The `attempts == 0` guard preserves #172 (the blank client's nonzero-while-black flicker only
appears *after* its reactivation → latch withheld → escalation still recovers it).
LIVE-VERIFIED: a 6-min session held (was dropping every ~50 s) and a real reconnect-blank
still self-healed. Trade-off: a genuine *mid-session* blackout on a cleanly-presented
connection is no longer auto-recovered (unconfirmed case) — `Ctrl+Alt+Shift+R` is the manual
lever. **Known issue, deferred (NOT a regression):** a blank-recovery reactivation un-blanks
the `--capture-primary` physical panel (macOS resets gamma on the reconfiguration; the
same-size `Resize` skips the re-assert the resize path runs) — present since v0.8.27, only
noticed now; a gamma-timing reblank was flaky + reverted (`wip/capture-primary-reblank-on-heal`),
the robust fix is a shield-window helper (`--shield-primary` avoids the class). See
`docs/release-history.md` + the blank-recovery note in `@docs/known-quirks.md`.)
Earlier: **v0.9.1** (the lockable-headless release — a new
opt-in headless blanking mode **`--shield-primary`** covers the physical panel with an
opaque black *window* (via the `macrdpshield` helper) instead of capturing it, so unlike
`--capture-primary` **the Mac can still be locked** and there's no ~250 ms resize flash;
default OFF, capture/detach unchanged. Also: client-resolution auto-adopt on the
`--virtual-display` path (#165), a blank-recovery established-session tier (#172), a
majority-area `Ctrl+Alt+G` window-gather, and a `--detach-primary` launchd-restart stopgap
(#169) for the macOS-26 in-process panel-re-enable bug (#168, root fix still open). See
`docs/release-history.md` + the shield/lock notes in `@docs/known-quirks.md`.)
Earlier: **v0.9.0** (the webcam release — **a webcam
redirected from the client now presents as a REAL macOS camera**. Opt-in
`--enable-camera-redirection` (default OFF, default runtime path byte-identical when
off): tick "Video capturing devices" in the client and **"macrdp Camera"** appears in
Photo Booth / Zoom / FaceTime / Teams showing the client's live webcam. As far as is
known the **first *known* OSS RDP *server* to present a client-redirected webcam as a native
OS camera** (FreeRDP ships `channels/rdpecam/server/`, but that is a protocol
endpoint — it hands raw samples to a callback and never decodes or registers an OS
device; the first-ness is the end-to-end presentation, not speaking MS-RDPECAM) — and the path that actually works for **mstsc**, which routes webcams over
MS-RDPECAM and refuses the raw-USB reads (`0x8007001f`) the USB path needs. Pipeline:
H.264 samples over the MS-RDPECAM `RDCamera` DVC (plain TCP — UDP is NOT a prerequisite)
→ **VideoToolbox** decode to `420v` `CVPixelBuffer`s → macrdp as a **CoreMediaIO client**
enqueues them onto the **sink stream of a CoreMediaIO Camera system extension**
(IOSurface-backed ⇒ **zero-copy**) → the extension forwards to its source stream, which
apps see. LIVE-VERIFIED on real mstsc at 1080p/~30 fps, **zero dropped frames**. The
extension is **hand-assembled from a plain SwiftPM target — no Xcode** — signed +
notarized, activated once from the menu-bar controller ("Enable macrdp Camera…"; the
`system-extension.install` entitlement is self-serviceable, no Apple grant). Setup
runbook: `docs/camera-extension-setup.md`. **FOUR silent CMIO failure modes were found
and are documented there — read it before touching this, every one fails with NO error:**
(1) the `.systemextension` filename MUST equal its `CFBundleIdentifier`; (2)
`CMIOExtensionClient.signingID` is literally the string `"unknown"`, so sink-producer
auth is impossible and a rejecting hook surfaces as a bogus `CMIODeviceStartStream -4`;
(3) **`kCMIOStreamPropertyDirection` is INVERTED** vs the headers — pick the sink by
NAME, since starting the wrong stream RETURNS SUCCESS while nothing ever drains; (4)
macOS never replaces a same-`CFBundleVersion` system extension (monotonic build number
now). Also: a CMIO extension's `os_log` needs `sudo` to read, and every extension change
costs a reboot to test. Decode diagnostics are now opt-in behind `MACRDP_CAMERA_DUMP=1`.
Migrating the camera channel to UDP is scoped but deferred — TCP carries it fine.)
Earlier: **v0.8.40** (the headless-laptop release —
three fixes for daily headless-laptop use plus a which-client audit signal, **no
change to the default runtime path**; the first three are opt-in or headless-only.
**(1) Opt-in `--restore-windows-on-disconnect`** (config
`RESTORE_WINDOWS_ON_DISCONNECT=1`) makes windows follow you: the process-lifetime
virtual display strands its windows off-screen on disconnect (invisible on a
laptop's built-in panel), so this sweeps them back onto the built-in on disconnect
(Mac usable locally) and auto-gathers them onto the vd on reconnect (no
`Ctrl+Alt+G`) — reuses the gather machinery, default OFF (a remote-only server
wants windows to stay on the vd). Live-verified: 6 windows swept home. **(2) Dock
no longer disappears on disconnect** — follow-on to v0.8.39: `CapturedPrimary::
install` persists vd-as-main via `ConfigureForSession`, but `drop` reverted only
`ConfigureForAppOnly` (process-scoped), so the session store still said "vd is
main" while the physical went back to (0,0) and the Dock sometimes followed the
phantom vd off-screen; `drop` now persists the restore too (symmetric). **(3)
`--capture-primary` blanking survives a live resize** — a re-mode (`applySettings`)
is a display reconfiguration and macOS resets the gamma tables on one, un-blanking
the panel (which v0.8.39 keeps engaged, so nothing re-applied it → the desktop
STAYED showing); now `CapturedPrimary::reassert_blanking()` re-applies the all-black
LUT after the re-mode and after each post-resize gather sweep (the gather's relayout
re-resets gamma ~700 ms in). Dead ends confirmed live + removed: a
`CGDisplayRegisterReconfigurationCallback` NEVER fired (the private `applySettings`
resets gamma without a public reconfiguration event), and a timed polling burst
can't beat it (a gamma write DURING a reconfiguration doesn't stick). **Documented
residual (accepted):** a ~250 ms desktop flash WHILE the re-mode is in flight —
macOS shows the desktop during the reconfiguration and won't let gamma stick until
it commits; the only fix is a black shield-window helper process, not worth it for
a local-panel flash during an intentional resize. **(4) Client fingerprint audit
record** (`event="fingerprint"`, #163) names which RDP client connected —
`client_name`/`rdp_version`/`client_build`/`platform` from the handshake, in
`macrdp.log` + the opt-in SIEM JSON stream; fingerprinting not auth (spoofable) but
tells clients apart: mstsc=real Windows build+`WINDOWS`, FreeRDP=build 2600+`UNIX`,
Thincast=18363+`UNSPECIFIED`. See the vd-arrangement + capture-primary quirk notes.)
Earlier: **v0.8.39** (the smooth-resize release —
polish for live client-driven resize on the **headless** path, **no change to
the default runtime path**. Two fixes: **(1)** the headless overlay watcher
now **polls through the reactivation's transient session flap** (1→0→1, up to
a 2.5 s grace) instead of tearing down + re-engaging the headless capture on
every resize — killing the gamma flicker and the audio restart (#160); **(2)**
**the Dock + windows stay put across a re-mode** — root cause was the vd's
`(0,0)`/main placement being `ConfigureForAppOnly` (process-scoped, never
persisted), so every `applySettings` re-mode re-derived the arrangement from
the WindowServer's session store ("physical is main") and snapped the vd off
`(0,0)`: the Dock jumped to the blanked panel and a variable-timing relayout
kept re-stranding windows (sweep-retry proved whack-a-mole). Fixed with
**`ConfigureForSession`** — the store agrees, a re-mode has nothing to snap
back to; crash-safety unchanged (capture/gamma stay process-scoped; a dead
process's vanishing vd auto-restores the physical as main). Defense-in-depth:
a **synchronous** `reanchor_as_main` after each re-mode (off-thread it's too
late — the Dock has already settled) + a two-sweep post-resize auto-gather
(#162, closes #161). Live-verified consistent across maximize +
drag-between-monitors on real mstsc. See the vd-arrangement quirk note.)
Earlier: **v0.8.38** (the A/V resync hotkey — a small
feature release, **no change to the default runtime path**. Adds an on-demand
**`Ctrl+Alt+Shift+R`** to recover a session gone stale after a long idle: an
mstsc session left idle for hours can blank the picture and drift the audio
(Windows' audiodg buffers playback downstream where the server can't observe it,
so auto-detection is a dead end). The chord — always-on like `Ctrl+Alt+G`,
Win-key-free so mstsc forwards it — forces a clean **IDR keyframe** (video, to
repaint the stale presentation) and **rebuilds the audio SCK stream** (so the
client's drifted backlog drains and re-syncs, the same effect as a
minimize→unminimize), with **no disconnect**. Live-verified on real mstsc:
un-blanked smoothly, audio resynced, zero flicker. The video uses an IDR rather
than the heavier core reactivation (`Gfx::request_reactivation`, kept as an
escalation) — the reactivation un-blanks too but on the
`--virtual-display`/`--capture-primary` headless path cascades into a visible
~1–2 s session re-cycle; the IDR is lighter and flicker-free. #159.) Earlier:
**v0.8.37** (live resize + the webcam stall
watchdog — two opt-in-path additions, **no change to the default runtime path**.
**Live client-driven resize (MS-RDPEDISP)**: when the client drags its window,
macrdp re-negotiates the session at the new size on the fly via a core
Deactivation–Reactivation (the same in-place machinery the mstsc blank-recovery
uses), re-moding a `--virtual-display` like a monitor changing resolution;
debounced so a drag settles to one reactivation. Verified on Windows App for
macOS (all session modes) + FreeRDP; a clean **no-op on mstsc** (it doesn't emit
the MS-RDPEDISP monitor-layout PDU on a window drag — DVC-traced — so no
regression). Contributed by Anton Mostovoy (#155). **USB webcam stall watchdog**:
a bulk webcam redirected over FreeRDP could stream then **freeze after a while
while the rest of the session kept working** — the read-ahead engine delivers
frame reads strictly in sequence order, and a bulk read the client never
completes (the camera stalls host-side: USB autosuspend / uvcvideo timeout /
bandwidth) was a permanent gap with no timeout, so the stream wedged until
re-attach. A `dispatch_source` watchdog (`usb_spike.m`) detects a stream waiting
with no in-order data for `MACRDP_USB_STREAM_STALL_MS` (default 3000; 0 disables),
completes the waiting ring head with a zero-length read → macOS re-COMMITs →
read-ahead re-engages when the client resumes, turning a transient host-side
stall into a self-recovering hiccup instead of a permanent freeze. The USB
read-ahead knobs (`USB_STREAM_STALL_MS`, `USB_PREFETCH_DEPTH`) are now
`config.env`-bridged. LIVE-VERIFIED on FreeRDP + an A4Tech webcam: a client-side
stall froze the picture, the watchdog fired at ~1.7–2.2 s, and the video resumed
in Photo Booth with no disconnect/crash (#158). Plus a README **Hotkeys**
section.) Earlier: **v0.8.36** (the fork-workers removal — a
cleanup release with **no change to the default runtime path** (single-process
was already the default). Removes the experimental **`--fork-workers`**
per-connection process model: a supervisor that `fork+exec`'d a fresh worker
process per RDP connection (xrdp's model), built to dodge mstsc's H.264 EGFX
reconnect-blank by giving each connection a fresh process. That blank has
**self-healed in place since v0.8.27** (a bare core Deactivation–Reactivation +
the Server Auto-Reconnect Cookie), and an exhaustive 2026-07-07 A/B found the
process model was a **net-negative** for interactive mstsc — mstsc opens an
abandoned extra TCP connection on each reconnect attempt, which stalled the
supervisor's serialized worker slot. So single-process + `--enable-h264` +
blank-recovery + ARC is now the only, field-proven model; a stale
`FORK_WORKERS=1` in a deployed `config.env` is ignored (unknown key), so existing
installs don't break. Net **−756 LOC** (−691 in `main.rs`); the GUI's
"Per-connection workers" toggle is gone too. Also lands **experimental
camera-redirection groundwork** (Phase 0, opt-in, inert by default):
**`--enable-camera-redirection`** negotiates the MS-RDPECAM
`RDCamera_Device_Enumerator` DVC and logs the client announcing its redirected
webcam — the protocol gate proving a modern mstsc/Win11 will hand macrdp a camera
over MS-RDPECAM (the channel USB redirection can't reach). It does **not** present
a camera yet (no per-device stream, no macOS capture); nothing changes when the
flag is off. Groundwork for a future native-macOS-camera feature.) Earlier:
**v0.8.35** (the USB read-ahead gate fix — a
one-fix point release over v0.8.34 reworking *how* the bulk-IN read-ahead engine
tells a streaming BULK endpoint apart from an HID interrupt endpoint, because
v0.8.34's method silently broke the **webcam**. Background: the v0.8.30 read-ahead
engine's `isBulkIn` test was address-only (non-EP0 IN + NormalTransfer), which an
**interrupt-IN pipe also satisfies**, so a redirected **gamepad enumerated but its
buttons did nothing** (v0.8.30 → v0.8.33; input reports at ~20 s instead of ~8 ms —
its interrupt pipe was routed into the streaming branch and only serviced on a
forced re-walk). **v0.8.34** gated instead on the endpoint's **declared** transfer
type (`is_bulk` from the client's SelectConfiguration pipe info, pushed to the ObjC
controller via `macrdp_usb_set_endpoint_bulk()`) — which fixed the gamepad but
**wrongly excluded the webcam**: a UVC video endpoint is frequently reported over
the wire with `is_bulk=false` (measured **69/81** on a redirected A4Tech cam), so
read-ahead never engaged and there was no image. **v0.8.35 gates on the transfer's
read LENGTH instead** — a reliable physical signal where the declared type is not:
a webcam's streaming bulk read is tens of KB (102656 B observed), an HID interrupt
poll is ≤ `wMaxPacketSize` (64 B), so the `isBulkIn` gate now requires
`normalReadLen >= 512`. The webcam's large reads engage read-ahead (byte-identical
to the known-good v0.8.33 path) while the gamepad's tiny polls stay serial; the
`is_bulk` plumbing is removed. Load-bearing: moving the size check up into the
`walkEndpoint:` gate — not just inside `engageStream:`, where v0.8.30–0.8.33 had it
— is what stops an interrupt endpoint's control flow from diverging into the
streaming branch at all. Live-verified: gamepad buttons work (1140 reports, stayed
serial), webcam read-ahead engages (`readLen=102656`, depth 4). **Don't re-break
it: at the UserHCI ring an interrupt endpoint is indistinguishable from a bulk one
by address + msg-type, AND by the wire-declared `is_bulk` flag — only the read
length reliably separates them. Whether *mstsc* then delivers a webcam's frames is
client-side (it prefers its own camera-redirection channel and can refuse the
raw-USB transfers with `0x8007001f`); the FreeRDP bulk-webcam path is unchanged.**)
Earlier: **v0.8.34** (the gamepad-input fix — the first repair of the above
gamepad regression, via the `is_bulk` declared-type gate that v0.8.35 replaces).
Earlier: **v0.8.33** (the audit-forwarding release — a
SIEM/SOC observability roll-up over v0.8.32, no change to the default runtime
path; everything here is opt-in + default-off and byte-identical when off. **Opt-in
structured JSON audit stream** (`--audit-file` / `MACRDP_AUDIT_JSON=1`, config
`AUDIT_FILE`): the per-connection `macrdp::audit` events (accept / reject / auth /
disconnect, with source IP+port, reason, outcome) are also written as one
schema-versioned JSON object per line on a dedicated self-rotating file for a
standard log collector (Vector / Fluent Bit / rsyslog / Splunk UF) to tail and
forward to a SIEM — macrdp deliberately does **not** speak network syslog (the
collector owns TLS/buffering/backpressure; macOS has no syslogd). The
human-readable `macrdp.log` audit lines are unchanged; the JSON file is an
additional sink, emitted **independent of `RUST_LOG`** (a `Targets` filter pins
`macrdp::audit=INFO`) so a quiet operational filter never suppresses security
events. New explicit **`event="auth"` login verdict** (`outcome="success"` when
CredSSP/NLA validates, else `"did_not_complete"` + a short `reason`), emitted once
per connection **after** the TLS upgrade — single-process path — so a SOC sees the
real authentication result instead of inferring it from the connection-duration
heuristic. The audit `reason` is **control-char-stripped** (log-injection
defense for the human-readable logfmt sink, which writes fields verbatim; the JSON
sink was already serde-safe) and length-bounded, never carrying credential
material. New `docs/audit-log.md` (per-event/-field interpretation guide with
worked examples) + `docs/siem-forwarding.md` (collector configs). An **end-to-end
CI job** drives a real FreeRDP `+auth-only` CredSSP handshake (correct + wrong
password) against a loopback server and asserts the JSON audit writes
(`scripts/test-audit-log.sh`) — a `cargo test` can't drive CredSSP (the harness is
TLS-only). Also lands inert isoch-USB observe-only groundwork (no functional
change).) Earlier: **v0.8.32** (the security-hardening release — a
security-focused roll-up over v0.8.31, no change to the default runtime path.
**Fuzzed the network-facing protocol decoders** with new in-tree `cargo-fuzz`
harnesses: `ironrdp-rdpeudp` (raw-UDP multitransport) came through ~250M execs
clean, but `ironrdp-rdpeusb` (URBDRC / USB-redirection PDUs) surfaced **3 real
panics** — unchecked `read_slice` on a truncated PDU in `TsUrbResult` /
`IoControlCompletion` / `TsUsbdInterfaceInfoResult` — now `ensure_size!`-guarded
(107M execs clean after; already fixed upstream, so they drop on the pin bump).
**`--max-client-size WxH`** (config `MAX_CLIENT_SIZE`) caps the client-requested
auto-adopt resolution, closing the audit residual where an authenticated client
could request 8192×8192 (~256 MB BGRA/frame); clamps in-band per-dimension,
refuses out-of-band, opt-in + byte-identical when unset, mirrors upstream #1404.
**Bounded the smart-card IFD-bridge `CMD_TRANSMIT`** allocation (was a 4 GB local
DoS on an unbounded wire `u32`) and documented the unauthenticated-loopback trust
boundary for the three helper channels. Added **scheduled `cargo-deny`** dep-vuln
scanning (daily, hardened runner, separate workflow). Plus **`--alt-backtick-switch`**
(Option+\` cycles the current app's windows, the Option analogue of
`--alt-tab-switch`; also fixes headless frontmost detection to read the AX
system-wide focused app), and the **blank-recovery + auto-reconnect tunables are
now `config.env` keys** (`BLANK_RECOVERY=0` etc. no longer need a plist edit).
Two contributions from Anton Mostovoy: the alt-backtick work and a
virtual-display descriptor-serial fix (per-pid, so two concurrent vd instances
don't collide). Earlier: **v0.8.31** (the gamepad-resilience release — a
one-fix point release over v0.8.30 hardening HID/gamepad USB redirection: a
redirected device's **interrupt-IN endpoint** (e.g. an Xbox controller's
input-report pipe) no longer goes dead when the client fails a single interrupt
read. mstsc intermittently completes an interrupt read with `hresult 0x8007001f`
(`ERROR_GEN_FAILURE`) while the device channel is still open; surfacing that as an
endpoint STALL made the macOS class driver give up polling the pipe (the gamepad
"hung" after seconds). The server (`src/usb_redirect/mod.rs`) now treats a
**channel-still-open transient failure on an interrupt endpoint as an empty poll**
(0 bytes, success) so the OS keeps the pipe alive and re-polls — matching interrupt
"no data ready" semantics (one dropped report is imperceptible; the next poll gets
fresh state). Scoped strictly to interrupt endpoints (`is_bulk == false`), so
mass-storage **bulk** keeps the strict stall (a real bulk error surfaces; a short
read never corrupts a transfer) and link death (`channel closed`) stays fatal for
clean teardown. Log marker: `interrupt-IN transient failure — completing as an
empty poll to keep the pipe alive`. Field note: a physically loose/jostled USB
cable causes the same dead-gamepad symptom (a real disconnect + re-enumeration —
`status=11` → `endpoint created ep=0x00` in the log) and is *not* software-fixable;
reseat the cable, which this fix correctly leaves alone.) Earlier: **v0.8.30**
(the webcam release — a
**bulk USB webcam redirected over FreeRDP now streams live video** into the Mac
session (`--enable-usb-redirection`, entitled build) — as far as is known a first
for any open-source RDP *server*. The blocker was USB **read-depth starvation**,
not the (separate, mstsc-only) camera-channel limit: macOS double/triple-buffers a
streaming bulk-IN endpoint (queuing several concurrent reads so the device pipe
never runs dry), but the user-space host controller's transfer ring exposes only
one transfer at a time, so serving reads one-at-a-time starved the camera (no data
→ macOS tore the stream down), while re-forwarding the same read for depth dropped
half the frame data. A **bulk-IN read-ahead engine** (`src/usb_redirect/usb_spike.m`)
now keeps `MACRDP_USB_PREFETCH_DEPTH` (default 4) concurrent `bulk_transfer_in`
reads in flight to the client, decoupled from the ring, buffered in **sequence
order** and delivered one chunk per ring transfer — restoring URB depth with no
data loss. Gated on the endpoint's **real transfer type** (the client's
SelectConfiguration pipe info), so **mass storage** (regression-verified
byte-exact) streams and **interrupt/HID** (the redirected gamepad) stays on the
serial path. That gate was originally address-only and silently broke HID input
from v0.8.30 to v0.8.33 — a redirected gamepad enumerated but its buttons did
nothing; fixed 2026-07-14, see the USB quirk note. No Rust change to the
transfer path (the URBDRC/`UsbHandle` side is already
per-token concurrent). Verified live on an A4Tech bulk UVC cam over Linux FreeRDP
(smooth video in Photo Booth); isochronous webcams + the mstsc camera-redirection
channel remain unimplemented. See the USB feature note in `@docs/features.md`.
Earlier: **v0.8.29** (the stability release —
a bug-fix roll-up over v0.8.28: closes a rare clipboard-churn **crash** (a
use-after-free from unsynchronized `NSPasteboard` access, now serialized behind a
process-global guard, #144); fixes a **scroll-wheel runaway** on the macOS Windows
App where a gentle scroll-down jumped to the bottom of the page — `ironrdp-pdu`
mis-decodes the 9-bit wheel-rotation field as sign-magnitude instead of two's
complement, so `-1..-3` deltas arrived as `-255..-253`; corrected at the vendored
handler (divergence (17)) plus per-event scroll accumulation, issue #113/#140;
stops a redirected **Xbox controller's Guide button** from tearing down the
USB-redirection session (its `SET_FEATURE` control-OUT is now routed via
`TRANSFER_IN` and the URBDRC send path is encode-tolerant, so no malformed URB can
kill the session); and makes `packaging/make-app.sh` auto-prefer a stable
self-signed `macrdp-dev` signing identity so Screen-Recording/Accessibility TCC
grants survive dev rebuilds — ad-hoc's cdhash-keyed identity does not, #141.
Earlier: **v0.8.28** (the gather-windows release — an
on-demand hotkey **`Ctrl+Alt+G`** (`Ctrl+Option+G`) sweeps windows stranded off
the virtual display in the headless modes (`--capture-primary`/`--detach-primary`)
back onto the display the client sees. In those modes the client sees the virtual
display, which occupies a different region of global coordinate space than the
physical panel, so a window opened on the physical panel before connecting is
invisible/unclickable over RDP; the hotkey walks each regular Dock app's
`AXWindows` via Accessibility and moves any window entirely off the target
display to just inside its top-left (partly-visible windows untouched). **Manual
by design** — an on-connect auto-gather was built first and rejected as too
surprising; the chord is Win-key-free so mstsc forwards it, and no-op when there's
no virtual display. Live-verified on real mstsc. See the stranded-windows quirk
note. Earlier: **v0.8.27** (the reconnect-blank-cracked
release — the mstsc reconnect-blank, documented for months as a not-server-
fixable client surface-retention bug, now **self-heals in place in ~4 s with no
disconnect**: on a detected blank the server sends a bare core RDP
Deactivation–Reactivation (Server Deactivate All → new Demand Active) that
**preserves the EGFX channel/surface** — no DeleteSurface, no DYNVC close, no
RESET_GRAPHICS, all of which were exhaustively proven client-fatal — and
mstsc re-maps its retained surface 0 and presents again. Live-verified 9/9
blanks healed on real mstsc/WiFi, EDR=0 → presenting in ~1-2 s, zero drops.
Default recovery action (`BlankAction::Reactivate`); the old connection-drop is
now only the fallback. Detection sped up via a wall-clock fast-path (~70 s →
~4 s on a static blank). Still RTT/QoE-gated so FreeRDP + high-latency links are
untouched. Zero vendored-server change — a no-op `DisplayUpdate::Resize` reuses
the existing reactivation path. See the reconnect-blank quirk note. Earlier:
**v0.8.26** (the roaming-client release —
UDP multitransport now configures and cleans up after itself, making
`ENABLE_LOSSY_AUDIO`/`ENABLE_UDP_MULTITRANSPORT` safe to leave permanently on
for a client that moves between networks: **RTT-gated offer** — links measured
at/above `MACRDP_UDP_OFFER_MAX_RTT_MS` (80 ms) at accept are never offered UDP
and run plain TCP from the first byte, so overlay links have no tunnel to wedge
(#136); **tunnel-lifecycle hardening** — an ended session's abandoned tunnel
retires quietly instead of triggering the 10-min offer cooldown (the false
cooldown that silently downgraded healthy-LAN reconnects, observed live), while
a tunnel that wedged BEFORE its session ended is still adjudicated as dead so
the reset-cycle protection can't be laundered away; offer cookies are evicted
on every connection end (was: leaked per failed handshake, with a
late-tunnel-bind zombie-peer window); all three peer-removal sites now lower
the shared bound flag (#137/#138/#139). Triple
adversarially reviewed; docs de-drifted.)
Earlier: **v0.8.25** (the resilient-link release —
three session-killers fixed, all live-verified over ZeroTier incl. mobile:
**oversized-cursor clamp** — a shake-to-locate/enlarged cursor at Retina backing
pixels overflowed the pointer PDU's u16 mask and the encode error tore down the
whole session, now downscaled to fit (#134); **link-aware blank recovery** —
kernel TCP RTT sampled per connection at accept gates the detector: evidence
window scales with RTT and the drop lever is withheld ≥80 ms, where the EDR==0
signal is untrustworthy and the false drops themselves poisoned mstsc into a
real permanent black (#135); **RTT-seeded adaptive bitrate** — slow links start
at ceiling/3 and climb instead of overshooting the pipe (#135); **UDP
tunnel-death detection + offer cooldown** — a wedged tunnel falls audio back to
TCP in ~30 s and suppresses multitransport offers so mstsc's dead-tunnel reset
reconnects as a stable plain-TCP session, breaking the reset cycle (#133).)
Earlier: **v0.8.24** (the remote-link release: RTT-aware
adaptive rate control — standing-queue-delay signal, no-ack distress fallback,
IDR backoff on both transports, ZeroTier-verified; Windows App for Android
support via channel-level EGFX decline; 2× faster blank recovery; parked idle
pollers. Lossy audio is LAN/WiFi-only until the UDP-tunnel keepalive lands.)
Earlier: **v0.8.23**
(the production-readiness arc — Tier 1 + Tier 2.5 of
`@docs/production-readiness-roadmap.md`: operator-supplied TLS certs `--cert`/`--key`,
connection rate-limiting + lockout + an auth audit log, bounded log rotation + a
startup reaper, and — v0.8.23 — a **health-check watchdog** (`src/health.rs`) that
bounces a hung-but-alive process so launchd
restarts a fresh one, closing the gap `KeepAlive` couldn't. v0.8.22 auto-recovers
the mstsc EGFX reconnect-blank via QoE-EDR detection + a Server Auto-Reconnect
Cookie; v0.8.21 fixed an auth-guard false-lockout of a legitimately reconnecting
client — surfaced by the Tier 2.4 soak).
RDP clients (mstsc, Microsoft Remote Desktop, FreeRDP) connect over TLS and get
the macOS desktop with keyboard/mouse/clipboard/audio, optional H.264-over-EGFX,
headless virtual displays, drive + smart-card redirection, and (opt-in) UDP
multitransport. See `@docs/features.md` for the full, current capability list and
the per-feature caveats; not-yet-implemented items are called out there too.

## Project goal

A native RDP server for macOS written in Rust on top of [`ironrdp`](https://github.com/Devolutions/IronRDP). Functionally analogous to `xrdp` on Linux: Windows / cross-platform RDP clients connect to the Mac and see its desktop, with keyboard/mouse forwarded back.

Not a client, not a VNC bridge, not a proxy — the server terminates the RDP protocol itself and renders/feeds the local macOS session.

@docs/features.md

@docs/architecture.md

@docs/macos-gotchas.md

@docs/known-quirks.md

@docs/cli.md

@docs/conventions.md
