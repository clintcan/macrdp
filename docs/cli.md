# Commands & CLI reference

```bash
cargo build                    # debug build
cargo build --release          # release build (LTO, ~30s)
cargo run                      # prompts for password, runs against PAM
cargo run -- --skip-auth --password test  # bypass PAM for quick tests
cargo run -- --virtual-display --width 1920 --height 1080  # headless remote desktop, local screen untouched
cargo test                     # run all tests
cargo clippy --all-targets -- -D warnings  # lint as errors
cargo fmt                      # format
RUST_LOG=debug cargo run       # crank logging for troubleshooting
```

Useful CLI flags (see `src/main.rs::Args` for the full set):
```
--bind 0.0.0.0:3390       # listen address
--username NAME           # default: $USER
--password PASS           # avoid the interactive prompt (logs are warned)
--skip-auth               # bypass PAM (also skips password validation)
--width  / --height       # override autodetected display size
--hidpi                   # capture the primary display at backing (Retina) pixels
                          #   instead of logical points (~4x pixels; crisp; best
                          #   with --enable-h264). Ignored with --width/--height
                          #   or --virtual-display. macOS-only.
--fps N                   # default 60 with --enable-h264, else 15
--cursor-scale MULT       # pointer size multiplier (default 1.0 = native macOS
                          #   size, hotspot-exact). Bump (e.g. 1.5/2.0) if your
                          #   client upscales the desktop but draws the pointer
                          #   at native pixels, making it look small.
--keyboard-layout SPEC    # Force a non-US layout (name like `french`/`de`/
                          #   `azerty`, a Windows KLID like 0x040C, or a macOS
                          #   input-source id) instead of auto-detecting from the
                          #   client. `none` disables translation. Keys are
                          #   translated via UCKeyTranslate and posted as Unicode;
                          #   the Mac's own input source is untouched. The layout
                          #   must be installed on the Mac. Auto-detect is the
                          #   default (no flag needed). macOS-only.
--map-ctrl-to-cmd         # Remap Windows editing shortcuts (Ctrl+C/V/X/A/Z/S/F/
                          #   N/T/W/O/P/R/G, + Shift variants) to their Cmd
                          #   equivalents so Windows muscle memory drives macOS
                          #   copy/paste. Off by default (Q excluded; nav keys
                          #   untouched). Auto-suppressed when a terminal is
                          #   frontmost so Ctrl+C stays SIGINT. macOS-only.
--no-remap-apps LIST      # Comma-separated bundle ids where --map-ctrl-to-cmd is
                          #   suppressed, on top of the built-in terminal list —
                          #   for editors with an embedded terminal that can't be
                          #   auto-detected (e.g. com.microsoft.VSCode). macOS-only.
--no-client-resolution    # Serve the Mac display's native size instead of the
                          #   resolution the client requests at connect (the
                          #   auto-adopt default when no --width/--height/
                          #   --hidpi/--virtual-display is given).
--stretch                 # On the auto-size path, fill the client frame instead
                          #   of the default aspect-preserving letterbox/pillarbox.
                          #   No effect with --width/--height or matching aspect.
--enable-h264             # stream H.264 over EGFX (AVC420) instead of legacy bitmaps
--keyframe-interval SECS  # periodic IDR safety net (default 2; only with --enable-h264)
--flush-frames N          # trailing skip-P-frames re-sent after each change to drain
                          #   mstsc's presentation buffer (default 4; 0 disables; --enable-h264)
--enable-aac              # Compress RDPSND audio as AAC-LC (WAVE_FORMAT_AAC_MS)
                          #   instead of raw PCM; ~11x less bandwidth. PCM fallback is
                          #   automatic for clients without AAC decode. Adds ~40-50 ms
                          #   latency, so off by default.
--aac-bitrate BPS         # AAC target bitrate (default 128000; only with --enable-aac)
--enable-drive-redirection # RDPDR drive redirection (opt-in, read-write): the
                          #   client redirects its local drive and the Mac mounts
                          #   each as a real NFS volume in Finder. The client must
                          #   opt in too (mstsc Local Resources → Drives; FreeRDP
                          #   /drive:NAME,PATH). macOS-only.
--enable-smartcard-redirection # RDPDR smart-card redirection (opt-in,
                          #   MS-RDPESC): the client redirects its smart-card
                          #   reader and macOS apps use the card through it.
                          #   Needs the PC/SC IFD handler installed once
                          #   (packaging/install-ifd-handler.sh) + a USB trigger
                          #   device. Client opts in too (mstsc Local Resources →
                          #   More → Smart cards; FreeRDP /smartcard). macOS-only.
--no-lazy-paste           # Opt out of lazy Windows→Mac file paste (default ON).
                          #   Lazy streams bytes on Cmd-V (NSFilePresenter) with native
                          #   "Preparing to paste" progress and lower chunk parallelism;
                          #   --no-lazy-paste reverts to eager download + auto-paste hack.
--enable-udp-multitransport # EXPERIMENTAL, opt-in (default OFF; feature-gated by
                          #   the `multitransport` cargo feature). Offers RDP UDP
                          #   multitransport (MS-RDPEMT over reliable RDPEUDP) and
                          #   binds a UDP listener on the same address/port as TCP.
                          #   On its own EGFX stays on TCP (the proven safe spike);
                          #   pass --udp-migrate-egfx to move H.264 video onto the
                          #   reliable UDP tunnel. Input/audio/clipboard always ride
                          #   TCP. Not supported under --fork-workers (falls back to
                          #   TCP). macOS-built but the protocol layer is
                          #   cross-platform. See
                          #   docs/rdp-udp-multitransport-feasibility.md.
--udp-migrate-egfx        # EXPERIMENTAL, opt-in (default OFF; requires
                          #   --enable-udp-multitransport). Migrate the EGFX (H.264)
                          #   channel onto the reliable UDP tunnel via MS-RDPEDYC
                          #   Soft-Sync (verified rendering on mstsc). Clean-link
                          #   optimal: under packet loss the reliable ordered tunnel
                          #   head-of-line-blocks like TCP, but an auto-recovery
                          #   WATCHDOG now detects the wedge (~3s of silent EGFX acks
                          #   while shipping) and de-migrates EGFX back to the TCP
                          #   DRDYNVC channel (one-way per session) so the session
                          #   keeps running instead of freezing-until-reconnect (audio
                          #   was always on TCP). Promoted 2026-06-28 from the
                          #   MACRDP_UDP_MIGRATE_EGFX env var (still works as a
                          #   fallback). macOS-built; protocol layer cross-platform.
--adaptive-bitrate        # Opt-in (default OFF; ADAPTIVE_BITRATE=1 in config.env).
                          #   Congestion-responsive H.264 rate control on BOTH the UDP
                          #   tunnel AND the TCP path (only with --enable-h264): an
                          #   AIMD controller reads the STANDING QUEUE DELAY — each
                          #   frame's ship→ack round trip minus the windowed-minimum
                          #   RTT, EWMA-smoothed — plus reliable-tunnel retransmits,
                          #   and live-adjusts the VideoToolbox bitrate within
                          #   [floor, --bitrate ceiling] — multiplicative-decrease
                          #   under sustained congestion, additive-increase when clear
                          #   (hysteresis + a 3-zone hold so single spikes don't pump
                          #   the bitrate). The signal is RTT-aware: a long-but-clean
                          #   pipe (VPN/ZeroTier at 200+ ms) reads as ~0 queue and
                          #   keeps FULL quality (the pre-2026-07-04 frame-count
                          #   signal misread RTT as congestion and oscillated); a
                          #   genuinely thin pipe reads as real queue and degrades
                          #   gracefully — including a no-ack distress fallback so a
                          #   fully choked client (acks silent) still registers.
                          #   Under congestion it also stretches the periodic IDR
                          #   (BOTH transports) to avoid injecting a big keyframe
                          #   into a congested pipe. So --bitrate is a CEILING, not a
                          #   fixed target: set it high (e.g. 8) and it backs off
                          #   only when the link strains.
                          #   P2b frame-rate floor: once bitrate is pinned at the floor
                          #   AND still congested (quality cuts exhausted), it caps the
                          #   effective fps (default 10) to shed packet load — on BOTH
                          #   transports, never to zero (the client needs trailing frames
                          #   to present/ack). Video degrades to choppy-but-steady-and-in-
                          #   sync instead of freezing; fps + bitrate recover when clear.
                          #   RTT-seeded start (2026-07-05): when the kernel-measured
                          #   TCP RTT at accept is >= MACRDP_ADAPTIVE_SEED_RTT_MS
                          #   (default 50; 0 disables), the encoder STARTS at
                          #   ceiling/3 (clamped to the floor) instead of the full
                          #   ceiling, so the first seconds don't overshoot a distant
                          #   pipe; the controller climbs back within seconds if the
                          #   link has headroom (long-but-fat links still reach full
                          #   quality). Fast/unknown links start at the ceiling.
                          #   Tunables: MACRDP_UDP_ADAPTIVE_{FLOOR_BPS,INCREASE_BPS,
                          #   DECREASE,INTERVAL_MS,RETX_TOLERANCE},
                          #   MACRDP_ADAPTIVE_QUEUE_HIGH_MS (congestion threshold,
                          #   default 100 ms of standing queue; replaces the removed
                          #   MACRDP_{UDP,TCP}_ADAPTIVE_LAG_THRESHOLD),
                          #   MACRDP_ADAPTIVE_EWMA_ALPHA, MACRDP_ADAPTIVE_FLOOR_FPS.
                          #   The RETX_TOLERANCE (default 2) is the per-interval
                          #   reliable-retransmit count tolerated before treating loss
                          #   as congestion — keeps sporadic wireless (WiFi)
                          #   retransmits from ratcheting the bitrate down (0 = any
                          #   retransmit counts, the old behaviour). macOS-only.
--enable-lossy-audio      # EXPERIMENTAL, opt-in (default OFF; ENABLE_LOSSY_AUDIO=1
                          #   in config.env). Implies --enable-udp-multitransport;
                          #   needs --enable-aac + --enable-h264. Stream RDPSND audio
                          #   over a LOSSY UDP/DTLS tunnel instead of TCP — the loss-
                          #   resilient audio path. AAC Wave2 data rides a lossy
                          #   RDPEUDP flow (deliver-on-arrival, no retransmit) and
                          #   each datagram is sent TWICE (client DTLS anti-replay
                          #   dedups), so an independent-loss link of rate p drops a
                          #   payload only at p^2. Soak-verified smooth on mstsc at
                          #   5/10/15% loss where single-send glitches. Bridges the
                          #   MACRDP_UDP_{OFFER_FECL,LOSSY_DELIVERY,LOSSY_AUDIO,
                          #   LOSSY_AUDIO_DUP} env gates (still work standalone).
                          #   CAVEAT (2026-07-04): prefer LAN/WiFi. Over VPN/ZeroTier-
                          #   class overlays the UDP tunnel can wedge and mstsc's ~60s
                          #   dead-tunnel timeout resets the session. Tunnel-death
                          #   detection now bounds that: after MACRDP_UDP_TUNNEL_
                          #   DEAD_SECS (30) of inbound silence on a bound tunnel,
                          #   audio falls back to TCP and multitransport offers are
                          #   suppressed for MACRDP_UDP_MT_COOLDOWN_SECS (600) — so
                          #   the reset reconnects as a stable plain-TCP session
                          #   (at most ONE reset instead of an endless cycle;
                          #   LIVE-VERIFIED 2026-07-04 on real mstsc/ZeroTier:
                          #   3x tunnel-DEAD at ~30s, audio kept playing, all
                          #   reconnects offer-SUPPRESSED plain TCP). And the offer
                          #   is now RTT-GATED at connect: links at/above
                          #   MACRDP_UDP_OFFER_MAX_RTT_MS (80; 0 disables) are never
                          #   offered UDP at all — they run plain TCP from the first
                          #   byte — so this flag is safe to leave on for a roaming
                          #   client. Plain TCP (both flags off) remains the
                          #   zero-risk config for overlay-only setups.
                          #   macOS-only.
--fork-workers            # Opt-in (default OFF; FORK_WORKERS=1 in config.env) —
                          #   and staying opt-in by DECISION (2026-07-04, roadmap
                          #   Tier 2.6): the production default is single-process +
                          #   blank-recovery + the auto-reconnect cookie (field-
                          #   proven, composes with every feature). Choose
                          #   --fork-workers for mstsc-heavy, no-UDP deployments
                          #   that want blip-free reconnects + per-connection
                          #   isolation; it COMPOSES with blank-recovery (recovery
                          #   drops land on a fresh worker = strongest combo).
                          #   xrdp's model on macOS: a thin supervisor
                          #   binds the port and fork+execs a FRESH worker process
                          #   per connection (socket via MACRDP_WORKER_FD). The fresh
                          #   process dodges mstsc's reconnect-blank (it re-maps a
                          #   fresh EGFX surface on a brand-new channel) — reconnect
                          #   to a still-running server renders instead of going
                          #   blank; a residual ~1/7 blank recovers by reconnecting
                          #   once more. The supervisor owns the persistent state
                          #   (virtual display, headless blanking, caffeinate,
                          #   app-switcher HUD); workers are per-connection. Works
                          #   mirror-primary or with --virtual-display (+ optional
                          #   --capture-primary/--detach-primary). Smart-card
                          #   redirection works under it too (per-connection
                          #   :40242 bridge; verified incl. reconnect).
                          #   MUTUALLY EXCLUSIVE with --enable-udp-multitransport:
                          #   the UDP path needs ONE persistent socket on the port
                          #   that survives reconnects, but a per-connection worker
                          #   can't own it (the supervisor owns the port). If both
                          #   are set, the supervisor warns and each worker still
                          #   OFFERS multitransport but binds no UDP listener, so
                          #   the client falls back to TCP (EGFX on TCP). Combining
                          #   them would need the supervisor to own the UDP socket +
                          #   demux datagrams to the right worker — deferred, and low
                          #   priority (the soak found reliable-UDP is a clean-link
                          #   nicety, while --fork-workers fixes the real mstsc pain).
                          #   macOS-only. See the H.264 reconnect-blank quirk note.
--cert-dir PATH           # default ~/Library/Application Support/macrdp
--cert PATH               # Operator-supplied TLS certificate (PEM; leaf first,
                          #   then any intermediate chain). Serve a real CA / ACME
                          #   / Let's Encrypt cert instead of the self-signed
                          #   default. MUST be given with --key. When set, macrdp
                          #   uses exactly these files and NEVER falls back to
                          #   self-signed — a missing/unreadable file is a hard
                          #   error. A cert change needs a restart (launchctl
                          #   kickstart -k). Config keys: TLS_CERT / TLS_KEY.
--key PATH                # Private key (PEM) for --cert. Must be chmod 600 and
                          #   readable by the macrdp user. Required with --cert.
--log-dir PATH            # Directory for the rotating log file (macrdp.log). If
                          #   unset: a rotating file in ~/Library/Logs when running
                          #   HEADLESS (stdout is not a TTY, e.g. under the
                          #   LaunchAgent), or stdout when interactive (cargo run).
                          #   The file is size-bounded — keeps macrdp.log + N
                          #   logrotate-style archives (macrdp.log.1, .2, …),
                          #   dropping the oldest. Tunables: MACRDP_LOG_MAX_BYTES
                          #   (default 10 MiB), MACRDP_LOG_MAX_FILES (default 5).
                          #   Config key: LOG_DIR.
```

