# Architecture

```
src/main.rs       CLI, TCC preflight, TLS cert mgmt, RdpServer assembly
src/auth.rs       Startup PAM auth against the macOS account (libpam FFI)
src/auth_guard.rs Connection-level auth hardening (Tier 1.2): per-source-IP
                  rate-limiting + escalating auto-expiring lockout + a greppable
                  `macrdp::audit` log, in front of the NLA/CredSSP gate. Pure,
                  platform-independent AuthGuardCore (decide/record_outcome, time
                  passed in for deterministic tests) + the ConnectionHandler
                  adapter (AuthGuardHandler) for the single-process path; the
                  --fork-workers supervisor (main.rs) drives the SAME core in its
                  own accept loop (classifying each worker by exit code + duration).
                  On by default, loopback-exempt, env-tunable (MACRDP_CONN_GUARD /
                  MACRDP_GUARD_* / MACRDP_AUDIT_LOG), zero vendored divergence
                  (reuses ironrdp-server's existing ConnectionHandler seam).
                  Lockout is heuristic (errored/short ⇒ failure, clean long
                  session resets) so a benign disconnect never locks anyone out.
src/capture.rs    ScreenCaptureKit → BgrA32 BitmapUpdate, dirty-rect driven
src/cursor.rs     NSCursor → RGBAPointer, hashed for change detection
src/input.rs      RDP scancodes/mouse PDUs → CGEvent synthesis (US ANSI by
                  default; non-US via src/keyboard_layout.rs),
                  per-side modifier state with NX_DEVICE bits, Caps Lock
                  toggle, AX-driven symbolic-hotkey workarounds
                  (Cmd+Tab app cycle, Cmd+` window cycle, Spotlight,
                  screencapture) since WindowServer's symbolic-hotkey
                  dispatcher won't fire for CGEventPost. The Cmd+Tab cycle
                  (also Option+Tab with --alt-tab-switch) makes the landing
                  app always surface — un-minimize / reopen-windowless
                  (open -b) / unhide, gated to the committed app — and, with
                  --app-switcher-hud, drives the macrdphud overlay via
                  src/switcher_hud.rs. Also the optional Ctrl→Cmd
                  Windows-shortcut remap (--map-ctrl-to-cmd): rewrites a
                  curated key set Ctrl+<k>→Cmd+<k> (post_ctrl_as_cmd),
                  suppressed when a terminal / --no-remap-apps app is
                  frontmost. Frontmost detection merges three last-wins
                  signals into LAST_FOCUS_BUNDLE — an NSWorkspace
                  activation observer (init_focus_observer, on the
                  runloop_thread), a mouse-down AX hit-test
                  (update_focus_from_click, AXUIElementCopyElementAtPosition),
                  and the AX MRU poll — because Electron apps take key
                  focus without activating.
src/switcher_hud.rs  App-switcher HUD IPC client (--app-switcher-hud). A bg
                  thread pushes SHOW/ADVANCE/HIDE (opcode+len framing, like
                  the smart-card bridge) over loopback to the macrdphud helper,
                  best-effort/non-blocking (try_send) so the input path never
                  stalls. input.rs calls it from cycle_apps/commit_cycle_session;
                  main.rs auto-spawns the helper + sets the captured display id.
                  The helper itself is gui/Sources/macrdphud/main.swift (a 2nd
                  SwiftPM target alongside the menu-bar controller): a borderless
                  non-activating NSPanel drawing an app-icon row that
                  ScreenCaptureKit captures, so the remote sees the switcher.
                  Cross-platform stub-free (pure std on the Rust side).
src/keyboard_layout.rs  Optional non-US layout translation
                  (--keyboard-layout). Resolves a name/KLID/input-source-id
                  to a macOS UCKeyboardLayout via TIS and translates
                  (keycode + mods) → Unicode with UCKeyTranslate, so input.rs
                  posts the right character for non-US clients WITHOUT changing
                  the Mac's active input source. Cmd/Ctrl combos stay on the
                  keycode path; dead keys compose via UCKeyTranslate state.
                  macOS-only (Carbon); the translatable-key set + spec parsing
                  are platform-independent and unit-tested.
