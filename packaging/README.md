# Packaging — `macrdp.app` (personal use)

Wraps the `macrdp` binary in a stably-signed `.app` bundle and runs it as a
per-user **LaunchAgent**. The point is not double-click UX (macrdp is a
flag-driven server) — it's a **stable signed identity at a fixed path** so the
Screen Recording / Accessibility TCC grants survive rebuilds, plus
non-interactive autostart via the Keychain.

The LaunchAgent runs the **signed binary directly** as
`macrdp --config <config.env>` — the binary parses the same `key=value` file the
menu-bar controller edits. (There used to be an unsigned `macrdp-launch` shell
wrapper here; it was removed because macOS Background Task Management re-flagged
it as a new "background item" on every rebuild — a signed Mach-O launch target
is approved once and stays approved.)

This layout is also GUI-ready: the menu-bar controller drives the same
LaunchAgent and edits the same `config.env` — no re-permissioning.

> **vs. `dist/install.sh`:** the repo's other auto-start path installs a *bare
> binary* to `~/.local/bin/macrdp` under the launchd label `com.user.macrdp`.
> This `packaging/` path instead produces a real `.app` bundle under the label
> `com.clintcan.macrdp`. They share the `macrdp` Keychain entry and both bind
> `:3390`, so they're **mutually exclusive** — pick one. Use `dist/install.sh`
> for the lightweight binary; use `packaging/` when you want a stable bundle
> identity and a path a future GUI can build on. The build is staged in
> `target/macrdp.app` (gitignored) before install.

## Files

| File | Role |
|------|------|
| `Info.plist` | Bundle metadata template (`__VERSION__` filled from `Cargo.toml`); `LSUIElement` agent, `NSAppleEventsUsageDescription`. |
| `config.env.example` | Seed for `~/Library/Application Support/macrdp/config.env`. The signed binary reads this directly via `macrdp --config`. |
| `launchagent.plist.template` | LaunchAgent template (`__LABEL__`/`__APP_DIR__`/`__HOME__` filled at install). |
| `make-app.sh` | Build → assemble bundle → co-sign helper + bundle (incl. the embedded smart-card IFD handler) → install. |
| `install-launchagent.sh` | Seed config, render plist, bootstrap the agent. |
| `ifd-Info.plist` | Info.plist template for the embedded `ifd-macrdp.bundle` (smart-card IFD handler); `__VERSION__`/`__BUNDLE_ID__` filled by `make-app.sh`. |
| `install-ifd-handler.sh` | Privileged install of the IFD handler into `/usr/local/libexec/SmartCardServices/drivers` (one GUI admin prompt). Run interactively it offers the USB-trigger picker below. Also embedded in the app at `Contents/Resources/`. |
| `select-usb-trigger.sh` | List attached USB devices and pick one as the IFD-handler load trigger; prints its `VID PID` (and a ready-to-paste `IFD_VID=.. IFD_PID=..` install line). Invoked by `install-ifd-handler.sh` interactively; runnable standalone. Embedded in the app next to the installer. |
| `notarize.sh` | Notarize + staple a `.app`/`.dmg`/`.pkg` (used by the build scripts). |
| `make-dmg.sh` | Wrap signed apps into a signed + notarized distribution DMG (styled icon layout). |
| `make-icns.sh` | Build `AppIcon.icns` from a square PNG (used by the build scripts). |

### App icon & DMG styling

Drop a square (1024×1024) **`packaging/icon.png`** and the build bakes it into
both bundles' `AppIcon.icns` + `CFBundleIconFile`. Per-app overrides:
`packaging/macrdp.png` (server) and `packaging/macrdpController.png` (controller);
each falls back to `icon.png`. No icon file → bundles use the default (generic) icon.

`make-dmg.sh` lays out the DMG window (positioned app + controller + `/Applications`
drop-link) via Finder automation — **best-effort**: if the build process lacks
Finder automation permission it falls back to an unstyled but functional DMG.
Optional **`packaging/dmg-background.png`** sets a window background.

## One-time setup

