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

Functional v0 — daily-driver usable on a trusted LAN. **Latest release: v0.8.21**
(the production-readiness arc: operator-supplied TLS certs `--cert`/`--key`,
connection rate-limiting + lockout + an auth audit log, and bounded log rotation +
a startup reaper — Tier 1.1/1.2/2.5 of `@docs/production-readiness-roadmap.md`.
v0.8.21 fixes an auth-guard false-lockout that could block a legitimately
reconnecting client — surfaced by the Tier 2.4 soak).
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
