# macrdp Controller (menu-bar app)

A small AppKit menu-bar (tray) app that **controls** the macrdp LaunchAgent and
its `config.env` — start/stop the server, flip feature toggles, jump to the
permission panes, and open the log. It's a separate controller process, not the
server: quitting it leaves macrdp running under launchd.

## Why a separate process (not UI inside the Rust binary)

macrdp's tokio runtime owns the main thread, but AppKit's menu bar **must** run
on the main thread — so an in-process UI would mean restructuring the carefully
tuned threading/QoS model. The controller sidesteps that entirely: it drives
the server through `launchctl` and the shared `config.env`. TCC is unaffected —
the Screen Recording / Accessibility grants belong to the macrdp *binary* (the
API caller), and this controller needs none of them.

## Build & install

Prereq: build the server bundle (`../packaging/make-app.sh`). You do **not**
need to run `install-launchagent.sh` — the controller **self-installs** the
LaunchAgent and onboards the Keychain password on first **Start** (see below).

```bash
./make-tray-app.sh                                  # -> /Applications/macrdpController.app
APP_DIR="$HOME/Applications" ./make-tray-app.sh     # or install without sudo
open "/Applications/macrdpController.app"            # display icon appears in the menu bar
```

Built with plain SwiftPM (no `.xcodeproj`): `swift build -c release` produces
the executable, `make-tray-app.sh` wraps it into a signed `LSUIElement` bundle
in `target/` and installs it.

The bundle-ID prefix is configurable with `BUNDLE_PREFIX` (default `com.clintcan`)
— the controller becomes `$BUNDLE_PREFIX.macrdp.controller` and derives the
server's LaunchAgent label by stripping `.controller` at runtime. **Use the same
`BUNDLE_PREFIX` here as in `../packaging/`**, or the controller drives the wrong
agent.

## First-run self-install

The intended end-user flow is just: drag both apps from the DMG into
`/Applications`, open the controller, click **Start**. On first Start the
controller:

1. **Locates `macrdp.app`** (sibling in the same folder, else `/Applications`,
   else `~/Applications`).
2. **Prompts for your macOS account password** and stores it in the Keychain
   (the headless server reads it from there; written via `security` so there's
   no read-time Keychain prompt).
3. **Writes + loads the LaunchAgent** pointing at that server bundle.
4. **Reminds you to grant** Screen Recording + Accessibility, with a button to
   open the pane.

No Terminal, no `install-launchagent.sh`. For unattended/MDM deploys there's a
headless equivalent:

```bash
macrdpController.app/Contents/MacOS/macrdptray --install-agent   # locate + write + load agent
macrdpController.app/Contents/MacOS/macrdptray --print-paths      # diagnose resolved paths (no side effects)
```
(`--install-agent` assumes the Keychain password is set separately.)

## Menu

- **Status header** — running (with pid) / stopped / not installed; a
  **⚠️ error** line appears when stopped and the log shows a known failure (login
  failed / port in use / crash). The menu-bar icon dims when not running.
- **Set Up Remote Desktop** — one click applies the recommended config (virtual
  display + detach + H.264 + app-switcher HUD + per-connection workers) and
  starts. Shown until that config is active.
- **Start · Stop · Restart** — self-installs on first run, then `kickstart -k` /
  `bootout` the agent (with EIO retry).
- **Options** — H.264 / AAC / HiDPI / **Un-minimize on Cmd+Tab** / **App-switcher HUD** /
  **Per-connection workers (reconnect fix)** checkmarks, **Drive / Smart-card
  redirection** toggles, plus **Allow network connections** (flips `BIND` between
  `127.0.0.1` and `0.0.0.0`, preserving the port, with a confirmation before
  exposing to the LAN); shows the current listening address. All write
  `config.env` and live-`kickstart` if running.
- **Install smart-card handler…** — one-time privileged install of the PC/SC IFD
  handler (the redirection toggle only flips the server flag). Pops up a list of
  your **attached USB devices** to pick the load trigger (macOS loads the driver
  only on a USB hotplug matching its VID/PID), passes the choice to the embedded
  installer as `IFD_VID`/`IFD_PID`, and runs it (one admin prompt). Choose
  *Keep default trigger* to leave the bundle's baked-in IDs.
- **Display** — **Virtual display (headless)** toggle, a **Primary screen** radio
  (`PRIMARY_MODE`: *Keep local screen on* / *Detach — move apps to remote* /
  *Blank — keep apps on Mac*), and a **Virtual display resolution** picker
  (1280×720 / 1600×900 / 1920×1080 default / 2560×1440). **Detach** disables the
  physical panel so macOS moves your real apps onto the virtual display (the
  responsive "remote into my desktop" setup); **Blank** (capture) just blacks the
  panel and leaves apps on it. Both need the virtual display, so picking either
  auto-enables it; turning the virtual display off resets the mode to *Keep on*.
  A virtual display at the client's resolution is captured 1:1 (no scaling), so
  it's snappier than mirroring a non-matching panel.
- **Edit config… · Set/Change Account Password…** — edit flags; (re)store the
  Keychain password.
- **Open Logs** — opens `~/Library/Logs/macrdp.log`.
- **Permissions** — Screen Recording / Accessibility with **live ✓ / needs-grant
  status** (parsed from the server log); clicking opens the System Settings pane.
- **Quit Controller** — quits the menu-bar app only; the server keeps running.

## Distribution (paid product)

Ad-hoc signing is local-only. For a shippable build, sign with a Developer ID
and notarize (same env contract as `packaging/`):

```bash
xcrun notarytool store-credentials macrdp-notary \
  --apple-id you@example.com --team-id TEAMID --password <app-specific-pw>

CODESIGN_IDENTITY="Developer ID Application: Your Name (TEAMID)" \
  NOTARIZE=1 NOTARY_PROFILE=macrdp-notary ./make-tray-app.sh
```

This is MIT-licensed; selling a productized, notarized build + support is fully
compatible with that (you're selling the product, not a license exemption).
The Mac App Store is not viable for the server it controls (private
`CGVirtualDisplay` API + system-wide input/capture) — ship a direct download.

## Notes
- To auto-launch the controller at login: System Settings → General →
  Login Items → add `macrdpController.app` (the server itself already
  autostarts via its own LaunchAgent).