```bash
# 1. Build + install the bundle (ad-hoc signed) to /Applications.
#    Use APP_DIR=$HOME/Applications to avoid sudo.
packaging/make-app.sh

# 2. Store the macOS account password so launchd can start headless.
security add-generic-password -s macrdp -a "$(id -un)" -w 'YOUR_PASSWORD'

# 3. Install + load the LaunchAgent.
packaging/install-launchagent.sh

# 4. First launch will need TCC grants. Grant macrdp.app under
#    System Settings -> Privacy & Security -> Screen Recording AND Accessibility,
#    then: launchctl kickstart -k gui/$(id -u)/com.clintcan.macrdp
```

## Day to day

```bash
tail -f ~/Library/Logs/macrdp.log                       # logs
launchctl print gui/$(id -u)/com.clintcan.macrdp        # status (state/pid)
$EDITOR "$HOME/Library/Application Support/macrdp/config.env"
launchctl kickstart -k gui/$(id -u)/com.clintcan.macrdp # apply config change
launchctl bootout    gui/$(id -u)/com.clintcan.macrdp   # stop entirely
```

Edit feature toggles (H.264, AAC, HiDPI, un-minimize-on-Cmd+Tab), the headless virtual display
(`VIRTUAL_DISPLAY`/`PRIMARY_MODE`/`VD_WIDTH`/`VD_HEIGHT`), per-connection workers
(`FORK_WORKERS` — experimental; fixes mstsc's reconnect blank by forking a fresh
worker process per connection), bind address, and
an `EXTRA_FLAGS` escape hatch in `config.env`. It's outside the bundle, so edits
never disturb the code signature or the TCC grants.

## Smart-card redirection (optional)

Lets the connecting client's smart-card reader be used by macOS apps. The
macOS-side virtual reader is a PC/SC IFD handler that `make-app.sh` builds and
**embeds in the app** (`Contents/Resources/ifd-macrdp.bundle`); it ships in the
DMG. It must be installed once into the system driver directory (root-owned), so
it needs a one-time admin prompt and **isn't** done by drag-to-Applications:

```bash
# Install the IFD handler (GUI admin prompt; no manual sudo). From a checkout:
packaging/install-ifd-handler.sh
# …or from an installed app (e.g. a DMG install):
/Applications/macrdp.app/Contents/Resources/install-ifd-handler.sh

# Bind the USB device that triggers the driver load (macOS loads IFD drivers
# only on a matching USB hotplug — a headless server needs a permanent dongle):
IFD_VID=0x2174 IFD_PID=0x2100 packaging/install-ifd-handler.sh

# Uninstall:
packaging/install-ifd-handler.sh --uninstall
```

Then set `ENABLE_SMARTCARD_REDIRECTION=1` in `config.env` (and have the client
redirect its reader: mstsc → Local Resources → More → Smart cards). Verify the
reader registered with `system_profiler SPSmartCardsDataType`.

## Notes & limits