src/rdpdr/        RDPDR drive redirection — the macrdp side of the server-side
                  RDPDR static channel (--enable-drive-redirection, opt-in,
                  read-write). The protocol state machine (handshake + device
                  discovery) lives in the vendored ironrdp-server::rdpdr; this
                  module is the MacRdpdr factory + RdpdrServerHandler backend.
                  1a: handshake + drive discovery. 1b: device I/O — the backend
                  browses/reads/writes the client drive via RdpdrHandle
                  (list_dir/read_file/write_file/create/remove/rename, matched
                  by a completion-id router in the vendored server; file
                  read/write reuse an LRU kept-open handle cache so sequential
                  transfers don't re-open per chunk). Surface
                  (Phase 2, surface.rs): a real NFS mount. RdpdrFs implements
                  nfsserve's NFSFileSystem over RdpdrHandle (list_dir/read_file,
                  with a path<->fileid cache); an in-process NFSv3 server is
                  mounted via the built-in mount_nfs (no root, no kext) so the
                  client's drive is a proper Finder volume with lazy subdir
                  navigation. Read-write: NFS write/create/mkdir/remove/rename/
                  setattr map to RDPDR DeviceWrite / DeviceCreate /
                  SetInformation. Each redirected filesystem device gets its own
                  mount (MacRdpdrHandler keeps a HashMap<device_id, Surface>).
                  Surface::Drop unmounts on disconnect. smartcard.rs: the
                  smart-card bridge (--enable-smartcard-redirection). When a
                  Smartcard device is announced, binds 127.0.0.1:40242 and serves
                  macrdp's own PC/SC IFD handler (ifd-handler/ cdylib, loaded by
                  com.apple.ifdreader), mapping its POWER_ON/OFF/TRANSMIT/PRESENCE
                  protocol to RdpdrHandle::scard_* (MS-RDPESC) calls on the
                  client's reader. Presence cached briefly; dropped on disconnect.
ifd-handler/      Standalone cdylib (NOT in the macrdp cargo package) — macrdp's
                  PC/SC IFD handler, the IFDHandler v3.0 C ABI macOS
                  SmartCardServices loads. Bridges every card op to macrdp over
                  loopback TCP (src/rdpdr/smartcard.rs). From scratch (MIT/Apache)
                  so smart-card redirection ships no GPL vpcd. Built + embedded in
                  macrdp.app by packaging/; installed by install-ifd-handler.sh.
src/clipboard.rs  CLIPRDR ↔ NSPasteboard (CF_UNICODETEXT + CF_DIB
                  + Mac↔Windows file copy via FileGroupDescriptorW
                  and FileContentsRequest streaming)
src/file_promise.rs  Windows→Mac EAGER download to /tmp + NSPasteboard
                     publish + Glass-chime auto-paste into Finder.
                     Default path; provides fetch_one_file (Arc<File>+
                     pwrite chunk fan-out) reused by the lazy path.
