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
(VPN/ZeroTier). **Latest release: v0.8.27** (the reconnect-blank-cracked
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
the shared bound flag (#137/#138/#139); **fork-workers sample the link RTT**
so the blank-recovery gate + bitrate seed apply there too (#138). Triple
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
bounces a hung-but-alive process so launchd / the `--fork-workers` supervisor
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