- **Bundle-ID prefix is configurable** via `BUNDLE_PREFIX` (default `com.clintcan`):
  the app becomes `$BUNDLE_PREFIX.macrdp`, the LaunchAgent label the same, and the
  controller `$BUNDLE_PREFIX.macrdp.controller`. **Set the *same* `BUNDLE_PREFIX`
  for `make-app.sh`, `install-launchagent.sh`, and `gui/make-tray-app.sh`** — the
  controller derives the agent label from its own bundle id, so a mismatch makes
  it drive the wrong (or no) agent. Pick this before your first public build (it's
  what publishes under your company's reverse-DNS). The launchctl examples below
  use the default label; substitute yours if you changed the prefix.

  ```bash
  BUNDLE_PREFIX="com.acme" packaging/make-app.sh
  BUNDLE_PREFIX="com.acme" packaging/install-launchagent.sh
  BUNDLE_PREFIX="com.acme" gui/make-tray-app.sh
  ```
- **Ad-hoc signing is local-only.** `make-app.sh` ad-hoc signs by default
  (`CODESIGN_IDENTITY=-`), which is fine for your own machine but Gatekeeper
  quarantines it on anyone else's. For distribution, sign with a Developer ID
  and notarize:

  ```bash
  # one-time: store notary credentials in the keychain
  xcrun notarytool store-credentials macrdp-notary \
    --apple-id you@example.com --team-id TEAMID --password <app-specific-pw>

  # build signed + notarized + stapled (secure timestamp is automatic for a real ID)
  CODESIGN_IDENTITY="Developer ID Application: Your Name (TEAMID)" \
    NOTARIZE=1 NOTARY_PROFILE=macrdp-notary packaging/make-app.sh
  ```

  `packaging/notarize.sh` (zip → `notarytool submit --wait` → `stapler staple`)
  runs on the staged app so the ticket travels with the install copy.
  **Note:** the Mac App Store is not a viable channel — macrdp uses the private
  `CGVirtualDisplay` API and system-wide `CGEventPost`/ScreenCaptureKit, which
  the MAS sandbox forbids. Ship a notarized direct download (DMG/zip).

- **Distribution DMG.** Once the app(s) are signed + notarized, wrap them into a
  download-ready DMG (with an `/Applications` drop-link), then sign + notarize
  the DMG itself so it passes Gatekeeper offline:

  ```bash
  # build + notarize both apps first (NOTARIZE=1 as above), then:
  CODESIGN_IDENTITY="Developer ID Application: Your Name (TEAMID)" \
    NOTARIZE=1 NOTARY_PROFILE=macrdp-notary packaging/make-dmg.sh
  # -> target/macrdp-<version>.dmg  (pass explicit App.app paths to override)
  ```

  Verifying a notarized DMG: `xcrun stapler validate <dmg>` is definitive;
  `spctl` needs `--type open --context context:primary-signature` or it reports
  a misleading "Insufficient Context".
- **TCC is keyed to the binary.** The grants attach to `Contents/MacOS/macrdp`
  (the process that calls ScreenCaptureKit / CGEventPost) — which is exactly the
  process launchd now runs. Keep the install path stable and the grants persist.
- **Re-signing on rebuild** keeps the same identity as long as the bundle ID,
  install path, and signing identity are unchanged — so re-running
  `make-app.sh` for an update does not reset permissions. **CAVEAT: this is
  only true with a real certificate identity — ad-hoc signing (`-`) can NOT
  keep TCC grants across rebuilds.** An ad-hoc signature has no certificate,
  so its designated requirement degrades to `cdhash H"<hash of the binary>"`
  (verify with `codesign -d -r- macrdp.app`) — TCC keys the grant to that
  exact binary hash, and every rebuild changes it, invalidating the grant
  (confirmed live 2026-07-05: three consecutive ad-hoc rebuilds each required
  re-granting Screen Recording + Accessibility). The zero-cost fix is a
  **self-signed code-signing certificate named `macrdp-dev`** in the login
  keychain — `make-app.sh` auto-prefers it when `CODESIGN_IDENTITY` is unset —
  which makes the requirement `identifier "com.clintcan.macrdp" and
  certificate leaf = H"<cert hash>"`, stable across rebuilds. Create it once
  via Keychain Access → Certificate Assistant → Create a Certificate →
  type "Code Signing", or scripted:
  ```bash
  openssl req -x509 -newkey rsa:2048 -keyout k.pem -out c.pem -days 3650 -nodes \
    -subj "/CN=macrdp-dev" -addext "keyUsage=critical,digitalSignature" \
    -addext "extendedKeyUsage=critical,codeSigning" -addext "basicConstraints=critical,CA:FALSE"
  openssl pkcs12 -export -legacy -out d.p12 -inkey k.pem -in c.pem -passout pass:tmp
  security import d.p12 -k ~/Library/Keychains/login.keychain-db -P tmp -T /usr/bin/codesign
  security add-trusted-cert -p codeSign -k ~/Library/Keychains/login.keychain-db c.pem
  rm k.pem c.pem d.p12   # the key lives in the keychain now
  ```
  (`-legacy` matters: macOS `security` can't read OpenSSL 3's default PKCS12.)
  One re-grant after switching identities, then grants persist.
- Login-window / lock-screen / secure-input contexts still can't receive
  synthetic input — an OS limitation, unchanged by packaging.
