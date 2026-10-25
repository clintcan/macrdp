# macrdp

[![Latest release](https://img.shields.io/github/v/release/clintcan/macrdp?sort=semver&label=release)](https://github.com/clintcan/macrdp/releases/latest)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)
[![Ko-fi](https://img.shields.io/badge/Ko--fi-buy%20me%20a%20coffee-FF5E5B?logo=ko-fi&logoColor=white)](https://ko-fi.com/clintcan)

A native RDP server for macOS, written in Rust on top of [IronRDP]. Connect from `mstsc`, the Windows App / Microsoft Remote Desktop, or FreeRDP and drive your Mac — authenticated against your local Mac account (NLA/CredSSP, TLS). What you get:

- **Desktop:** H.264 video (hardware-encoded, opt-in, with optional adaptive bitrate) or legacy bitmaps; keyboard (non-US layouts auto-detected) and mouse with the real cursor shape; the session follows your client's resolution.
- **Clipboard & files:** text, images and rich text both ways; copy files and folders between Mac and Windows.
- **Audio:** Mac audio to the client; the client's microphone as a real Mac input device *(experimental)*.
- **Devices from the client:** drives mounted in Finder (read-write), smart cards for macOS apps, the webcam as a real Mac camera, and USB devices *(experimental)*.
- **Headless:** serve a virtual display so the Mac needs no monitor, with the physical screen blanked while you're connected.

This is the macOS equivalent of `xrdp`. Not a client, not a VNC bridge.

## Status

v0 — daily-driver usable on a trusted LAN and over a VPN (ZeroTier, high-latency and mobile links included). Scope and limits: [Production readiness](#production-readiness).

**Latest: [v0.9.11](https://github.com/clintcan/macrdp/releases/latest)** — input fixes: held modifiers now reach clicks, drags and scrolls (Shift-click selection, Cmd-click), a Ctrl+click becomes Cmd+click and Ctrl+, opens Preferences under `--map-ctrl-to-cmd`, and a disconnect mid-click no longer leaves a stuck button or modifier on the next connection (@antonmos, #183, #184).

**Recent releases**

- **v0.9.10** (2026-09-30) — microphone redirection *(experimental)*; `--lock-on-disconnect` no longer locks under a reconnecting client
- **v0.9.9** (2026-09-29) — rich-text clipboard in both directions
- **v0.9.8** (2026-09-27) — lock while away: `--lock-on-disconnect` + `--auto-unlock` *(experimental)*
- **v0.9.7** (2026-09-19) — security patch: rustls, cryptoki and h2 advisories
- **v0.9.6** (2026-08-16) — fixes a scroll-down regression and an unauthenticated denial of service
- **v0.9.5** (2026-08-06) — IronRDP update; two vendored forks retired
- **v0.9.4** (2026-08-05) — security hotfix for a pre-TLS denial of service
- **v0.9.3** (2026-07-29) — a second client takes over the session; blank-recovery storm-guard fix
- **v0.9.2** (2026-07-26) — blank recovery no longer drops a session that is working
- **v0.9.1** (2026-07-23) — `--shield-primary`: headless and still lockable
- **v0.9.0** (2026-07-20) — the client's webcam as a real Mac camera

Full per-release notes (what shipped, what was verified live, and the war stories): **[docs/release-history.md](docs/release-history.md)**.

## Production readiness

Short version: **production-ready for single-user use on a LAN or VPN** — soak-tested past 48 h twice with no leaks or panics, hardened, and abuse-tested. **Not an enterprise RDP server:** one session at a time, a solo project with no SLA, and features marked *experimental* (for example USB, microphone, lock-on-disconnect/auto-unlock) haven't been soak-tested. **Never expose it — or any RDP server — on a public IP;** reach it over a VPN or RD Gateway.

**Solid (verified on real mstsc / Microsoft Remote Desktop / FreeRDP):** TLS + NLA/CredSSP auth against your Mac account (Keychain-backed, real CA certs supported, per-IP rate-limiting + lockout + audit log); the full daily workflow (display, input incl. non-US layouts, clipboard/files both ways, audio, drive + smart-card redirection, headless virtual displays); H.264 with congestion-responsive rate control that degrades gracefully instead of freezing; signed/notarized packaging with a LaunchAgent, menu-bar controller, and a health-check watchdog; soak-tested past 48 h twice (v0.9.3, v0.9.6; no leaks, 0 panics) and abuse-tested; 250+ tests in CI.

**Know before relying on it:** single session/single user; no multi-monitor or printer redirection; DRM video and password-manager windows capture black (macOS policy, not fixable); synthetic input can't reach the login window/secure fields (same); reconnecting *mstsc* can briefly show a blank screen (client quirk — the server now auto-heals it in ~4 s by reactivating the RDP core in place, no user action); the UDP paths are opt-in and newer than the TCP core; it's a solo v0 on vendored [IronRDP] forks, no SLA. **Never expose any RDP server on a raw public IP — reach it over a VPN or RD Gateway.**

Details and the path to closing the gaps: [docs/production-readiness-roadmap.md](docs/production-readiness-roadmap.md).

## Quick start

**With Homebrew** (Apple Silicon, macOS 14+), installing the same notarized build as the releases:

```bash
brew tap clintcan/tap
brew trust clintcan/tap    # current Homebrew asks you to trust third-party taps
brew install --cask macrdp
```

Then open **macrdpController** from /Applications: it stores your Mac password in the Keychain and starts the server at login. After `brew upgrade`, restart the server with `launchctl kickstart -k gui/$UID/com.clintcan.macrdp`. It's a cask rather than a formula on purpose: only the signed app carries the entitlement USB redirection needs, and it keeps your permission grants across upgrades.

**From source:**

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

Optional flags: `--alt-tab-switch` / `--alt-backtick-switch` / `--alt-space-spotlight` accept **Option+Tab** / **Option+\`** / **Option+Space** as the same triggers; `--app-switcher-hud` draws a visible switcher overlay; `--map-ctrl-to-cmd` remaps Windows **Ctrl+C/V/X/…** editing shortcuts to their **Cmd** equivalents.

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

Both are **ad-hoc signed, not notarized** — open the app once via **right-click → Open** (or `xattr -dr com.apple.quarantine macrdp.app`).

Each release also carries **`macrdp-<ver>-usb.dmg`**, built and attached by hand rather than in CI: the **Developer-ID-signed, notarized, entitled** build with both the server and the menu-bar controller app. It's the only build in which `--enable-usb-redirection` works, and it's what the [Homebrew cask](#quick-start) installs. To make your own signed build, use [packaging/make-app.sh](packaging/README.md).

## Documentation

| Guide | What's in it |
|-------|--------------|
| **[Configuration & CLI](docs/configuration.md)** | Every flag, the auth-hardening environment variables (rate-limit/lockout/audit), headless mode (`--virtual-display`, `--detach-primary`/`--capture-primary`), and a full set of example invocations. |
| **[Video](docs/video.md)** | The H.264/EGFX pipeline, Retina capture (`--hidpi`), client-resolution auto-adopt and letterboxing, bitrate/keyframe tuning, the mstsc reconnect-blank quirk and its in-place auto-recovery, and the vImage color-conversion benchmarks. |
| **[Audio](docs/audio.md)** | RDPSND PCM, opt-in AAC compression (`--enable-aac`), the self-healing capture stream, mute-on-minimize — and the experimental **microphone redirection** (`--enable-microphone-redirection`): the client's mic as a real macOS input device. |
| **[File copy](docs/file-copy.md)** | Mac↔Windows clipboard file copy (files and folder trees), lazy vs eager paste, and the two Windows-side gotchas (Explorer folder-copy, archive shell extensions). |
| **[Drive redirection](docs/drive-redirection.md)** | Mounting the client's drives as read-write Finder volumes (`--enable-drive-redirection`) — how the in-process NFS bridge works and what to expect from permissions. |
| **[Camera redirection](docs/camera-extension-setup.md)** | Presenting the client's **webcam as a real macOS camera** (`--enable-camera-redirection`) — the one-time system-extension setup and activation, how the MS-RDPECAM → VideoToolbox → CoreMediaIO pipeline fits together, and the four CoreMediaIO failure modes that all fail *silently*. **For a webcam use this, not USB redirection** — it's the path mstsc feeds. |
| **[Smart-card redirection](docs/smart-card-redirection.md)** | Using the client's smart card from macOS apps (`--enable-smartcard-redirection`) — one-time IFD-handler install, the USB-trigger caveat, upgrade/reload notes, and why it's a user-space handler rather than USB passthrough. |
| **[Audit log & SIEM](docs/audit-log.md)** | The security audit events (accept / reject / auth / disconnect) — every field and how to interpret them — plus [forwarding the JSON stream](docs/siem-forwarding.md) to a SIEM/SOC collector (Vector / Fluent Bit / rsyslog) and a runnable [OpenSearch SIEM tutorial](docs/siem-tutorial.md) that detects an RDP brute-force end-to-end. |
| **[vs. other OSS RDP servers](docs/oss-rdp-server-comparison.md)** | Two parts. **Part 1** — the evidence behind every "first" claim in these docs, verified adversarially against FreeRDP/xrdp and re-checked in the source, including what macrdp is **not** first at and how to re-verify when upstreams move. **Part 2** — an honest head-to-head against the other native macOS RDP servers (`x6nux/macrdp`, `RDPonMAC`), written steelmanning theirs, including where they beat us. |
| **[Release history](docs/release-history.md)** | Per-release narrative of what shipped and what was live-verified. |
| [CLAUDE.md](CLAUDE.md) | Developer/agent index — current release, plus pointers to the feature list, architecture, macOS gotchas and known quirks. |

## Why this was made

This was done to scratch an itch. There are practically no active open source RDP servers for macOS. The closest project with this functionality is xrdp; however it only runs on Linux/Unix machines and has no homebrew equivalent on Macs. The initial POC was done in a few hours with the help of Claude and ran pretty well from the start. Additional combing through pcap files and documentation, and debugging each mstsc/FreeRDP connect, is what makes this work tedious yet rewarding when it finally works. Multi-monitor support is on the list for when I'm bored or need a distraction from real life.

## Support this project

macrdp is free and open source. If it's helped you out, you can buy me a coffee to help me get through the bumps — totally optional, no pressure.

- ☕ **[Buy me a coffee on Ko-fi](https://ko-fi.com/clintcan)**

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at your option. Being permissively licensed, a productized/notarized build may be sold commercially with support — that's selling the product, not a license exemption.

[IronRDP]: https://github.com/Devolutions/IronRDP
