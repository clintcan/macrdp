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
>
> The `vendor/ironrdp-*/` forks each have their own nested `CLAUDE.md` (the
> divergence logs) that load only when you work inside those directories.

## Status

Functional v0 — daily-driver usable on a trusted LAN and over the internet
(VPN/ZeroTier). **Latest release: v0.8.35** (the USB read-ahead gate fix — a
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
