# Configuration & CLI reference

Every flag macrdp accepts, the auth-hardening environment variables, headless
mode, and a set of ready-to-run examples. For the LaunchAgent / `config.env`
packaging side, see [../packaging/README.md](../packaging/README.md).

## Full flag reference

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
                          or --virtual-display. See [video.md](video.md). macOS-only.
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
                          matching aspect ratio. See [video.md](video.md).
--max-client-size WxH     Cap the resolution a client can request on the
                          auto-adopt path (e.g. 2560x1440). A request above the
                          cap is clamped per-dimension and the session is served
                          at the clamped size. Defense-in-depth resource bound:
                          without it an authenticated client can request up to
                          the protocol maximum 8192x8192 — a ~256 MB framebuffer
                          per frame. Each dimension must be in [200, 8192]. No
                          effect with --no-client-resolution or an explicit
                          --width/--height/--hidpi/--virtual-display (those pin
                          the size). Config key: MAX_CLIENT_SIZE.
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
--alt-backtick-switch     Also accept Option+` (Alt+` from the client) as a
                          window-cycle trigger for the current app, in addition
                          to Cmd+`. Off by default. The Option+Tab analogue of
                          --alt-tab-switch, for clients that forward Alt+` but
                          gate Win-key combos. Option+Shift+` cycles backward.
                          macOS-only.
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
                          — see [video.md](video.md) for why H.264 wants the higher rate)
--enable-h264             Stream the display as H.264 over EGFX (AVC420),
                          hardware-encoded via VideoToolbox, instead of legacy
                          bitmaps. Falls back to legacy automatically for
                          clients that don't negotiate H.264. See [video.md](video.md).
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
                          so enable it only if big updates lag. See [video.md](video.md).
--flush-frames N          Trailing frames re-sent after each change to drain
                          mstsc's presentation buffer (default 4; only with
                          --enable-h264). Stops the last keystroke before a pause
                          lagging until the next keyframe. 0 disables. See [video.md](video.md).
--enable-aac              Compress system audio as AAC-LC over RDPSND
                          (WAVE_FORMAT_AAC_MS) instead of raw PCM — ~11x less
                          audio bandwidth. Clients that don't decode AAC fall
                          back to PCM automatically. Off by default (adds
                          ~40–50 ms latency). macOS-only. See [audio.md](audio.md).
--aac-bitrate BPS         AAC target bitrate in bits/sec (default 128000; only
                          with --enable-aac). 96000 saves the most bandwidth,
                          192000 is near-transparent.
--no-lazy-paste           Opt out of lazy Windows→Mac file paste (default ON).
                          With lazy, temp files are pre-sized but empty when the
                          copy lands and stream bytes only on Cmd-V, with macOS's
                          native "Preparing to paste" progress dialog. Pass this
                          to fall back to the eager path (downloads everything
                          on copy, auto-fires Cmd-V into Finder when done).
                          See [file-copy.md](file-copy.md).
--enable-drive-redirection  Let the connecting client redirect its local
                          drive(s) (mstsc: Local Resources → Drives; FreeRDP:
                          /drive:NAME,PATH); the Mac mounts each as a real
                          read-write volume in Finder (in-process NFS + built-in
                          mount_nfs, no root/kext/FUSE). Off by default. See
                          [drive-redirection.md](drive-redirection.md). macOS-only.
--enable-smartcard-redirection  Let the connecting client redirect its
                          smart-card reader (mstsc: Local Resources → More →
                          Smart cards; FreeRDP: /smartcard) so macOS apps can use
                          the card through it (MS-RDPESC). Off by default.
                          Requires installing the PC/SC IFD handler once + a USB
                          trigger device — see [smart-card-redirection.md](smart-card-redirection.md).
                          macOS-only.
--enable-usb-redirection  EXPERIMENTAL. Let the connecting client redirect a
                          physical USB device; macrdp presents it as a REAL local
                          device — e.g. a redirected flash drive mounts in Finder.
                          Off by default. The client must opt in too. mstsc gates
                          this behind Group Policy: enable "Allow RDP redirection of
                          other supported RemoteFX USB devices from this computer"
                          (Computer Config → Admin Templates → Windows Components →
                          Remote Desktop Services → Remote Desktop Connection Client →
                          RemoteFX USB Device Redirection) and reboot — only then does
                          the device show under Local Resources → More → USB. FreeRDP:
                          /usb:... (no policy needed). Generic USB redirection
                          (MS-RDPEUSB) via a user-space virtual USB host controller,
                          so it needs the entitled signed+provisioned build (a plain
                          build no-ops). Mass storage is verified; other device
                          classes are untested. macOS-only. See
                          [usb-redirection-feasibility.md](usb-redirection-feasibility.md).
--no-mute-on-minimize     Opt out of muting audio while the client window is
                          minimized (default ON). When the client sends the
                          standard `SuppressOutput` PDU on minimize, the server
                          stops emitting Wave PDUs so the client's audio queue
                          drains naturally; on refocus, audio resumes in sync
                          with the freshly IDR'd video. Pass this to keep audio
                          flowing through a minimize (preserves "minimized
                          YouTube keeps playing on the Mac speakers") at the
                          cost of accepting that drift on refocus. See [audio.md](audio.md).
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
                          See the mstsc reconnect notes in [video.md](video.md). macOS-only; off by default.
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

### Blank recovery + auto-reconnect (on by default with `--enable-h264`)

The mstsc reconnect-blank auto-heal: on a detected blank the server sends a bare
core Deactivation–Reactivation that makes mstsc re-map its retained surface and
present again **in place** (~1-2 s, no disconnect); a connection drop — healed by
the client's auto-reconnect cookie — is only the fallback. Detection is
**link-aware**: the server measures each connection's TCP RTT and, on slow links
(≥ 80 ms) where the detection signal is unreliable, stands down instead of
dropping a working session. The defaults are right for almost everyone; tune via
`config.env` (keys shown) or the matching `MACRDP_*` env vars:

```
BLANK_RECOVERY=1              # master switch (0 = disable the detector entirely)   [env: MACRDP_BLANK_RECOVERY]
BLANK_RECOVERY_REACTIVATE=1   # 1 = reactivate-in-place (heals mstsc); 0 = drop-only  [MACRDP_BLANK_RECOVERY_REACTIVATE]
BLANK_RECOVERY_MAX_RTT_MS=80  # stand down at/above this link RTT (0 = always armed)  [MACRDP_BLANK_RECOVERY_MAX_RTT_MS]
AUTO_RECONNECT=1              # 0 = don't provision the auto-reconnect cookie         [MACRDP_AUTO_RECONNECT]
```

Expert window tunables follow the same pattern (`BLANK_RECOVERY_{MIN_QOE,
MAX_WAIT_MS,MIN_WALL_REPORTS,ARM_MS,RETRY_MS,MAX_ATTEMPTS,MAX_CONSECUTIVE_DROPS}`);
QoE-less clients (FreeRDP) and high-RTT links are never touched. See the
blank-recovery notes in `docs/known-quirks.md` for the full story.

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