Auth hardening (env-only, **on by default**). macrdp rate-limits and briefly
(escalating) locks out source IPs that hammer the port with connection attempts,
in front of the existing NLA/CredSSP gate, and writes a per-connection audit line.
**Loopback (`127.0.0.1`/`::1`) is always exempt** — these only bite when `--bind`
exposes the server to other hosts, and you can't lock yourself out locally. The
defaults are conservative; tune or disable via env (settable through `config.env`
— see the matching keys there — or the LaunchAgent plist `EnvironmentVariables`):
```
MACRDP_CONN_GUARD=1               # master switch (0/off = disable rate-limit + lockout)
MACRDP_AUDIT_LOG=1                # connection audit log (independent of the guard)
MACRDP_GUARD_RL_MAX=10            # max attempts per window per IP (0 = no rate-limit)
MACRDP_GUARD_RL_WINDOW_SECS=60    # rate-limit sliding window
MACRDP_GUARD_FAIL_THRESHOLD=5     # consecutive failures before lockout (0 = no lockout)
MACRDP_GUARD_FAILFAST_SECS=3      # only errored connections that fail within this window (pre-handshake) count toward lockout
MACRDP_GUARD_COOLDOWN_BASE_SECS=30  # first lockout length (doubles per extra failure)
MACRDP_GUARD_COOLDOWN_MAX_SECS=900  # lockout escalation cap (15 min)
```
The lockout **escalates and auto-expires** — duration `COOLDOWN_BASE << (failures −
threshold)`, capped at `COOLDOWN_MAX`. With the defaults it triggers at the 5th
consecutive failure and doubles from 30 s as the IP keeps failing past each cooldown:

| Consecutive failures | Lockout |
|---|---|
| 5 (threshold) | 30 s |
| 6 | 60 s |
| 7 | 120 s |
| 8 | 240 s |
| 9 | 480 s |
| 10+ | 900 s (capped) |

There is **no manual unlock** — each cooldown auto-expires, then the next attempt is
allowed through. Escalation requires *actually failing again after* each cooldown
clears (while locked out, attempts are rejected pre-handshake and don't count as new
failures), and **a clean session — or any connection that got past the handshake —
resets the IP to 0**. The lockout is **heuristic**: only a connection that errored
*and* failed within the fail-fast window (`MACRDP_GUARD_FAILFAST_SECS`, ~3s — i.e.
never authenticated, the brute-force signature) counts as a failure. A client that
connected for several seconds and *then* errored (e.g. mstsc's reconnect-blank or a
flaky link) is treated as legitimate and does **not** accrue toward a lockout — so a
reconnecting real client is never locked out, and a single benign disconnect (mstsc's
first-connect cert-prompt "Broken pipe") never does either. Audit lines are tagged
`macrdp::audit`: `grep 'macrdp::audit' ~/Library/Logs/macrdp.log` shows
`event="accept|reject|disconnect"` with the source IP and (for rejects) the reason
and retry-after.

