# macrdp

[![Latest release](https://img.shields.io/github/v/release/clintcan/macrdp?sort=semver&label=release)](https://github.com/clintcan/macrdp/releases/latest)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)
[![Ko-fi](https://img.shields.io/badge/Ko--fi-buy%20me%20a%20coffee-FF5E5B?logo=ko-fi&logoColor=white)](https://ko-fi.com/clintcan)

A native RDP server for macOS, written in Rust on top of [IronRDP]. Connect from `mstsc`, Microsoft Remote Desktop, or FreeRDP to drive your Mac desktop with keyboard, mouse, real-cursor-shape forwarding, text + image clipboard sync, Mac↔Windows file copy, **read-write drive redirection** (mount the client's drives in Finder), **smart-card redirection** (use the client's smart card from macOS apps), system audio forwarding, and optional H.264 video (EGFX/AVC420, hardware-encoded). NLA/CredSSP is supported. Authenticates against your local Mac account via PAM.

This is the macOS equivalent of `xrdp`. Not a client, not a VNC bridge.

## Status

v0 — daily-driver usable on a trusted LAN. **Latest release: [v0.8.23](https://github.com/clintcan/macrdp/releases/latest)** — a **health-check watchdog** for unattended reliability. `launchd`'s `KeepAlive` only restarts macrdp on an outright *crash*; a process that's alive but **wedged** (a deadlocked async runtime, every worker thread blocked) would otherwise stay up and unreachable with nothing to revive it. The watchdog probes the runtime from a dedicated OS thread (which keeps ticking even when the runtime is stuck) and, on a sustained wedge, exits with a distinct code so `launchd` — or the `--fork-workers` supervisor — restarts a fresh process. Conservative by default (a wedge must persist ~90 s before a bounce, so load spikes never trip it), on when running headless under launchd, and env-tunable (`MACRDP_HEALTHCHECK*`); it closes the last reliability gap `KeepAlive` couldn't. Earlier: **automatic recovery from the mstsc EGFX reconnect-blank** (v0.8.22). A reconnecting `mstsc` that lands on its own stale retained surface (decodes every frame but never composites — the long-standing "black screen on reconnect with `--enable-h264`") is now **auto-detected and healed with no user action**. Detection reads the client's own QoE Frame Acknowledge `TimeDiffEDR` (decode+render time): a blank session reports it as zero on every frame while a rendering one shows nonzero within ~200 ms — a signal proven by decrypted-pcap comparison and verified live. On detection the server drops the connection (straight away by default — the non-destructive fresh-surface remap was verified never to heal `mstsc` and is now opt-in via `MACRDP_BLANK_RECOVERY_MAX_ATTEMPTS`); a **Server Auto-Reconnect Cookie** (MS-RDPBCGR ARC) then makes `mstsc` reconnect *itself* and the fresh connection renders, with a consecutive-drop cap so a truly-stuck client gets clear guidance instead of a reconnect loop. Default on (`MACRDP_BLANK_RECOVERY=0` / `MACRDP_AUTO_RECONNECT=0` to disable); pairs with `--fork-workers`. Earlier: the **congestion-responsive rate-control arc**: under packet loss an H.264 session now degrades gracefully instead of freezing. **`--adaptive-bitrate`** runs an AIMD controller (EWMA-smoothed frame-ack lag + hysteresis + a 3-zone hold) on **both** the UDP tunnel and the TCP path, so `--bitrate` is a ceiling that backs off under congestion; once it bottoms out, a **frame-rate floor** sheds fps (never to zero) so video stays choppy-but-steady-and-in-sync rather than freezing. An **EGFX-over-UDP watchdog** auto-de-migrates a wedged reliable UDP tunnel back to TCP (incl. proactively on minimize/restore). And **`--enable-lossy-audio`** streams RDPSND over a lossy UDP/DTLS tunnel with 1+1 redundancy (each datagram sent twice → p→p² loss) — soak-verified smooth on real mstsc at 5/10/15% loss where single-send glitches. All verified on real mstsc; every switch is opt-in, default (TCP) behavior unchanged. Earlier milestones: **`--enable-udp-multitransport`** (first known OSS RDP *server* with a working UDP data path) in v0.8.15; **`--fork-workers`** (per-connection worker processes, xrdp's model — reconnecting `mstsc` renders instead of going blank) in v0.8.13; `--map-ctrl-to-cmd` in v0.8.10; the generic-USB-redirection [feasibility writeup](docs/usb-redirection-feasibility.md) in v0.8.8; the visual [app-switcher HUD](#cli) (`--app-switcher-hud`) in v0.8.3. See [CLAUDE.md](CLAUDE.md) for what's wired up, what isn't, and known quirks.

## Production readiness

Short version: **a polished v0 daily-driver for trusted LANs — not an enterprise RDP server.** Use it to reach your own Mac over a network you control; don't put it on a public IP or treat it as multi-user/critical infrastructure.

**What's solid (verified on real mstsc / Microsoft Remote Desktop / FreeRDP):**
- Real auth — TLS + NLA/CredSSP against the macOS account via PAM, password from the Keychain. TLS can use a real CA / ACME / Let's Encrypt cert (`--cert`/`--key`), or the self-signed default. Per-IP connection **rate-limiting + lockout + an audit log** sit in front of the auth gate (on by default, loopback-exempt).
- The full daily workflow — display, keyboard/mouse (incl. non-US layouts, Cmd+Tab, optional Ctrl→Cmd), clipboard text/images/files both ways, system audio, drive + smart-card redirection, headless virtual displays.
- H.264/EGFX with **congestion-responsive rate control** — under packet loss it degrades gracefully (bitrate backs off → fps sheds at the floor → stays choppy-but-in-sync) instead of freezing, and audio can ride a loss-resilient lossy-UDP path.
- Deployable — signed + notarized `.app`, a LaunchAgent, and a menu-bar GUI controller; TCC grants survive rebuilds. A **health-check watchdog** bounces a hung-but-alive process so launchd restarts it (not just on crash).
- Tested — 130+ unit tests + a regression harness, run in CI on every push.

**Known limitations — read before relying on it:**
- **Trusted-LAN scope is load-bearing.** TLS supports a real CA / ACME cert (`--cert`/`--key`) and there's now per-IP connection rate-limiting + lockout + an audit log — but it still isn't hardened for full internet exposure or hostile networks. Even with all that, **put internet-facing RDP behind a VPN or an RD Gateway** — that's the production answer for any RDP server, not just this one.
- **Single session, single user.** No multi-monitor (client-side multi-display) and no printer redirection.
- **Some content can't be captured, by OS design** — DRM video (Netflix etc.) and password-manager vault windows render blank; macOS excludes protected content from screen capture and that can't (and shouldn't) be overridden.
- **Synthetic input can't reach secure contexts** — the login window, lock screen, and secure-input password fields are OS-blocked.
- **Client quirks, documented not fixed** — reconnecting *mstsc* to a still-running server can show a blank screen (`--fork-workers` largely fixes it; residual ~1/7 recovers by reconnecting once more); FreeRDP/Thincast are unaffected.
- **The UDP multitransport / lossy-audio paths are EXPERIMENTAL** (opt-in, default OFF) — robust in testing but newer and less soaked than the TCP core, which remains the default everything rides on.
- **It's a solo v0** built on vendored [IronRDP](https://github.com/Devolutions/IronRDP) forks — no commercial support or SLA.

If your use case is "remote into my own Mac over my LAN/VPN," it's in good shape. If it's unattended production, untrusted networks, or multi-user, it isn't there yet — see [`docs/production-readiness-roadmap.md`](docs/production-readiness-roadmap.md) for what would close that gap (real TLS certs ✓ done; auth hardening ✓ done; a multi-day soak — foundation core passed a 31 h leak/drift run, full 48–72 h on the latest build still open) and what can't be (multi-user GUI sessions, on macOS).

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

`dist/install.sh` above installs a **bare binary**. If you'd rather have a
proper **signed `macrdp.app`** — a stable bundle identity at a fixed path (so
TCC grants survive rebuilds), an `LSUIElement` background agent, and the
**embedded smart-card IFD handler** + its installer — build it with `packaging/`.
(A [GitHub release](#release-artifacts) already includes an ad-hoc-signed `.app`;
build locally when you want a Developer-ID-signed + notarized one, or the
menu-bar controller app, which CI doesn't produce.)

```bash
packaging/make-app.sh                                 # build + sign + install to /Applications
security add-generic-password -s macrdp -a "$(id -un)" -w 'YOUR_PASSWORD'
packaging/install-launchagent.sh                      # load LaunchAgent (label com.clintcan.macrdp)
```

`make-app.sh` does the whole thing: builds the release binary **and** the
`ifd-handler` cdylib, assembles `macrdp.app`, embeds `ifd-macrdp.bundle` (the
smart-card IFD handler) plus `install-ifd-handler.sh` under `Contents/Resources/`,
co-signs everything, and installs to `/Applications`.

**Signing.** By default it **ad-hoc signs** (local use only — fine for your own
Mac). For a build you can distribute, sign with your Developer ID and notarize:

```bash
CODESIGN_IDENTITY="Developer ID Application: Your Name (TEAMID)" \
  NOTARIZE=1 NOTARY_PROFILE=macrdp-notary \
  packaging/make-app.sh
```

(`NOTARY_PROFILE` is a `notarytool` keychain profile you set up once with
`xcrun notarytool store-credentials`.) Override the bundle identifier with
`BUNDLE_PREFIX=com.acme`.

**Distribution DMG.** To wrap the signed app(s) into a styled, signed +
notarized `.dmg`:

```bash
NOTARIZE=1 NOTARY_PROFILE=macrdp-notary packaging/make-dmg.sh
```

**Smart-card redirection** needs one extra privileged step after the app is
installed — the IFD handler has to be copied into a root-owned system directory.
Run the embedded installer once (one GUI admin prompt):

```bash
/Applications/macrdp.app/Contents/Resources/install-ifd-handler.sh
```

See [Smart-card redirection](#smart-card-redirection) for the USB-trigger caveat
and verification.

**Config.** Toggle features (H.264/AAC/HiDPI), bind address, and an `EXTRA_FLAGS`
escape hatch live in `~/Library/Application Support/macrdp/config.env` — outside
the bundle, so edits never disturb the signature or the TCC grants. The two
auto-start paths (LaunchAgent vs the controller app) are **mutually exclusive**
(both bind `:3390` and share the `macrdp` Keychain entry) — pick one. See
[packaging/README.md](packaging/README.md) for the full guide (icons, controller
app, per-script options, TCC notes).

## Release artifacts

Pushing a `v*` tag runs the [release workflow](.github/workflows/release.yml),
which builds on an Apple-Silicon runner and attaches these to a draft GitHub
Release (Apple Silicon / `aarch64-apple-darwin` only):

| File | What it is |
|------|------------|
| `macrdp-<ver>-aarch64-apple-darwin.tar.gz` | the **bare CLI binary** + `LICENSE`/`README` |
| `macrdp-<ver>-aarch64-apple-darwin-app.zip` | the full **`macrdp.app`**, with the embedded smart-card IFD handler (`ifd-macrdp.bundle`) + `install-ifd-handler.sh` — the only artifact that carries everything `--enable-smartcard-redirection` needs |
| `SHA256SUMS` | checksums for both |

Both are **ad-hoc signed, not notarized** — macOS Gatekeeper shows a "can't
verify developer" prompt, so open the app once via **right-click → Open** (or
`xattr -dr com.apple.quarantine macrdp.app`). For a Developer-ID-signed +
notarized build, or the menu-bar **controller** app (neither is produced in CI),
build locally with [`packaging/make-app.sh`](#building-the-full-app).

## CLI

```
--bind 0.0.0.0:3390       Listen address (3390 by default; 3389 needs root)
--username NAME           Defaults to $USER
--password PASS           Skip the interactive prompt
--skip-auth               Bypass PAM (testing only)
--keychain                Read password from macOS Keychain (service=macrdp)
-v, --verbose             Show all the noisy logs the default filter hides
--allow-sleep             Let the Mac sleep / auto-lock normally (default
                          is to spawn `caffeinate` so an idle Mac doesn't
                          drop the connection mid-session)
--width / --height        Override autodetected display size
--hidpi                   Capture the primary display at backing (Retina) pixel
                          resolution instead of logical points (e.g. 3024×1964
                          vs 1512×982) for crisp native pixels. ~4× the pixels;
                          best with --enable-h264. Ignored with --width/--height
                          or --virtual-display. See "Display". macOS-only.
--no-client-resolution    Serve the Mac's native size instead of adopting the
                          resolution the client requests at connect. By default,
                          when mirroring without --width/--height/--hidpi/
                          --virtual-display, macrdp serves exactly what the client
                          asks for (so mstsc presents 1:1, no client-side rescale).
                          Pass this to serve native and let the client scale.
--stretch                 On the auto-size path, stretch the Mac screen to fill
                          the client frame. By default, when the client's
                          resolution has a different aspect ratio than the Mac,
                          macrdp preserves the Mac's aspect ratio with black bars
                          (letterbox/pillarbox) and maps mouse input into the
                          centered picture. Pass this for the old fill-and-distort
                          behavior. No effect with --width/--height or at a
                          matching aspect ratio. See "Display".
--unminimize-on-switch    On Cmd+Tab, un-minimize the target app's window (bring
                          it back from the Dock) instead of just activating the
                          app. Off by default (matches native macOS, which leaves
                          a minimized window minimized). macOS-only.
--alt-tab-switch          Also accept Option+Tab (Alt+Tab from the client) as an
                          app-switch trigger, in addition to Cmd+Tab. Off by
                          default. For clients/configs that forward Alt+Tab but
                          gate Win+Tab (e.g. mstsc's "Apply Windows key
                          combinations" when windowed). Option+Shift+Tab cycles
                          backward. macOS-only.
--app-switcher-hud        Show a visual app-switcher overlay (icon row, like
                          macOS's native Cmd+Tab) on the remote during Cmd+Tab /
                          Option+Tab. Off by default. macrdp spawns a small helper
                          that draws a real on-screen panel, which ScreenCaptureKit
                          captures — so the client sees it. The switch behaves the
                          same with or without it. macOS-only.
--map-ctrl-to-cmd         Remap Windows editing shortcuts from Ctrl to Cmd so
                          Windows muscle memory works on macOS: Ctrl+C/V/X/A/Z/S/
                          F/N/T/W/O/P/R/G (and Shift variants, e.g. Ctrl+Shift+Z =
                          redo) fire as the Cmd equivalent. Off by default (then
                          Ctrl reaches remote apps unchanged, so macOS shortcuts
                          need the client's Win/Super key). Cmd+Q is never produced
                          (Q excluded); nav keys untouched. Always suppressed when a
                          terminal is frontmost so Ctrl+C stays SIGINT. macOS-only.
--no-remap-apps LIST      Comma-separated bundle ids where --map-ctrl-to-cmd is
                          suppressed, on top of the built-in terminal list. For
                          apps with an embedded terminal that can't be auto-detected
                          (front app is the IDE, not a TTY), e.g.
                          --no-remap-apps com.microsoft.VSCode. macOS-only.
--keyboard-layout SPEC    Force a keyboard layout for non-US clients instead of
                          auto-detecting it from the client. By default the
                          layout is auto-detected from the client's announced
                          KLID (US/unknown keep the positional keycode path);
                          pass a name (`french`, `de`, `azerty`), a Windows KLID
                          (`0x040C`), or a macOS input-source id to force one, or
                          `none` to disable translation. Keys are translated via
                          UCKeyTranslate and posted as Unicode; the Mac's own
                          input source is untouched. macOS-only.
--fps N                   Frame rate cap (default 15, or 60 with --enable-h264
                          — see "Video" for why H.264 wants the higher rate)
--enable-h264             Stream the display as H.264 over EGFX (AVC420),
                          hardware-encoded via VideoToolbox, instead of legacy
                          bitmaps. Falls back to legacy automatically for
                          clients that don't negotiate H.264. See "Video".
--bitrate N               Target H.264 bitrate in Mbps (default 6; only with
                          --enable-h264). Raise it (8–12) for sharper detail if
                          you have bandwidth headroom.
--keyframe-interval SECS  H.264 periodic keyframe (IDR) interval in seconds
                          (default 2; only with --enable-h264). Safety net for
                          transient decode glitches; fractional values OK.
--keyframe-on-change      Force on-change H.264 keyframes (OFF by default; only
                          with --enable-h264): an IDR on large changes (window-
                          to-front, scroll, app launch) and briefly after a click.
                          The periodic interval + flush-burst already cover this,
                          so enable it only if big updates lag. See "Video".
--flush-frames N          Trailing frames re-sent after each change to drain
                          mstsc's presentation buffer (default 4; only with
                          --enable-h264). Stops the last keystroke before a pause
                          lagging until the next keyframe. 0 disables. See "Video".
--enable-aac              Compress system audio as AAC-LC over RDPSND
                          (WAVE_FORMAT_AAC_MS) instead of raw PCM — ~11x less
                          audio bandwidth. Clients that don't decode AAC fall
                          back to PCM automatically. Off by default (adds
                          ~40–50 ms latency). macOS-only. See "Audio".
--aac-bitrate BPS         AAC target bitrate in bits/sec (default 128000; only
                          with --enable-aac). 96000 saves the most bandwidth,
                          192000 is near-transparent.
--no-lazy-paste           Opt out of lazy Windows→Mac file paste (default ON).
                          With lazy, temp files are pre-sized but empty when the
                          copy lands and stream bytes only on Cmd-V, with macOS's
                          native "Preparing to paste" progress dialog. Pass this
                          to fall back to the eager path (downloads everything
                          on copy, auto-fires Cmd-V into Finder when done).
                          See "Windows → Mac file copy" below.
--enable-drive-redirection  Let the connecting client redirect its local
                          drive(s) (mstsc: Local Resources → Drives; FreeRDP:
                          /drive:NAME,PATH); the Mac mounts each as a real
                          read-write volume in Finder (in-process NFS + built-in
                          mount_nfs, no root/kext/FUSE). Off by default. See
                          "Drive redirection" below. macOS-only.
--enable-smartcard-redirection  Let the connecting client redirect its
                          smart-card reader (mstsc: Local Resources → More →
                          Smart cards; FreeRDP: /smartcard) so macOS apps can use
                          the card through it (MS-RDPESC). Off by default.
                          Requires installing the PC/SC IFD handler once + a USB
                          trigger device — see "Smart-card redirection" below.
                          macOS-only.
--no-mute-on-minimize     Opt out of muting audio while the client window is
                          minimized (default ON). When the client sends the
                          standard `SuppressOutput` PDU on minimize, the server
                          stops emitting Wave PDUs so the client's audio queue
                          drains naturally; on refocus, audio resumes in sync
                          with the freshly IDR'd video. Pass this to keep audio
                          flowing through a minimize (preserves "minimized
                          YouTube keeps playing on the Mac speakers") at the
                          cost of accepting that drift on refocus. See "Audio"
                          below.
--qoi-force-rgb           Force QOI BitmapUpdates to emit `Channels::Rgb` instead
                          of the natural `Channels::Rgba` mapping from a *A32
                          capture. Default OFF (matches upstream ironrdp-server).
                          Only matters if you connect with an IronRDP-based viewer
                          built against ironrdp-session WITHOUT the RGBA decode
                          patch (currently every published release, until
                          Devolutions/IronRDP#1341 lands) — without this flag those
                          viewers will render blank with one "Unsupported RGBA QOI
                          data" warning per frame. mstsc / Microsoft Remote Desktop
                          / Windows App / FreeRDP don't advertise QOI and are
                          unaffected either way.
--cert-dir PATH           Persisted self-signed TLS cert (default ~/Library/Application Support/macrdp)
--cert PATH / --key PATH  Operator-supplied TLS cert + key (PEM) — serve a real CA / ACME /
                          Let's Encrypt cert instead of self-signed. Both required; no
                          silent self-sign fallback. Config: TLS_CERT / TLS_KEY.
--log-dir PATH            Directory for the rotating log file (macrdp.log + N
                          logrotate-style archives). Defaults to ~/Library/Logs when
                          headless (e.g. under the LaunchAgent), or stdout when
                          interactive. Size-bounded; see MACRDP_LOG_MAX_BYTES /
                          MACRDP_LOG_MAX_FILES below. Config: LOG_DIR.
--virtual-display         Serve a headless virtual display at --width × --height
                          instead of mirroring the primary panel — local screen
                          stays untouched. Requires --width and --height.
--make-primary            Promote the virtual display to system primary (the one
                          with the menu bar). Only valid with --virtual-display.
--detach-primary          While a client is connected, disable every physical
                          display (backlights off, no menu bar). Restored on
                          disconnect / exit. Only with --virtual-display.
--capture-primary         Alternative to --detach-primary: exclusive
                          CGDisplayCapture of every physical display, then
                          gamma-clamp to black. Panels stay backlit but render
                          solid black. Use when --detach-primary doesn't
                          actually blank the panel on your hardware. Mutually
                          exclusive with --detach-primary. Only with
                          --virtual-display.
--fork-workers            EXPERIMENTAL, opt-in. Fork a fresh worker process per
                          connection (xrdp's model) so reconnecting mstsc to a
                          still-running server renders instead of going blank.
                          See Known limitations (Video) below. macOS-only; off
                          by default.
--enable-udp-multitransport  EXPERIMENTAL, opt-in. Offer RDP UDP multitransport
                          (MS-RDPEMT over reliable RDPEUDP) and bind a UDP
                          listener on the same address/port as TCP. On its own,
                          EGFX still rides TCP (a proven safe spike) — add
                          --udp-migrate-egfx to move the video. Input/audio/
                          clipboard always ride TCP. As far as is known, the
                          first OSS RDP *server* with a working UDP data path.
                          Not supported under --fork-workers. Off by default.
--udp-migrate-egfx        EXPERIMENTAL, opt-in (needs --enable-udp-multitransport).
                          Migrate the H.264/EGFX video channel onto the reliable
                          UDP tunnel via MS-RDPEDYC Soft-Sync (verified on mstsc).
                          Clean-link optimal: the reliable tunnel is an ordered
                          stream, so under packet loss it head-of-line-blocks
                          like TCP — but an auto-recovery watchdog de-migrates
                          EGFX back to TCP on a wedge (no more freeze-until-
                          reconnect; audio always rode TCP). Off by default.
--adaptive-bitrate        Opt-in. Congestion-responsive H.264 rate control on
                          both the UDP tunnel and the TCP path (only with
                          --enable-h264): an AIMD controller reads the client's
                          frame-ack lag (EWMA-smoothed) + retransmits and live-
                          adjusts the encoder bitrate within [floor, --bitrate
                          ceiling] — backs off under congestion, climbs back when
                          clear. So --bitrate becomes a ceiling, not a fixed
                          target: set it high (e.g. 8) and let it adapt. Off by
                          default.
--enable-lossy-audio      EXPERIMENTAL, opt-in (implies --enable-udp-multitransport;
                          needs --enable-aac + --enable-h264). Stream RDPSND audio
                          over a LOSSY UDP/DTLS tunnel instead of TCP — the loss-
                          resilient audio path. AAC Wave2 data rides a lossy
                          RDPEUDP flow (deliver-on-arrival, no retransmit) and each
                          datagram is sent TWICE (the client's DTLS anti-replay
                          dedups), so an independent-loss link of rate p drops a
                          payload only at p^2. Verified smooth on mstsc at
                          5/10/15% loss where single-send glitches. Off by default.
```

`RUST_LOG=debug` for verbose logging.

### Auth hardening (environment variables, on by default)

In front of the NLA/CredSSP gate, macrdp rate-limits and (briefly, escalating) locks out
source IPs that hammer the port, and writes a per-connection audit line. **Loopback
(`127.0.0.1`/`::1`) is always exempt**, so this only bites when `--bind` exposes the server
to other hosts — you can't lock yourself out locally. The defaults are conservative; these
are env-only (no CLI flags) and can be set via `config.env` (the matching keys are shown) or
the LaunchAgent plist's `EnvironmentVariables`:

```
MACRDP_CONN_GUARD=1               # master switch (0/off = disable rate-limit + lockout)   [config.env: CONN_GUARD]
MACRDP_AUDIT_LOG=1                # connection audit log (independent of the guard)          [AUDIT_LOG]
MACRDP_GUARD_RL_MAX=10            # max attempts per window per IP (0 = no rate-limit)        [GUARD_RL_MAX]
MACRDP_GUARD_RL_WINDOW_SECS=60    # rate-limit sliding window                                 [GUARD_RL_WINDOW_SECS]
MACRDP_GUARD_FAIL_THRESHOLD=5     # consecutive failures before lockout (0 = no lockout)      [GUARD_FAIL_THRESHOLD]
MACRDP_GUARD_FAILFAST_SECS=3      # only errored connections that fail this fast (pre-handshake) count toward lockout  [GUARD_FAILFAST_SECS]
MACRDP_GUARD_COOLDOWN_BASE_SECS=30  # first lockout length, doubles per extra failure          [GUARD_COOLDOWN_BASE_SECS]
MACRDP_GUARD_COOLDOWN_MAX_SECS=900  # lockout escalation cap (15 min)                          [GUARD_COOLDOWN_MAX_SECS]
```

The lockout **escalates and auto-expires** (no manual unlock): with the defaults it triggers
at the 5th consecutive failure, then 30s → 60 → 120 → 240 → 480 → 900s (cap) as the IP keeps
failing past each cooldown; **a clean session — or any connection that got past the handshake
— resets that IP to 0**. It's heuristic: only a connection that errored *and* failed within
the fail-fast window (~3s, i.e. never authenticated) counts as a failure, so a reconnecting
real client (mstsc reconnect-blank, flaky link) and a single benign disconnect (mstsc's
first-connect cert-prompt "Broken pipe") never lock you out. Audit lines are
tagged `macrdp::audit`: `grep 'macrdp::audit' ~/Library/Logs/macrdp.log` shows
`event="accept|reject|disconnect"` with the source IP and (for rejects) the reason and
retry-after.

(Log rotation is likewise env-tunable: `MACRDP_LOG_MAX_BYTES` (default 10 MiB) and
`MACRDP_LOG_MAX_FILES` (default 5); see `--log-dir`.)

## Headless mode

`--virtual-display --width W --height H` allocates a headless display via undocumented `CGVirtualDisplay*` private API and serves it over RDP instead of mirroring the Mac's panel. Behaves like plugging in an external monitor — the remote session gets its own desktop at the requested resolution, and you keep using the Mac locally as normal. Add `--make-primary` to give the virtual display the menu bar so new app windows open there.

To go *fully* headless while a client is connected, pick one:

- **`--detach-primary`** — turns the backlight off on every built-in / external panel via `CGSConfigureDisplayEnabled`. Cleanest visually. On some macOS versions / displays the disable transaction succeeds but the panel keeps showing the desktop; if you hit that, switch to:
- **`--capture-primary`** — takes exclusive `CGDisplayCapture` of every physical display and forces the gamma LUT to map every input to black. Backlight stays on but panels render solid black. Works everywhere capture is allowed; uses only public CG symbols.

Both restore the original layout when the last client disconnects, and both auto-revert on `SIGKILL` / panic (no logout required). Pick `--detach-primary` first; fall back to `--capture-primary` if your hardware doesn't honor the disable.

## Examples

```bash
# Default — loopback only, mirror primary panel, prompt for password.
./macrdp

# Accept LAN connections, force a non-$USER account.
# NETWORK EXPOSURE: --bind 0.0.0.0 opens the port to your LAN. Keep it on a
# network you control. Do NOT port-forward / expose it to the public internet —
# even with TLS + NLA/CredSSP + rate-limit/lockout, the production answer for any
# RDP server is to reach it over a VPN or an RD Gateway, never a raw public IP.
./macrdp --bind 0.0.0.0:3390 --username clint

# Higher frame rate, custom cert dir.
./macrdp --fps 30 --cert-dir ~/.macrdp-certs

# H.264 video over EGFX — much lower bandwidth AND crisper than legacy bitmaps.
# Recommended for mstsc / Microsoft Remote Desktop / FreeRDP / Thincast: the
# default legacy bitmap path looks grainy on those clients by comparison.
./macrdp --enable-h264

# Make Windows shortcuts (Ctrl+C/V/X/…) drive macOS copy/paste; keep VSCode's
# integrated terminal on real Ctrl.
./macrdp --map-ctrl-to-cmd --no-remap-apps com.microsoft.VSCode

# Verbose logs (DEBUG level).
./macrdp -v

# Headless virtual display at 1440p — local Mac screen stays available.
./macrdp --virtual-display --width 2560 --height 1440

# Same, but the virtual display owns the menu bar (drive it as your main desktop).
./macrdp --virtual-display --width 2560 --height 1440 --make-primary

# Fully headless on connect: physical panels go dark, revived on disconnect.
./macrdp --virtual-display --width 2560 --height 1440 --detach-primary

# Same idea, for hardware where --detach-primary doesn't actually blank the panel.
./macrdp --virtual-display --width 2560 --height 1440 --capture-primary

# Non-interactive launch (used by dist/install.sh): password from Keychain.
./macrdp --keychain

# Quick dev test on loopback — skips PAM, accepts --password verbatim.
./macrdp --skip-auth --password test

# Use the eager Windows→Mac file paste path (default is lazy / on-demand).
./macrdp --no-lazy-paste
```

## Display resolution (`--hidpi`)

By default macrdp captures and advertises the Mac's **logical** resolution — the points it reports in System Settings (e.g. 1512×982 on a default-scaled 14" MacBook). On a Retina panel that's half the physical pixels, so any client whose window is larger upscales it and text looks soft.

Pass **`--hidpi`** to capture at the display's **backing (Retina) pixel resolution** instead (e.g. 3024×1964) — clients then render crisp native pixels. It's **opt-in** because it's ~4× the pixels:

- **Pair it with `--enable-h264`.** H.264 compresses the higher resolution cleanly and the client downscales it sharply — that's the real "Retina remote desktop" experience. On the legacy bitmap path it just means 4× the bandwidth.
- **mstsc feels laggy at HiDPI.** mstsc decodes 4× the pixels every frame and its ~2-frame presentation buffer now holds 4×-bigger frames, so responsiveness drops. **Thincast / FreeRDP stay snappy** — their H.264 decoders keep up. The server itself isn't the bottleneck (it encodes a 3024×1964 frame in ~10 ms, well inside the 60fps budget); the cost is client-side decode. Prefer a capable client if you want HiDPI.
- Ignored when you pass explicit `--width`/`--height` (you've chosen the size) or with `--virtual-display` (already an explicit resolution).

Input and cursor are resolution-correct at any setting — clicks land precisely and the pointer stays normal-sized.

### Aspect ratio (auto-size path)

By default macrdp serves exactly the resolution the connecting client requests (e.g. mstsc full-screen on a 1920×1080 monitor gets a 1920×1080 session). When that resolution's aspect ratio differs from the Mac's panel (e.g. a 16:9 client against a 16:10 MacBook), macrdp **preserves the Mac's aspect ratio and adds black bars** (letterbox top/bottom or pillarbox left/right) so the picture isn't distorted, and maps mouse input into the centered picture so clicks stay accurate. Verified: a 1512×982 Mac served to a 1920×1080 client produces a centered 1663×1080 image with 128 px bars each side.

Pass **`--stretch`** to instead fill the whole frame (the old behavior) — no bars, but the image is non-uniformly scaled on an aspect mismatch (e.g. ~13.5% vertical compression for 16:10→16:9). `--stretch` has no effect when the aspect already matches, or with explicit `--width`/`--height` (those always stretch). Either way, serving a non-native resolution forces full-frame updates (higher bandwidth) and, on **mstsc with `--enable-h264`**, the scaling amplifies its trailing-frame presentation lag — a Mac whose native resolution already matches the client (no scaling) is snappier. See "Video".

## Video (H.264)

By default the display is sent as legacy bitmaps (RemoteFx/QOI to mstsc, NSCodec/raw to others) — works everywhere, but bandwidth-heavy. Pass **`--enable-h264`** to stream the desktop as **H.264 over the EGFX virtual channel** (MS-RDPEGFX, AVC420), hardware-encoded with VideoToolbox. Far less bandwidth, especially for video/scrolling/photos.

How it behaves:

- **Automatic fallback.** Clients that don't advertise H.264 (AVC420) decode — e.g. a FreeRDP build without an H.264 decoder — transparently fall back to legacy bitmaps. No need to match the flag to the client. mstsc, FreeRDP-with-H.264, and the macOS **Windows App** / Microsoft Remote Desktop client all decode the H.264 stream.
- **Wire format.** The AVC420 payload is Annex-B framed (what Microsoft's decoder expects). The bitstream is verified rendering on `mstsc` and on FreeRDP built with H.264 (e.g. the [Thincast client]).
- **Bitrate.** `--bitrate N` sets the target encoder bitrate in megabits/sec (default `6`, only meaningful with `--enable-h264`). Raising it sharpens detail but grows each frame, so the big per-frame writes are more likely to fill the socket buffer and delay audio on a constrained link — `6` is a good balance; try `8`–`12` if you have headroom.
- **Color.** The stream is encoded as full-range BT.709. This matters for `mstsc`, which reads AVC420 luma as full-range regardless of the bitstream flag — video-range output otherwise renders washed-out / lighter there. FreeRDP honors the flag and is correct either way. To get full range we convert each captured BGRA frame to full-range NV12 ourselves (VideoToolbox would otherwise emit video-range from a BGRA source); that conversion is **vImage**-accelerated — see [Color conversion: scalar vs vImage](#color-conversion-scalar-vs-vimage).
- **Frame rate.** `--enable-h264` defaults to **60fps** (vs 15 for legacy). mstsc holds a fixed ~2-frame presentation buffer for the H.264 stream, so at 30fps typing lags ~2 keystrokes (~66ms) while at 60fps that buffer is ~33ms and feels immediate. FreeRDP-based clients don't buffer this way and are snappy at any rate. Set `--fps` explicitly to override (lower it to save CPU/bandwidth if your client/link doesn't need 60).
- **Keyframes.** A keyframe (IDR) is forced on the first frame, then periodically every `--keyframe-interval` seconds (default `2`) as a safety net — some clients (mstsc) only fully recover a transient decode glitch on the next IDR, so a long interval leaves garbled regions (notably text) lingering. Lower it for faster recovery at the cost of bandwidth/quality; raise it for smoother typing. Optionally, pass **`--keyframe-on-change`** (off by default) to additionally force an IDR whenever a large area changes at once (window-to-front, scroll, app launch) and briefly after a mouse click, so big updates land immediately instead of waiting for the periodic interval (rising-edge detection keeps sustained churn like video from forcing an IDR every frame). It's off by default because the periodic interval plus the trailing flush-burst (`--flush-frames`) already drain mstsc's presentation buffer, so the extra forced IDRs mostly just spend bitrate/quality at a fixed bitrate for no typing benefit — enable it only if large updates visibly lag on your client/link. When enabled, the trigger thresholds are tunable: `--keyframe-change-pct` (default 20, the dirty-area % that fires an IDR), `--keyframe-click-pct` (default 5, the lowered threshold after a click), and `--keyframe-click-window-ms` (default 400, how long that lowered threshold lasts).
- **Flush frames (`--flush-frames`, default `4`).** ScreenCaptureKit only delivers a frame when the screen changes, so after the last keystroke before a pause there are no further frames to push it through mstsc's ~2-frame AVC420 presentation buffer — it would strand there until the next change or periodic keyframe (the classic "typing follows the keyframe" lag). After each change the server re-submits the last frame this many times as cheap skip-P-frames, draining the buffer so the change appears within a couple of frame intervals (~33 ms at 60fps), then goes quiet. mstsc needs ≥2; raise if a slight trailing lag remains, or set `0` to disable.

### Known limitations

- **Reconnecting `mstsc` to a still-running macrdp can show a black screen** (with a live cursor). This is an mstsc-specific quirk: it retains EGFX surfaces for the lifetime of its process and mis-composites on reconnect. It is *not* a server bug — FreeRDP reconnects cleanly over the same stream. **Two ways to handle it:**
  - **`--fork-workers`** (experimental, opt-in, `FORK_WORKERS=1` in `config.env`, or the GUI controller's "Per-connection workers" toggle / "Set Up Remote Desktop" preset): adopts xrdp's process model — a thin supervisor forks a *fresh worker process per connection*, and a brand-new process dodges mstsc's surface retention, so reconnect renders. A rare residual blank (~1 in 7) clears by **reconnecting once more** (no need to close the window). The supervisor owns the persistent state (virtual display, headless blanking, caffeinate, app-switcher HUD); works mirror-primary or with `--virtual-display`. Smart-card redirection works under it too (the `:40242` bridge re-binds per worker — verified end-to-end including reconnect). macOS-only.
  - **Or just close + reopen the mstsc window** — quitting the client clears its surface cache, so the desktop renders every time, no Windows reboot needed. (This was the only recovery before `--fork-workers`; a server-side fresh-surface-id workaround was tried earlier but only mitigated it unreliably, so it was dropped.)
- H.264 is **macOS-only** (VideoToolbox) and still maturing — bitrate and keyframe behavior are tunable (above), but dirty-region *encoding* is not yet done: every frame is a full encode (dirty rects are used only to time on-demand keyframes, not to encode sub-regions). H.264's own inter-prediction keeps unchanged regions cheap regardless.

### Color conversion: scalar vs vImage

*(Implementation detail — skip unless you're profiling CPU or porting the encoder.)*

VideoToolbox, given a BGRA source, emits **video-range** YUV (luma 16–235). `mstsc` reads AVC420 luma as **full-range**, so that looks washed out (see **Color** above). The fix is to hand VideoToolbox a YUV buffer that's already full-range, which means doing the BGRA → full-range BT.709 NV12 (`420f`) color conversion ourselves, once per captured frame, on the capture thread.

That conversion is a real per-frame cost, so it's done with **vImage** (Apple's Accelerate framework), which runs the RGB→Y'CbCr math on the CPU's vector units (NEON on Apple Silicon). A scalar reference implementation (a plain Rust loop) is kept as well: it's the fallback for any frame vImage declines (e.g. odd dimensions), the oracle the vImage path is unit-tested against, and the baseline below. Both produce identical output (within ±1 rounding).

Single-thread cost per frame, Apple M3 (`cargo test --release bench_nv12_full_range -- --ignored --nocapture`):

| Resolution | scalar | vImage | speedup |
|---|---:|---:|---:|
| 1470×956 | 3.36 ms | 0.12 ms | ~29× |
| 1920×1080 | 4.98 ms | 0.16 ms | ~32× |
| 2560×1440 | 8.88 ms | 0.33 ms | ~27× |
| 3840×2160 (4K) | 20.0 ms | 0.84 ms | ~24× |

At 60fps the frame budget is 16.67 ms. The scalar path is fine at 1080p (~30% of one core) but **exceeds the budget at 4K**, where it would cap the achievable frame rate before the encoder even runs; vImage keeps the conversion at ~1% of budget across the board, so it's never the bottleneck. The implementation lives in `src/videotoolbox.rs` (`bgra_to_nv12_full_range_vimage`, with `bgra_to_nv12_full_range` as the scalar reference).

## Audio

System audio rides over the RDPSND virtual channel as 16-bit stereo PCM at **44.1 kHz**. ScreenCaptureKit only supports 8 / 16 / 24 / 48 kHz, so the capture loop captures at 48 kHz and resamples to 44.1 with [`rubato`](https://github.com/HEnquist/rubato) before sending. 44.1 matches the native rate of most Windows audio endpoints, which avoids the client-side resampling drift that otherwise accumulates into multi-second audio backlogs. A generation counter on the audio factory keeps a client reconnect from leaving a second capture loop feeding the channel. The vendored `ironrdp-server` carries a single patch that makes `dispatch_server_events` keep the *newest* queued waves on per-batch overflow instead of the oldest — without it, a one-off video-encode stall would bake a permanent audio-latency offset into the session.

The capture loop also **self-heals a dead SCK audio stream**: over a long session ScreenCaptureKit can stop delivering samples or transiently fail to start, which previously left the connection silent for the rest of the session (video is a separate stream and kept running). The loop now rebuilds the audio `SCStream` with capped exponential backoff (250 ms → 5 s) on both start failures and mid-stream end, resetting the backoff once a sample arrives; the generation guard still retires it on reconnect, so there's no double-capture.

**AAC compression** (opt-in, `--enable-aac`). By default audio is uncompressed PCM (~1.4 Mbit/s). Pass `--enable-aac` to encode it as **AAC-LC** over RDPSND (`WAVE_FORMAT_AAC_MS`, ~128 kbps by default — about 11x smaller), which matters over WAN or constrained links. The encoder is AudioToolbox (software AAC-LC); the wire payload is raw AAC access units. The server advertises AAC ahead of PCM, so clients that decode it (mstsc, Microsoft Remote Desktop / Windows App, FreeRDP built with AAC support) negotiate AAC automatically while clients without it fall back to PCM transparently. It's off by default because AAC adds ~40–50 ms of encoder priming latency — on a LAN, PCM's zero added latency is the better default. Tune the bitrate with `--aac-bitrate` (default `128000`; `96000` saves the most bandwidth, `192000` is near-transparent for music).

**Mute on minimize** (default-on, opt out with `--no-mute-on-minimize`). When the client minimizes its window it sends the standard `SuppressOutput { None }` PDU; the server stops emitting both EGFX video frames and RDPSND waves until the client refocuses (`RefreshRectangle` / `SuppressOutput { Some(rect) }`). Without this, mstsc accumulates a backlog of video frames + audio waves during a long minimize that has to chew through on refocus, producing several seconds of input lockout, audio drift, and a video catch-up storm. With it, you get a brief audio gap on refocus and audio + video resume in sync. Both gates are debounced (1 s) so transient `SuppressOutput` flaps mstsc emits under wire pressure (e.g., during a heavy local `cargo build`) don't oscillate the mute and cause stutter. Pass `--no-mute-on-minimize` if you specifically want audio to keep playing while the client window is minimized — accepting that audio will drift by however long was spent minimized.

## File copy

Bidirectional via MS-RDPECLIP. Both directions support single files and folder trees.

**Mac → Windows.** `Cmd-C` a file or folder in Finder, `Ctrl-V` in Windows Explorer. The pasteboard walk recurses into directories (skipping symlinks, capped at 10 000 descriptors per copy) and emits the right `relative_path` so Explorer reconstructs the tree. Bytes stream on demand via `FileContentsRequest` chunks (4 MiB per chunk). Windows shows its native "Copying…" progress dialog.

**Windows → Mac (lazy, default).** `Ctrl-C` in Explorer, `Cmd-V` in Finder. The server pre-allocates an empty temp file per leaf at its declared size and registers each one with `NSFileCoordinator` via `NSFilePresenter`. Bytes only start streaming when Finder asks for them on `Cmd-V`, and macOS shows its **native "Preparing to paste" progress dialog** during the wait. Folder trees and multi-file selections both work. Lower chunk parallelism is used than the eager path so the RDP session stays responsive (mouse / keyboard / video) while a multi-hundred-MB paste is in flight. If you'd rather have files downloaded eagerly the moment Windows announces a copy (and `Cmd-V` auto-fired into Finder when ready, with an audible Glass-chime cue), pass `--no-lazy-paste`.

### Known limitations

- **`Ctrl-C` on a *folder* in Windows Explorer doesn't reach the Mac.** Explorer puts only the Shell IDList format on the clipboard and delay-renders `FileGroupDescriptorW`, which `mstsc` doesn't request — so nothing is forwarded over the RDP clipboard channel and you'll hear a beep on `Cmd-V`. Windows + mstsc behavior, not fixable server-side. **Workaround:** open the folder in Explorer, `Ctrl-A` to select its contents, then `Ctrl-C` — that path uses `FileGroupDescriptorW` directly and folder structure is preserved.
- **Some Windows shell extensions silently swallow specific files from the clipboard.** Archive tools (7-Zip, WinRAR, built-in Compressed Folders) commonly hook extensions like `.zip`, `.gz`, `.7z`, `.bz`, `.bz2`, `.rar`, `.tar` and intercept Explorer's clipboard so `Ctrl-C` either sends no `FileGroupDescriptorW` to mstsc or sends none at all. The Mac side detects the clipboard transition and clears the pasteboard, so `Cmd-V` in Finder beeps clearly instead of silently re-pasting the previous file. **Workaround:** rename the file to a neutral extension (e.g. `.bin`) and Windows will publish it normally.

## Drive redirection

Opt-in with **`--enable-drive-redirection`** (off by default). The connecting
client redirects its **local** drive(s) and the Mac mounts each as a real
**read-write volume** in Finder — the inverse of file copy: instead of moving
bytes through the clipboard, you browse the client's filesystem live. Enable it
on the client too (mstsc: *Local Resources → More → Drives*; FreeRDP:
`/drive:NAME,PATH`).

Under the hood each redirected drive is served by an **in-process NFSv3 server**
that translates NFS operations into RDPDR (MS-RDPEFS) requests, mounted via the
built-in `mount_nfs` — **no root, no kext, no FUSE**. The kernel drives lazy
lookups as you browse, so full subdirectory navigation works, and reads/writes
reuse a kept-open handle so large sequential transfers don't re-open per chunk.
Reading, editing, creating, `mkdir`, rename, and delete all work where the
**redirected Windows user has permission** — e.g. write to `Users\<you>\Documents`,
not the `C:\` root (which an unelevated mstsc session can't write; that surfaces
as a normal "permission denied", not an error). Mounts are torn down when the
client disconnects.

> macOS-only. Every redirected filesystem device gets its own volume.
> `/Volumes` isn't writable without root on a stock Mac, so the mountpoint
> falls back to a per-session folder under `$TMPDIR` (it still shows as a
> volume in Finder).

## Smart-card redirection

Opt-in with **`--enable-smartcard-redirection`** (off by default). The connecting
client redirects its **smart-card reader** and macOS PC/SC apps can use the card
through it — the standard RDP direction (MS-RDPESC), so the card stays on the
client while the Mac in the session reads it. Enable it on the client too
(mstsc: *Local Resources → More → Smart cards*; FreeRDP: `/smartcard`).

On the macOS side macrdp ships **its own PC/SC IFD handler** — a small reader
driver loaded by `com.apple.ifdreader` that presents the redirected card as a
real Finder/PC/SC reader and bridges every PC/SC call to the client over
MS-RDPESC. It's written from scratch (MIT/Apache), so there's **no GPL `vpcd`**
dependency. The whole chain is verified end-to-end on `mstsc` against a card,
including a full APDU transceive.

> **Why a user-space handler and not a kernel driver?** Redirection happens at
> the PC/SC (APDU) layer, not raw USB, and macOS's smart-card stack is user-space
> by design — the IFD handler is Apple's supported plug-in point, with no
> entitlements, signing gymnastics, or reboot a kext would demand. See the
> rationale in [docs/known-quirks.md](docs/known-quirks.md).

<details>
<summary><b>In plain terms: why this "reader hook" instead of USB passthrough (à la VirtualHere)?</b></summary>

There are two ways to let a card plugged into the client be used by apps on the Mac:

- **Fake the hardware (the VirtualHere route).** Pretend the whole USB card-reader
  is physically plugged into the Mac. To make macOS believe a USB device is really
  attached, you write a low-level driver (a DriverKit *system extension*) — which
  needs Apple-granted permissions, a user-approved install, and a lot of plumbing
  to emulate the USB gadget. It's like **shipping the physical reader across the
  network and bolting a fake one onto the Mac's USB port.** Powerful and general
  (works for *any* USB gadget), but heavy.

- **Use the built-in slot (what macrdp does).** macOS already has a smart-card
  system (PC/SC) with an official plug-in slot for "reader helpers." macrdp drops in
  a tiny helper that says *"I'm a card reader,"* and whenever an app asks the card a
  question, the helper **forwards it over the network to the real card on the client
  and relays the answer back.** No fake USB device, no driver, no special
  permissions — it installs as a small file in a folder. Think of it as a
  **receptionist macOS already provides**, to whom we just hand a message-forwarder.

Smart cards talk a simple **question-and-answer protocol**, so we don't need to fake
any hardware — just pass the messages along, and macOS gives us the exact spot to
plug that in. The USB-passthrough approach is the right tool for sharing *arbitrary*
USB gadgets that have no such slot, but for smart cards it's massive overkill — all
that driver/permission friction to end up at the **same place** the small helper
reaches directly. Same result, far less machinery.

</details>

**One-time setup** — the IFD handler installs into a root-owned system directory,
so it can't be done by drag-to-Applications; run the bundled installer once (one
GUI admin prompt, no manual `sudo`):

```bash
# From a checkout, or from an installed app's Resources:
packaging/install-ifd-handler.sh
/Applications/macrdp.app/Contents/Resources/install-ifd-handler.sh   # DMG install

packaging/install-ifd-handler.sh --uninstall                          # remove
```

Run interactively, the installer **lists your attached USB devices and lets you
pick the one to use as the load trigger** (see the caveat below for why a trigger
is needed). To bind one non-interactively instead — or just to look up a device's
IDs — use the picker directly or pass them yourself:

```bash
packaging/select-usb-trigger.sh                       # list devices, print VID/PID
IFD_VID=0x2174 IFD_PID=0x2100 packaging/install-ifd-handler.sh   # bind explicitly
```

Then verify the reader registered with `system_profiler SPSmartCardsDataType`.

> macOS-only. **macOS loads a third-party IFD driver only on a USB *hotplug***
> matching the bundle's VID/PID, so a headless server needs a USB device
> permanently attached (any stick works as the trigger — pick it during install
> or bind it with `IFD_VID`/`IFD_PID`); after installing, unplug/replug it so
> `slotd` loads the driver. The handler talks to macrdp on loopback port 40242
> (`MACRDP_SCARD_PORT`). No physical card needed to try it: create a Windows
> **TPM virtual smart card** (`tpmvscmgr create …`) and redirect that.

> **Reloading after an upgrade.** `slotd` keeps the loaded handler in memory for
> its whole lifetime and ignores `SIGTERM`, so simply replacing the bundle on disk
> does nothing — a rebuilt or upgraded handler isn't picked up until `slotd` is
> killed with `SIGKILL` and the trigger device is replugged. Just **re-run the
> installer**: it restarts `slotd` correctly (`sudo pkill -9 -f com.apple.ifdreader`)
> and verifies the new bundle landed. Then unplug/replug the trigger so the fresh
> `slotd` loads the new driver. (If you ever do it by hand, note that
> `killall com.apple.ifdreader` won't match — the process name is truncated past
> 15 chars; use `pkill -9 -f`.)

## Reason why this was made

This was done to scratch an itch.  There are practically no active open source RDP servers for MacOS.  The closest project that does this functionality is xrdp; however this program only runs on Linux/Unix machines, and has no homebrew equivalent on Macs. Initial POC done in a few hours with the help of Claude and runs pretty well at start. Additional combing through pcap files and documentation, and debugging each mstsc/FreeRDP connect is what makes this work tedious yet rewarding when it finally works.  Multi-monitor support is on the list when I'm bored or need a distraction from real life.

## Support this project

macrdp is free and open source. If it's helped you out, you can buy me a coffee
to help me get through the bumps — totally optional, no pressure.

- ☕ **[Buy me a coffee on Ko-fi](https://ko-fi.com/clintcan)**

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at
your option. Being permissively licensed, a productized/notarized build may be
sold commercially with support — that's selling the product, not a license
exemption.

[IronRDP]: https://github.com/Devolutions/IronRDP
[Thincast client]: https://thincast.com/en/products/client
[Thincast]: https://thincast.com/en/products/client
