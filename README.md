# macrdp

[![Latest release](https://img.shields.io/github/v/release/clintcan/macrdp?sort=semver&label=release)](https://github.com/clintcan/macrdp/releases/latest)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)
[![Ko-fi](https://img.shields.io/badge/Ko--fi-buy%20me%20a%20coffee-FF5E5B?logo=ko-fi&logoColor=white)](https://ko-fi.com/clintcan)

A native RDP server for macOS, written in Rust on top of [IronRDP]. Connect from `mstsc`, Microsoft Remote Desktop, or FreeRDP to drive your Mac desktop with keyboard, mouse, real-cursor-shape forwarding, text + image clipboard sync, Mac↔Windows file copy, **read-write drive redirection** (mount the client's drives in Finder), **smart-card redirection** (use the client's smart card from macOS apps), system audio forwarding, and optional H.264 video (EGFX/AVC420, hardware-encoded). NLA/CredSSP is supported. Authenticates against your local Mac account via PAM.

This is the macOS equivalent of `xrdp`. Not a client, not a VNC bridge.

## Status

v0 — daily-driver usable on a trusted LAN, and usable **over the internet** (VPN / ZeroTier / high-latency links, including mobile). **Latest release: [v0.9.4](https://github.com/clintcan/macrdp/releases/latest)** — *security hotfix*: an unauthenticated remote client could wedge the **entire server** with a single malformed 2-byte frame — a pre-TLS 100%-CPU spin in the IronRDP framing reader (`read_by_hint`) that both the auth-guard and the health-check watchdog miss. macrdp now vendors `ironrdp-async` with a guard that **cleanly rejects the degenerate frame instead of spinning** (macrdp's upstream PR #1556). No other runtime change. Builds on **v0.9.3** (*storm-guard fix + the connection/input batch*): the mstsc/Windows-App reconnect-blank drop loop can no longer run away — the reconnect-storm guard's counter now resets only on a genuinely **established** (sustained) session, so a brief-present-then-blank counts toward the cap and the loop bounds/trips instead of cycling forever (live-verified over ZeroTier). Ships with four merged contributions (@antonmos): a second client now **takes over** the live session (full-auth-gated) instead of hanging (#174), a blank-recovery heal-confirmation deadline (#175), relative-mouse + edge-clamp input fixes (#176), and a clipboard pre-connect-sync fix (#173). The default runtime path is unchanged. Builds on **v0.9.2** (*blank-recovery clean-presentation latch*) and **v0.9.1** (*the lockable-headless release*: the opt-in **`--shield-primary`** headless blanking mode that keeps the Mac lockable, client-resolution auto-adopt on `--virtual-display`, and a `--detach-primary` launchd-restart stopgap for the macOS-26 panel-re-enable bug) and **v0.9.0** (*the webcam release*: a client webcam presents as a **real macOS camera** via `--enable-camera-redirection` — as far as is known the first known open-source RDP _server_ to do so; H.264 over MS-RDPECAM → VideoToolbox decode → a CoreMediaIO Camera system extension, live-verified at 1080p/~30 fps), v0.8.40 (the *headless-laptop release*), and v0.8.39 (the *smooth-resize release*).

Full per-release notes (what shipped, what was verified live, and the war stories): **[docs/release-history.md](docs/release-history.md)**.

## Production readiness

Short version: **a polished v0 daily-driver for trusted LANs and your own VPN — not an enterprise RDP server.** Use it to reach your own Mac over a network you control; don't put it on a public IP or treat it as multi-user/critical infrastructure.

**Solid (verified on real mstsc / Microsoft Remote Desktop / FreeRDP):** TLS + NLA/CredSSP auth against your Mac account (Keychain-backed, real CA certs supported, per-IP rate-limiting + lockout + audit log); the full daily workflow (display, input incl. non-US layouts, clipboard/files both ways, audio, drive + smart-card redirection, headless virtual displays); H.264 with congestion-responsive rate control that degrades gracefully instead of freezing; signed/notarized packaging with a LaunchAgent, menu-bar controller, and a health-check watchdog; 160+ tests in CI.

**Know before relying on it:** single session/single user; no multi-monitor or printer redirection; DRM video and password-manager windows capture black (macOS policy, not fixable); synthetic input can't reach the login window/secure fields (same); reconnecting *mstsc* can briefly show a blank screen (client quirk — the server now auto-heals it in ~4 s by reactivating the RDP core in place, no user action); the UDP paths are opt-in and newer than the TCP core; it's a solo v0 on vendored [IronRDP] forks, no SLA. **Never expose any RDP server on a raw public IP — reach it over a VPN or RD Gateway.**

Details and the path to closing the gaps: [docs/production-readiness-roadmap.md](docs/production-readiness-roadmap.md).

## Quick start

```bash
cargo build --release
codesign -s - --force target/release/macrdp   # ad-hoc sign so TCC grants persist
./target/release/macrdp
```

First run will prompt for:
1. **Screen Recording permission** (System Settings → Privacy & Security → Screen Recording → enable `macrdp` → restart it).
2. **Accessibility permission** (same path, "Accessibility" — required to forward keyboard and mouse).
3. Your Mac password at the terminal — validated against your local account via PAM `checkpw`, then used as the RDP credential.

Then connect from a client to `<your-mac-ip>:3390` with your Mac username and password. `mstsc` will prompt for credentials in its own NLA dialog — no need to pre-type the username.

Common flags to try next (full reference: [docs/configuration.md](docs/configuration.md)):

```bash
./macrdp --enable-h264                      # H.264 video — crisper AND lighter than the default bitmaps
./macrdp --enable-h264 --adaptive-bitrate   # + congestion-responsive rate control (recommended off-LAN)
./macrdp --bind 0.0.0.0:3390                # accept LAN connections (keep it OFF public IPs)
./macrdp --virtual-display --width 2560 --height 1440   # headless second desktop; local screen untouched
./macrdp --map-ctrl-to-cmd                  # Windows Ctrl+C/V/X muscle memory drives macOS copy/paste
```

## Hotkeys

macrdp reimplements the macOS symbolic hotkeys in user space (WindowServer won't fire them for forwarded events). On a **Windows client the Cmd key is the Windows key**, so press the Win-key equivalent:

| Keys (on the client) | Action |
|---|---|
| **Cmd+Tab** / **Cmd+Shift+Tab** | Cycle apps (forward / back); the app you land on is surfaced |
| **Cmd+\`** / **Cmd+Shift+\`** | Cycle windows of the current app |
| **Cmd+Space** | Spotlight |
| **Cmd+Shift+3 / 4 / 5** | Screenshots (full screen / region / Screenshot.app) |
| **Ctrl+Alt+G** | Gather windows stranded off the virtual display (headless `--capture-primary`/`--detach-primary` modes) |
| **Ctrl+Alt+Shift+R** | On-demand A/V resync — repaint a stale/idle-blanked screen (forced keyframe) and re-sync drifted audio, without disconnecting. Handy for mstsc after hours idle. |

Optional flags: `--alt-tab-switch` / `--alt-backtick-switch` accept **Option+Tab** / **Option+\`** as the same triggers; `--app-switcher-hud` draws a visible switcher overlay; `--map-ctrl-to-cmd` remaps Windows **Ctrl+C/V/X/…** editing shortcuts to their **Cmd** equivalents.

> **mstsc tip:** if **Cmd+Tab** seems ignored, set **Local Resources → Keyboard → "Apply Windows key combinations"** to **"On the remote computer"** (or go full-screen) — the windowed default eats **Win+Tab** locally as Task View.

## Auto-start at login (launchd)

```bash
dist/install.sh
```

Builds + signs + installs to `~/.local/bin/macrdp`, stores your Mac password in the macOS Keychain under service `macrdp`, drops a launchd plist at `~/Library/LaunchAgents/com.user.macrdp.plist`, and loads it. macrdp will start on every login and restart if it crashes. Re-run the script after `cargo build --release` to refresh the installed binary.

```bash
launchctl print gui/$UID/com.user.macrdp | head    # status
launchctl kickstart -k gui/$UID/com.user.macrdp    # restart
launchctl bootout gui/$UID/com.user.macrdp         # stop / uninstall
```

## Building the full app

`dist/install.sh` installs a bare binary. For a proper **signed `macrdp.app`** — stable bundle identity (TCC grants survive rebuilds), background-agent behavior, the embedded smart-card IFD handler, optional notarization, the menu-bar controller app, and a distributable DMG:

```bash
packaging/make-app.sh                                 # build + sign + install to /Applications
security add-generic-password -s macrdp -a "$(id -un)" -w 'YOUR_PASSWORD'
packaging/install-launchagent.sh                      # load LaunchAgent (label com.clintcan.macrdp)
```

Feature toggles, bind address, and extra flags live in `~/Library/Application Support/macrdp/config.env` — outside the bundle, so edits never disturb the signature or TCC grants. The full packaging guide (Developer-ID signing, notarization, the DMG, the controller app, icons, TCC notes): **[packaging/README.md](packaging/README.md)**.

## Release artifacts

Pushing a `v*` tag runs the [release workflow](.github/workflows/release.yml), which builds on an Apple-Silicon runner and attaches these to a draft GitHub Release (Apple Silicon / `aarch64-apple-darwin` only):

| File | What it is |
|------|------------|
| `macrdp-<ver>-aarch64-apple-darwin.tar.gz` | the **bare CLI binary** + `LICENSE`/`README` |
| `macrdp-<ver>-aarch64-apple-darwin-app.zip` | the full **`macrdp.app`**, with the embedded smart-card IFD handler + installer — the only artifact that carries everything `--enable-smartcard-redirection` needs |
| `SHA256SUMS` | checksums for both |

Both are **ad-hoc signed, not notarized** — open the app once via **right-click → Open** (or `xattr -dr com.apple.quarantine macrdp.app`). For a Developer-ID-signed + notarized build, or the menu-bar controller app (neither is produced in CI), build locally with [packaging/make-app.sh](packaging/README.md).

## Documentation

| Guide | What's in it |
|-------|--------------|
| **[Configuration & CLI](docs/configuration.md)** | Every flag, the auth-hardening environment variables (rate-limit/lockout/audit), headless mode (`--virtual-display`, `--detach-primary`/`--capture-primary`), and a full set of example invocations. |
| **[Video](docs/video.md)** | The H.264/EGFX pipeline, Retina capture (`--hidpi`), client-resolution auto-adopt and letterboxing, bitrate/keyframe tuning, the mstsc reconnect-blank quirk and its in-place auto-recovery, and the vImage color-conversion benchmarks. |
| **[Audio](docs/audio.md)** | RDPSND PCM, opt-in AAC compression (`--enable-aac`), the self-healing capture stream, and mute-on-minimize. |
| **[File copy](docs/file-copy.md)** | Mac↔Windows clipboard file copy (files and folder trees), lazy vs eager paste, and the two Windows-side gotchas (Explorer folder-copy, archive shell extensions). |
| **[Drive redirection](docs/drive-redirection.md)** | Mounting the client's drives as read-write Finder volumes (`--enable-drive-redirection`) — how the in-process NFS bridge works and what to expect from permissions. |
| **[Camera redirection](docs/camera-extension-setup.md)** | Presenting the client's **webcam as a real macOS camera** (`--enable-camera-redirection`) — the one-time system-extension setup and activation, how the MS-RDPECAM → VideoToolbox → CoreMediaIO pipeline fits together, and the four CoreMediaIO failure modes that all fail *silently*. **For a webcam use this, not USB redirection** — it's the path mstsc feeds. |
| **[Smart-card redirection](docs/smart-card-redirection.md)** | Using the client's smart card from macOS apps (`--enable-smartcard-redirection`) — one-time IFD-handler install, the USB-trigger caveat, upgrade/reload notes, and why it's a user-space handler rather than USB passthrough. |
| **[Audit log & SIEM](docs/audit-log.md)** | The security audit events (accept / reject / auth / disconnect) — every field and how to interpret them — plus [forwarding the JSON stream](docs/siem-forwarding.md) to a SIEM/SOC collector (Vector / Fluent Bit / rsyslog) and a runnable [OpenSearch SIEM tutorial](docs/siem-tutorial.md) that detects an RDP brute-force end-to-end. |
| **[vs. other OSS RDP servers](docs/oss-rdp-server-comparison.md)** | Two parts. **Part 1** — the evidence behind every "first" claim in these docs, verified adversarially against FreeRDP/xrdp and re-checked in the source, including what macrdp is **not** first at and how to re-verify when upstreams move. **Part 2** — an honest head-to-head against the other native macOS RDP servers (`x6nux/macrdp`, `RDPonMAC`), written steelmanning theirs, including where they beat us. |
| **[Release history](docs/release-history.md)** | Per-release narrative of what shipped and what was live-verified. |
| [CLAUDE.md](CLAUDE.md) | Developer/agent reference — architecture, feature status, macOS gotchas, known quirks. |

## Why this was made

This was done to scratch an itch. There are practically no active open source RDP servers for macOS. The closest project with this functionality is xrdp; however it only runs on Linux/Unix machines and has no homebrew equivalent on Macs. The initial POC was done in a few hours with the help of Claude and ran pretty well from the start. Additional combing through pcap files and documentation, and debugging each mstsc/FreeRDP connect, is what makes this work tedious yet rewarding when it finally works. Multi-monitor support is on the list for when I'm bored or need a distraction from real life.

## Support this project

macrdp is free and open source. If it's helped you out, you can buy me a coffee to help me get through the bumps — totally optional, no pressure.

- ☕ **[Buy me a coffee on Ko-fi](https://ko-fi.com/clintcan)**

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at your option. Being permissively licensed, a productized/notarized build may be sold commercially with support — that's selling the product, not a license exemption.

[IronRDP]: https://github.com/Devolutions/IronRDP