Health-check watchdog (env-only; **on by default when headless**). Detects a
**hung-but-alive** process — the tokio runtime deadlocked / all workers blocked —
and exits (code 70) so the LaunchAgent `KeepAlive` (or the `--fork-workers`
supervisor) restarts a fresh one. `KeepAlive` alone only restarts on an outright
crash, not a wedge; this closes that gap. It's armed on the long-lived
launchd-watched process (single-process serve or the supervisor) and **skipped on
short-lived fork workers** and, by default, **interactively** (stdout is a TTY,
e.g. `cargo run` — a false bounce there would just kill a dev session, and nothing
would restart it). Defaults are conservative — a wedge must persist ~90 s before a
bounce, so load spikes never trip it. Tune / force / disable via env (or the
matching `config.env` keys):
```
MACRDP_HEALTHCHECK=1                 # force on (even interactive); 0/off = disable
MACRDP_HEALTHCHECK_INTERVAL_SECS=15  # delay between liveness probes
MACRDP_HEALTHCHECK_TIMEOUT_SECS=30   # max time a probe may take before it's a miss
MACRDP_HEALTHCHECK_FAILURES=2        # consecutive misses before the process exits
```
The mechanism: a dedicated OS thread (not a tokio task, so it keeps ticking even
when the runtime it watches is wedged) submits a trivial probe onto the runtime
each interval and waits `TIMEOUT_SECS` for it to run; a deadlocked runtime never
runs it. A bounce logs `health-check watchdog: tokio runtime wedged …` before
exiting.

Testing against the server:
```bash
# FreeRDP — easiest to script and get verbose logs from.
xfreerdp /v:127.0.0.1:3390 /u:$USER /cert:ignore /log-level:DEBUG

# Microsoft Remote Desktop / Windows App.app — closest to real-user UX.
# Windows mstsc: just enter the computer and click Connect — NLA/CredSSP
# is enabled, mstsc will prompt for credentials in its own dialog.
# Expect one "Broken pipe" error in the log on the first attempt: that's
# mstsc's cert-trust prompt closing and reopening the socket. The next
# attempt succeeds.
```

When iterating on the capture/encode path, prefer FreeRDP with `/log-level:DEBUG` — its PDU traces are far more useful than mstsc's silent failures.

To decrypt a Wireshark capture of a session, run macrdp with `SSLKEYLOGFILE=/path/to/keylog.txt`
and point Wireshark at that file (Preferences → Protocols → TLS → "(Pre)-Master-Secret log
filename"). Covers the TCP RDP connection and the reliable-UDP multitransport flow (rustls);
the lossy flow's DTLS is not covered. macrdp warns loudly at startup while the var is set —
session keys on disk break the capture's confidentiality, so use it only for protocol debugging.
