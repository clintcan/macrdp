# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

> Layout note: this file is a lean index. The rules-and-gotchas docs are pulled in via
> `@import` below and load every session; the reference docs load ON DEMAND — read the
> matching one before working in its area. Keep each topic file self-contained and add
> new long-form material to the matching file rather than growing this one.
> - `@docs/macos-gotchas.md` — TCC, CGVirtualDisplay, QoS, activation (always loaded)
> - `@docs/known-quirks.md` — hard-won client/codec/audio behavioural notes (always loaded)
> - `@docs/conventions.md` — conventions worth keeping when adding code (always loaded)
> - `docs/features.md` — what works today, per-feature caveats. Read before changing or
>   describing a feature's behaviour.
> - `docs/architecture.md` — module map + cross-cutting design (TLS, auth, session and
>   process model, audio rate). Read before adding a module or touching a cross-cutting path.
> - `docs/cli.md` — build/run/test commands, the full CLI flag + env-var reference.
>   Read before adding or changing a flag, or when a command/tunable is needed.
> - `docs/oss-rdp-server-comparison.md` — the verified evidence behind the "first OSS
>   RDP server to…" claims (and what NOT to claim). Read before repeating any of them.
>
> The `vendor/ironrdp-*/` forks each have their own nested `CLAUDE.md` (the
> divergence logs) that load only when you work inside those directories.

## Status

Functional v0 — daily-driver usable on a trusted LAN and over the internet
(VPN/ZeroTier). **Latest release: v0.9.11** (input fixes from @antonmos: held modifiers on
mouse events, Ctrl+click→Cmd+click and Ctrl+,→Cmd+, under `--map-ctrl-to-cmd`, per-connection
input reset — #183/#184, vendored divergence (24)). Next: the IronRDP pin bump on its own branch.
**Per-release detail — what shipped, what was verified live, the war stories — lives in
`docs/release-history.md`; read it before reasoning about when/why something changed.**
Release cuts update that file, the README Status (replace the **Latest** line and push the
previous release down as a one-line bullet in **Recent releases** — keep it a list, never a
paragraph), and this Status line.

## Before you call a change "live"

- **`/Applications/macrdp.app` is not `target/release/macrdp`.** Restarting the LaunchAgent
  re-launches the *installed* bundle; it never rebuilds. And a quiet startup proves
  nothing, because `config.env` silently ignores unknown keys, so a stale binary just
  drops a new setting. Rebuild, verify and watch the log using the steps in `docs/cli.md`
  ("Deploying a change to the installed bundle").
- **Synthetic-input bugs: ask the user what they *saw*.** The log often records only a
  pass/fail outcome. Five different auto-unlock failures logged identically; "the field
  shook" or "one dot short" is what found the causes.
- **Connect-edge features need a real disconnect + reconnect to test.** Auto-unlock,
  restore-windows and the connect-time gather run on the session watcher's 0→1 edge;
  locking and unlocking inside a session that stays open never runs them.

## Project goal

A native RDP server for macOS written in Rust on top of [`ironrdp`](https://github.com/Devolutions/IronRDP). Functionally analogous to `xrdp` on Linux: Windows / cross-platform RDP clients connect to the Mac and see its desktop, with keyboard/mouse forwarded back.

Not a client, not a VNC bridge, not a proxy — the server terminates the RDP protocol itself and renders/feeds the local macOS session.

@docs/macos-gotchas.md

@docs/known-quirks.md

@docs/conventions.md