src/file_promise_lazy.rs  Windows→Mac LAZY paste (default; opt out
                          with --no-lazy-paste): pre-
                          sized empty temp file per leaf + one
                          NSFilePresenter each via NSFileCoordinator;
                          on Finder Cmd-V, relinquishPresentedItemToReader:
                          blocks while we fetch_one_file with
                          LAZY_PARALLEL_CHUNKS (2, vs eager's 8) so RDP
                          input stays responsive during the download.
                          macOS shows native "Preparing to paste" progress;
                          no Glass chime / auto-Cmd-V needed.
                          cleanup_on_disconnect drains presenters + temp
                          dir on Drop(MacCliprdrBackend); shutdown_cleanup
                          does the same via a process-global handle for
                          signal exit (which bypasses Drop).
src/runloop_thread.rs  Dedicated CFRunLoop-hosting std::thread with a
                       submit(closure) API. Exists because tokio owns
                       macrdp's main thread (no pumped runloop), and
                       NSFileCoordinator.addFilePresenter / removeFilePresenter
                       calls must land on a thread with a pumped CFRunLoop
                       to deliver presenter callbacks. Wakes via a custom
                       CFRunLoopSource; one shared thread for the process
                       lifetime, started lazily on first submit().
src/audio.rs      RDPSND ← second SCK stream with system-audio capture,
                  rubato 48→44.1 kHz resample, latency-bounded. Ships raw
                  PCM by default, or AAC-LC via src/aac.rs (--enable-aac).
                  Capture loop self-heals a dead SCK stream: rebuilds the
                  SCStream with capped exponential backoff (250 ms→5 s) on
                  start failure OR mid-stream end (the async stream yielding
                  None), instead of going silent for the rest of the session;
                  backoff resets on the first delivered sample. The 'reconnect
                  outer loop preserves my_gen, so the generation guard still
                  retires it on client reconnect (no double-capture).
src/aac.rs        AudioToolbox AAC-LC encoder (--enable-aac). AudioConverter
                  FFI: interleaved i16 PCM → raw AAC access units for the
                  WAVE_FORMAT_AAC_MS RDPSND path. macOS-only.
src/virtual_display/    Opt-in headless display via undocumented
  mod.rs                CGVirtualDisplay* private API. Public Rust
  private_api.rs        surface is `VirtualDisplay::new(w,h,hz)` +
                        display_id/origin_pts/size_pts; ALL touches to
                        private Obj-C classes/symbols are confined to
                        private_api.rs (the maintenance boundary —
                        when Apple changes the API in a future macOS,
                        update only that file).
src/h264.rs       EGFX/H.264 video pipeline (opt-in via --enable-h264).
                  Bridges the VideoToolbox encoder (src/videotoolbox.rs) to
                  upstream's GraphicsPipelineServer: per SCK frame, encode →
                  non-blocking drain → AVC420 (Annex-B framing) → DRDYNVC →
                  ServerEvent::Egfx. Uses upstream's auto-allocated surface id
                  (see the mstsc reconnect-blank quirk note below). Falls back
                  to legacy BitmapUpdate for clients that don't advertise
                  AVC420 decode.
src/videotoolbox.rs  VideoToolbox H.264 encoder (AVCC NALs + SPS/PPS).
                  Feeds VT a full-range BT.709 NV12 (420f) buffer it builds
                  from the captured BGRA — VT would otherwise emit video-range
                  YUV, which mstsc renders washed-out. The BGRA→NV12 conversion
                  is vImage (Accelerate/NEON) accelerated, ~24-32x over the
                  scalar reference kept as a fallback + benchmark baseline.
src/multitransport.rs  macrdp-side RDP UDP multitransport provider
                  (--enable-udp-multitransport, feature `multitransport`,
                  default OFF). Thin: a MacMultitransport that tells the
                  vendored server to offer reliable UDP (UdpFecR). The transport
                  itself (RDPEUDP state machine + RDPEUDP2/EMT codecs) is the
                  sans-I/O vendor/ironrdp-rdpeudp crate; the UDP listener + rustls
                  TLS + MS-RDPEMT tunnel + DYNVC Soft-Sync EGFX migration live in
                  vendored ironrdp-server (src/multitransport/, divergence (12))
                  + vendored ironrdp-dvc (Soft-Sync codec). main.rs binds the
                  listener on the TCP address/port and wires the cookie registry +
                  the server↔listener tunnel handoff. EGFX-over-UDP rendering is
                  additionally env-gated by MACRDP_UDP_MIGRATE_EGFX until soaked.
                  Cross-platform (pure protocol policy); byte-exact Soft-Sync +
                  cookie-registry + Initiate-Request tests live here (the vendored
                  crates are test=false). See docs/rdp-udp-multitransport-
                  feasibility.md.
src/logging.rs    Tracing sink + size-based log rotation. By default stdout
                  (interactive). When headless (stdout not a TTY, e.g. under the
                  LaunchAgent) or given --log-dir/LOG_DIR, writes a self-owned,
                  size-bounded rotating file at ~/Library/Logs/macrdp.log — stable
                  live name (the GUI reads it + scans for "panicked") rotating
                  logrotate-style to macrdp.log.1..N (MACRDP_LOG_MAX_BYTES /
                  MACRDP_LOG_MAX_FILES). Custom (not tracing-appender, which only
                  does dated files; non_blocking was reverted) BLOCKING writer via
                  a MakeWriter over Arc<Mutex<Rotator>>. On the file path it also
                  installs a panic hook that routes panics through tracing into
                  macrdp.log (observability only — cleanup is the reaper's job).
                  The plist drops StandardOutPath (the binary owns the file) and
                  points StandardErrorPath at a small macrdp.err.log.
src/reaper.rs     Startup reaper — on launch, sweeps leftovers from a PRIOR macrdp
                  that died uncleanly (SIGKILL/panic/power-loss skip Drop AND the
                  signal handler): stale NFS mounts + $TMPDIR/macrdp-rdpdr-<pid>/
                  and macrdp-{paste,lazy-paste}-<pid>-* dirs. Holds the shared,
                  platform-independent primitives (process_is_alive via
                  libc::kill(pid,0)→ESRCH; pid_from_tagged; for_each_stale);
                  per-module reap_stale fns live next to each module's
                  shutdown_cleanup (rdpdr/surface.rs, file_promise*.rs). Only
                  DEAD-pid, non-self dirs are reaped, so it's safe with another
                  instance live. Called once from async_main on a detached thread
                  (a stale umount can't block startup); covers single-process,
                  the fork supervisor, and each worker.
src/health.rs     Health-check watchdog (Tier 2.5) — turns a hung-but-alive
                  process into a clean exit so launchd KeepAlive / the
                  --fork-workers supervisor restarts a fresh one (KeepAlive only
                  catches an outright crash, not a wedge). A dedicated OS thread
                  (NOT a tokio task, so it ticks even when the runtime is wedged)
                  submits a trivial probe onto the tokio runtime each interval and
                  waits a bounded time; a deadlocked runtime never runs it, and
                  after N consecutive misses it process::exits with code 70. Pure,
                  unit-tested decision + parsing (should_arm / HealthConfig); armed
                  from async_main on the long-lived launchd-watched process
                  (single-process OR supervisor), skipped on short-lived fork
                  workers and (by default) interactively (stdout a TTY).
                  Conservative defaults (15s interval / 30s timeout / 2 misses ⇒
                  ~90s to bounce). Env: MACRDP_HEALTHCHECK=0/1 +
                  MACRDP_HEALTHCHECK_{INTERVAL_SECS,TIMEOUT_SECS,FAILURES}
                  (config.env HEALTH_CHECK / HEALTHCHECK_*). Cross-platform.
build.rs          Bakes Xcode Swift-runtime rpath into the final binary

vendor/ironrdp-server/    Local fork of ironrdp-server 0.10.0, pulled in via
                          [patch.crates-io] in Cargo.toml. The live
                          divergences (audio-lag tracker, resize-stall
                          resync, per-batch dispatch priority, SuppressOutput
                          handling, NSCodec encoder, opt-in QOI Rgb
                          workaround, honor-client-desktop-size plumbing)
                          are documented in
                          vendor/ironrdp-server/CLAUDE.md — that nested
                          memory loads when you work in the fork. Keep the
                          vendor dir until all of those are upstreamed AND
                          released.

vendor/ironrdp-acceptor/  Local fork of ironrdp-acceptor 0.8.0 (added
                          2026-06-12, same upstream rev as the git pins).
                          Two divergences: (1) honor_client_desktop_size —
                          adopt the client's requested desktop size from its
                          GCC Client Core Data BEFORE Demand Active, which
                          is what powers the default client-resolution
                          auto-adopt (--no-client-resolution opts out); and
                          (2) expose the client's keyboard-layout id (KLID)
                          from the same core data on AcceptorResult, which
                          powers default non-US keyboard auto-detect (the
                          vendor server publishes it; input.rs auto-selects
                          the layout). See vendor/ironrdp-acceptor/CLAUDE.md
                          and the client-resolution / keyboard-layout quirk
                          notes for why these can't be done from server code.

vendor/ironrdp-rdpdr/     Local fork of ironrdp-rdpdr 0.5.0 (added 2026-06-16,
                          same upstream rev). Upstream is client-only
                          (SvcClientProcessor); the fork adds the
                          server-direction decode halves the PDUs lack
                          (ClientName/DeviceListAnnounce/DeviceAnnounceHeader)
                          so a server can read what the client sends. The
                          server-side RdpdrServer processor itself lives in
                          vendor/ironrdp-server/src/rdpdr.rs. See
                          vendor/ironrdp-rdpdr/CLAUDE.md.

vendor/ironrdp-rdpeudp/   NEW sans-I/O crate (added 2026-06-25) for RDP UDP
                          multitransport (--enable-udp-multitransport, feature
                          `multitransport`, default OFF). No sockets/tokio: PDU
                          codecs for RDPEUDP v1 (pdu.rs, big-endian) + RDPEUDP2
                          (eudp2.rs) + the MS-RDPEMT tunnel PDUs (emt.rs), and the
                          reliable transport state machine (state.rs: handshake +
                          in-order dedup delivery + cumulative-ACK + RTO retransmit)
                          driven by the listener. Candidate for upstream. See
                          vendor/ironrdp-rdpeudp/CLAUDE.md.

vendor/ironrdp-dvc/       Local fork of ironrdp-dvc 0.5.0 (added 2026-06-26, 4th
                          fork). Adds the server-direction MS-RDPEDYC **Soft-Sync**
                          codec (SoftSyncRequest/Response PDUs + the client
                          Soft-Sync-Response decode arm) so the server can move a
                          DVC (EGFX) onto the UDP tunnel and not tear down on the
                          client's reply. Patch wiring is TWO-SIDED (it's a path dep
                          of the git-pinned ironrdp crates) — see its CLAUDE.md.
                          Soft-Sync rides drdynvc on the MAIN TCP connection; only
                          channel data after the switch rides the UDP tunnel.

(vendor/ironrdp-egfx/     DELETED 2026-05-25. The CapabilitySet::decode
                          tolerance fix was merged upstream as PR #1298
                          (Devolutions/IronRDP@67f3c63). We bumped the
                          ironrdp rev in [patch.crates-io] to that commit
                          and the vendor dir is gone. If you're seeing this
                          comment in a stale checkout — the dir is not
                          missing, it just stopped existing.)

(vendor/ironrdp-cliprdr/  DELETED 2026-05-25. All THREE divergences
                          (on_format_list_response hook #1300,
                          Preferred DropEffect advertise+inline-response
                          #1301, always-SHOW_PROGRESS_UI FD flag #1299)
                          merged upstream the same day. Ironrdp pinned
                          at-or-after Devolutions/IronRDP@879ffed8 has
                          all three; vendor dir gone. If you're seeing
                          this comment in a stale checkout — the dir is
                          not missing, it just stopped existing.)
```

Cross-cutting:
- **TLS** terminates inside the acceptor; `rustls` with, by default, a self-signed cert at `~/Library/Application Support/macrdp/{cert,key}.pem` (generated on first run, persisted thereafter for stable client TOFU). **An operator can supply a real CA / ACME cert via `--cert`/`--key` (or `TLS_CERT`/`TLS_KEY`)** — `make_tls_acceptor` then loads exactly those PEM files and never silently self-signs (a missing/bad file is a hard error; it also warns at startup if the cert is expired / within 14 days). Both the self-signed and operator paths flow through the same `load_pem_cert_and_key` → so the rest is identical. `RdpServerSecurity::Hybrid` is used so the negotiation response advertises CredSSP — the public-key bytes handed to ironrdp are the raw `subjectPublicKey` BIT STRING from the X.509 cert (not the SPKI sequence, not the keypair-derived bytes), since that's what sspi hashes client-side. The same loaded cert/key also secure the UDP multitransport (TLS for the reliable flow, DTLS for the lossy flow), so operator certs apply there too.
- **Auth** at startup: `--username` (defaults to `$USER`) + interactive password prompt → PAM `checkpw` service → set as the static credential ironrdp_server checks per-connection. `--skip-auth` bypasses for dev.
- **Session model** — by default macrdp attaches to the console session of the logged-in user (single session, mirrors the primary panel). With `--virtual-display --width W --height H`, the server instead allocates a headless `CGVirtualDisplay` and serves *that*; the local Mac screen is untouched and the remote sees its own desktop at the requested resolution. The CG-side display is owned by `main()`'s scope, registered via `[CGVirtualDisplay initWithDescriptor:]` + `applySettings:`, and torn down on normal exit (signal-driven `std::process::exit(0)` skips Drop, but macOS reaps the registration when the owning process dies). Capture / input / cursor all parameterize on `(displayID, origin_pts, size_pts)` so they target the right surface regardless of which path is in effect.
- **Process model — single-process by default; `--fork-workers` is multi-process (xrdp's model).** Normally one `macrdp` process does everything (accept → capture/encode/serve) for the lifetime of the server. Opt-in `--fork-workers` (default OFF; `FORK_WORKERS=1` in config.env) splits it: a thin **supervisor** binds the port and does NO capture, and for every inbound connection `fork+exec`s a **fresh worker process** of the same binary, handing it the already-accepted socket fd via `MACRDP_WORKER_FD` (its presence marks a worker and takes precedence over `--fork-workers`, so a re-passed flag / `--config` re-expansion can't recurse). The worker serves exactly ONE connection then `std::process::exit`s (a normal return would leave it alive — SCK framework threads block exit — leaking SCStreams). The supervisor **serializes** (drains the previous worker before spawning the next, avoiding an SCStream-slot overlap) and owns all **persistent** state so it survives worker churn: the virtual display (workers capture it by id via `MACRDP_VD_ID`), headless blanking (`--capture-primary`/`--detach-primary`, engaged on first connect, process-scoped so it auto-restores on supervisor death), `caffeinate`, and the app-switcher HUD helper (`:40243`; workers only push to it). This exists to beat mstsc's EGFX reconnect-blank (a fresh *process* dodges the client-side surface-retention bug) — see the H.264 reconnect-blank quirk. `run_fork_supervisor` + the worker branch live in `src/main.rs`. macOS-only; mirror-primary or `--virtual-display`. Under launchd the supervisor IS the job (KeepAlive watches it; `bootout` kills the worker children via the job's process group).
- **Signal handling** — `main.rs` spawns a task that awaits SIGINT/SIGTERM and `std::process::exit(0)`s. Without it, ScreenCaptureKit's framework threads can leave the process unkillable by Ctrl-C once an SCStream is active. (With `--fork-workers` the supervisor's forced exit also relies on its headless blanking being process-scoped — it auto-restores the physical display on death — and on launchd's process-group kill to reap workers.)
- **Audio rate** — SCK only supports 8/16/24/48 kHz, so capture is at 48 kHz, but `src/audio.rs` resamples to 44.1 kHz via `rubato` before sending. 44.1 matches the native rate of most Windows audio endpoints, so the client plays directly without internal resampling — which used to cause a ~20% sustained over-feed and multi-second audio backlogs on mstsc. The advertised RDPSND `AudioFormat` is therefore 44.1 kHz / 2 ch / 16-bit.
- **Single capture loop** — `MacRdpsnd` (the audio factory) holds an `Arc<AtomicU64>` generation counter shared with every backend it builds. Each `start()` claims a fresh generation; older capture loops observe the bump on their next iteration and exit. Without this, an mstsc cert-prompt reconnect leaves the first capture loop running while the second starts, both feeding the shared event channel → ~2× audio reaching the client.

When adding a feature, locate it in one of those modules first; if it spans them (e.g., a new virtual channel), it belongs in a dedicated module alongside `clipboard.rs`, driven by `ironrdp_server`'s factory traits.
