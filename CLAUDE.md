# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

> Layout note: the big reference sections live in separate files and are pulled
> in via `@import` below, so the full context still loads every session.
> - `@docs/architecture.md` — module map + cross-cutting design
> - `@docs/macos-gotchas.md` — TCC, CGVirtualDisplay, QoS, activation
> - `@docs/known-quirks.md` — hard-won client/codec/audio behavioural notes
>
> The `vendor/ironrdp-server/` and `vendor/ironrdp-acceptor/` forks each have
> their own nested `CLAUDE.md` (the divergence logs) that load only when you
> work inside those directories.

## Status

Functional v0. RDP clients (mstsc, Microsoft Remote Desktop, FreeRDP) can:
- Connect over TLS to the Mac on port 3390 with a local Mac username/password.
- See the primary display at native resolution with incremental damage-region updates.
- **Get the session served at the resolution the client asks for, by default** (client-resolution auto-adopt; `--no-client-resolution` opts out). The vendored acceptor reads the client's requested desktop size from its GCC Client Core Data and negotiates the session at that size from the very first Demand Active — e.g. mstsc full-screen on a 1920×1080 monitor gets a 1920×1080 session instead of the Mac's 1512×982, so the client presents 1:1 with no client-side rescale (mstsc's rescale costs typing latency and, with `--enable-h264`, audio drift). Applies only on the mirror-primary path with no explicit `--width`/`--height`/`--hidpi`/`--virtual-display`. The usual non-native-capture trade-offs apply (full-frame legacy updates; and on an aspect mismatch the picture is **letterboxed/pillarboxed to preserve the Mac's aspect ratio by default** — `--stretch` opts back into fill-and-distort, with input mapped into the centered picture either way — see the scaling quirk note). Verified live on FreeRDP (legacy + H.264, incl. reconnect at a different size); see the quirk note below for why this *must* live in the acceptor.
- Optionally capture the primary display at its **backing (Retina) pixel resolution** (`--hidpi`, e.g. 3024×1964 instead of 1512×982 logical points) so clients render crisp native pixels instead of upscaling a point-density frame. Opt-in (it's ~4× the pixels); the win is biggest with `--enable-h264`. Verified crisp; input/cursor are resolution-correct. **Caveat:** mstsc decodes 4× the pixels per frame and feels laggy at HiDPI — Thincast/FreeRDP stay snappy. See the HiDPI quirk note below.
- Optionally stream the display as **H.264 over EGFX** (`--enable-h264`, AVC420, Annex-B framing, VideoToolbox-encoded) — far less bandwidth than legacy bitmaps. Verified rendering on mstsc, on FreeRDP built with H.264 decode, and on the macOS Windows App / Microsoft Remote Desktop client (it decodes AVC420 over EGFX — only its *legacy* bitmap-codec list is NSCodec-only). Clients that genuinely don't advertise AVC420 decode (e.g. a decoder-less FreeRDP build) fall back to legacy BitmapUpdate automatically. **Caveat:** reconnecting *mstsc* to a still-running macrdp can show a blank screen (mstsc-specific EGFX surface-handling quirk — confirmed not a server bug, since FreeRDP reconnects cleanly); reliable workaround is to fully close and reopen the mstsc window (clears its surface cache). See the H.264 quirk note below.
- Drive keyboard and mouse, including modifier keys (per-side L/R tracking with NX_DEVICE bits, Caps Lock as a toggle, MS-RDPBCGR Synchronize lock-state reconciliation), mouse buttons, and wheel. Keyboard input is positional scancode→macOS keycode by default — but **non-US layouts are served by translating each typing key against the client's layout via Carbon `UCKeyTranslate` and posting the resulting character as a Unicode string, without changing the Mac's own input source**. The layout is **auto-detected from the client's announced KLID by default** (US/unknown keep the plain keycode path, so the US majority is unaffected); `--keyboard-layout <name|KLID|input-source-id>` (e.g. `french`, `de`, `0x040C`) forces a specific layout, `--keyboard-layout none` disables translation. Cmd/Ctrl combos stay on the keycode path so shortcuts work; dead keys (´+e→é) compose via UCKeyTranslate's persistent state. See `src/keyboard_layout.rs` and the keyboard-layout quirk note.
- Optionally **remap Windows editing shortcuts from Ctrl to Cmd** (`--map-ctrl-to-cmd`, opt-in) so Windows muscle memory drives macOS copy/paste: a curated key set (C V X A Z S F N T W O P R G, plus Shift variants like `Ctrl+Shift+Z`→redo) fires as the Cmd equivalent. Off by default (Q excluded so `Ctrl+Q`≠`Cmd+Q`-quit; nav keys untouched). **Auto-suppressed when a terminal is frontmost** so `Ctrl+C` stays SIGINT — built-in standalone-terminal list plus `--no-remap-apps <bundle,…>` for editors with an embedded terminal (e.g. `com.microsoft.VSCode`), where Ctrl then stays Ctrl everywhere (editor copy uses Cmd+C, the app's native macOS copy). Detecting the frontmost app in a headless session needed three last-wins signals (NSWorkspace activation observer + mouse-down AX hit-test + AX poll) because Electron apps take *key* focus without *activating*; see `src/input.rs` and the Ctrl→Cmd quirk note. From Devolutions' IronRDP CTO feedback (pairs with the legacy-codec startup nudge toward `--enable-h264`).
- Forward macOS symbolic hotkeys that WindowServer's dispatcher refuses to fire for user-space CGEventPost: Cmd+Tab / Cmd+Shift+Tab cycle apps via Accessibility API (per-bundle dedup with MRU, dead-pid filtering via `kill(pid, 0)`; `--alt-tab-switch` additionally accepts Option+Tab / Option+Shift+Tab as the same trigger — committing on Option release — for clients that forward Alt+Tab but gate Win+Tab). The app you **release on always surfaces** — a window is raised+AXMain'd, a minimized app is un-minimized (with a deferred re-raise to beat the genie animation), and a running-but-windowless app (Notes/Calendar/Mail with their window closed) is reopened via `open -b` — all gated to the landing app so apps merely cycled *through* don't pop/flicker. Optionally **`--app-switcher-hud`** spawns a separate Swift helper (`macrdphud`, in `gui/`) that draws a real non-activating overlay panel (icon row, like native Cmd+Tab) which ScreenCaptureKit captures so the remote client sees the switcher; macrdp pushes SHOW/ADVANCE/HIDE to it over loopback (`src/switcher_hud.rs`), best-effort. Cmd+\` / Cmd+Shift+\` cycle windows of the current app (AXRaise + window AXMain + app AXMainWindow for Electron compatibility), Cmd+Space invokes Spotlight via AppleScript, Cmd+Shift+3/4/5 shell out to `/usr/sbin/screencapture` or open Screenshot.app.
- See the real macOS cursor shape (I-beam, hand, etc.) overlaid by the client.
- Copy/paste UTF-8 text and images (CF_DIB ↔ PNG) between Mac and remote.
- Mac→Windows file copy, including whole folders: copying a file or directory in Finder and pasting on Windows produces a real file/tree in Explorer. The pasteboard walk recurses into directories (skipping symlinks, capped at 10 000 descriptors per copy) and emits one FILEGROUPDESCRIPTORW entry per leaf with `relative_path` set so upstream's wire encoder reconstructs the right `MyFolder\sub\file.txt` cFileName. Bytes stream via MS-RDPECLIP `FileContentsRequest` SIZE + RANGE chunks (4 MiB per chunk). Reaches upstream `Cliprdr::initiate_file_copy` via the vendored `ServerEvent::ClipboardFileCopy(Vec<FileDescriptor>)` variant — that's the only API that populates `local_file_list`, without which upstream short-circuits every byte fetch with CB_RESPONSE_FAIL. Finder hands out *file-reference* URLs (`/.file/id=...`); we resolve them through `NSURL::URLByResolvingSymlinksInPath` because `std::fs::metadata` can't stat them directly.
- Windows→Mac file copy (single files OR folder trees via Ctrl-A+Ctrl-C inside a folder; raw Ctrl-C on a folder doesn't work — see caveat below). Two paths, switched by `--no-lazy-paste` (lazy is the default):
  - **Lazy (default, `src/file_promise_lazy.rs`):** create a pre-sized empty temp file per leaf, register one `NSFilePresenter` per file via `NSFileCoordinator.addFilePresenter:`, publish only the top-level `NSURL`s to NSPasteboard. Bytes stream only when Finder's Cmd-V triggers the coordinator's `relinquishPresentedItemToReader:` callback, during which we synchronously fetch chunks (1 MiB × 2 in flight, `LAZY_PARALLEL_CHUNKS` — lower than eager because the user is actively interacting at paste time and a higher count visibly stutters RDP input) into the pre-allocated file, then invoke `reader(nil)`. macOS shows its native "Preparing to paste" progress dialog during the wait. No Glass chime / auto-Cmd-V needed (the user pasted; Finder handles it). On fetch failure we delete the temp file so Finder errors loudly rather than silently copying a zero-padded ghost.
  - **Eager (`--no-lazy-paste`, `src/file_promise.rs`):** when Windows announces a `FileGroupDescriptorW` we download every entry to `/tmp/macrdp-paste-<pid>-<nanos>/` via parallel `FileContentsRequest` chunks (1 MiB × 8 in flight, `EAGER_PARALLEL_CHUNKS`), recreating any directory structure encoded in each descriptor's `relative_path`, then publish the top-level entries to NSPasteboard as real `NSURL`s. On completion we play `/System/Library/Sounds/Glass.aiff` (`afplay` bypasses notification permissions; `osascript display notification` is silently suppressed because macOS attributes the banner to the unsigned macrdp binary) and, *only if Finder is the frontmost app*, fire `Cmd-V` via System Events so the paste the user attempted finishes automatically. Kept as a fallback for users who prefer the up-front download + auto-paste UX, and for any file whose descriptor lacks a size (lazy falls back to eager automatically in that case). Both paths share `paste_temp_dir` + `self_change_count` on the backend and clean up on disconnect (Drop on `MacCliprdrBackend`) and signal exit (`shutdown_cleanup()` via process-global handle, since `std::process::exit` bypasses Drop).
  - Both paths use the same `resolve_dest` for `relative_path` sanitization (rejects `.`, `..`, embedded `/`) so a malicious remote can't escape the temp sandbox; both share the same `fetch_one_file` chunk fan-out (pwrite via `FileExt::write_at` over an `Arc<File>`, no per-chunk open+seek+close); both rely on `CAN_LOCK_CLIPDATA` being negotiated (see clipboard.rs `client_capabilities`) so cliprdr auto-issues Lock/Unlock around the descriptor — without that cap, Windows treated the descriptor as ephemeral and would silently drop rapid follow-up Ctrl-C *and* release file data mid-stream on large downloads (CB_RESPONSE_FAIL). A `SelfChangeCount` atomic stops our own NSPasteboard write from being rebroadcast to Windows by the change-count poller.

  **Ctrl-C on a folder in Windows Explorer is a known no-op** — not our bug, and not fixable from the server side. Explorer puts `CFSTR_SHELLIDLIST` (Shell IDList Array) on the clipboard as the primary format and delay-renders `FileGroupDescriptorW` only when a shell-aware receiver asks. mstsc doesn't request the delayed format, so it never forwards anything via CLIPRDR — `cliprdr=debug` shows zero PDUs for the folder copy attempt. Workaround for the user: enter the folder in Explorer, `Ctrl-A` then `Ctrl-C` to copy the contents (with directory descriptors for any subfolders) — that path uses `FileGroupDescriptorW` directly and forwards correctly. True drag-from-Windows folder copy would need drive redirection (a different RDP feature, not clipboard).
- Forward macOS system audio to the remote (RDPSND, 44.1 kHz stereo 16-bit PCM; SCK captures at 48 kHz and the capture loop resamples via `rubato`). Optionally compress it as **AAC-LC** (`--enable-aac`, `WAVE_FORMAT_AAC_MS` over RDPSND, ~128 kbps vs PCM's ~1.4 Mbit/s) — AudioToolbox-encoded (`src/aac.rs`), raw access units, advertised ahead of PCM so clients that decode AAC negotiate it while everyone else falls back to PCM automatically. Opt-in because AAC adds ~40–50 ms of encoder priming latency, so PCM stays the zero-latency LAN default. See the AAC quirk note below.
- NLA / CredSSP authentication — no more "type username before Connect" mstsc workaround.
- Optionally attach a **headless virtual display** (`--virtual-display --width W --height H`) and serve that to the client instead of mirroring the primary panel — behaves like plugging in an external monitor, so the local Mac screen stays available while the remote session has its own desktop at any requested resolution. Backed by undocumented `CGVirtualDisplay*` private API; see the maintenance note below.
- Optionally go **fully headless while a client is connected** via one of two mechanisms (mutually exclusive):
  - `--virtual-display ... --detach-primary`: disables every active physical display at the WindowServer level once the first RDP client actually connects (private `CGSConfigureDisplayEnabled`). Backlight off, no menu bar, cursor can't cross over. Cleaning a stale detach if macrdp dies hard happens automatically — the detach uses `CGConfigureForAppOnly` so SIGKILL / panic / power loss trigger an OS-level revert with no logout required. **Caveat:** on some macOS versions / displays the disable transaction succeeds but the panel keeps showing the desktop; if that's the case, use `--capture-primary` instead.
  - `--virtual-display ... --capture-primary`: takes exclusive `CGDisplayCapture` of every physical display once a client connects AND forces each panel's gamma LUT to map every input to black via `CGSetDisplayTransferByFormula(_, 0,0,1, 0,0,1, 0,0,1)`. Capture alone doesn't visually blank modern macOS panels (the "fill with black on capture" semantic disappeared around 10.10) — the gamma trick is what actually makes the panel render solid black while the WindowServer keeps compositing the desktop to it. Backlight stays on, cursor sunk by the capture. Both gamma changes and capture tokens are process-scoped, so SIGKILL / panic auto-restores. Uses only public CG symbols — no private SkyLight surface, no `CGError 1001` window.
  Either way, the original layout is restored the moment the last client disconnects; local Mac usage is normal whenever no one is connected.

Not yet implemented: multi-monitor (client-side multi-display), printer redirection. (Non-US keyboard layouts work, **auto-detected from the client by default** — `--keyboard-layout` overrides, `none` disables.) **Drive redirection (RDPDR)** behind `--enable-drive-redirection` (opt-in, **read-write**): the connecting client redirects its local drive(s) and the Mac mounts **each** as its own **real volume** (one NFS mount per redirected filesystem device, keyed by device id). Phase 1a — MS-RDPEFS handshake + drive discovery (verified FreeRDP **and** mstsc); 1b — device I/O via `RdpdrHandle` (`read_file`, `list_dir`, matched by a completion-id router); Phase 2 — the macOS surface (`src/rdpdr/surface.rs`) is a **real NFS mount**: `RdpdrFs` implements `nfsserve`'s `NFSFileSystem` over the `RdpdrHandle` (a path↔fileid cache), an in-process NFSv3 server is mounted via the built-in `mount_nfs` (**no root, no kext, no FUSE**), and the OS's VFS drives lazy lookups as the user browses — so **full subdirectory navigation** works (the Phase-1c temp-folder/`NSFilePresenter` mirror was top-level-only and is replaced). **Writes** map NFS ops to RDPDR: `write_file` (`DeviceWrite`), `create`/`mkdir` (`DeviceCreate`), and truncate/delete/rename (`SetInformation` FileEndOfFile/Disposition/Rename) — so editing, copying-in, `mkdir`, `mv`, and `rm` all work (verified byte-exact on FreeRDP and on mstsc, writing to a folder the redirected user owns — a `STATUS_ACCESS_DENIED` from a non-writable target like the `C:\` root is Windows' own ACL, surfaced as `NFS3ERR_ACCES`). `setattr` honors size only (mode/times are no-ops). Unmounted on disconnect (`Surface::Drop`). See the vendored `ironrdp-rdpdr` / `ironrdp-server` divergence logs and the RDPDR quirk notes.

**Smart-card redirection (RDPDR / MS-RDPESC)** behind `--enable-smartcard-redirection` (opt-in): the connecting client redirects its **smart-card reader** and **macOS PC/SC apps use the card through it** — the standard client→server direction, so the card stays on the client while the Mac in the session reads it. The card is announced as an RDPDR `Smartcard` device and PC/SC calls ride that channel as `SCARD_IOCTL_*` device-control IOCTLs (MS-RDPESC), bodies NDR/RPCE-marshaled. Server-direction MS-RDPESC lives in the vendored `ironrdp-rdpdr` (`pdu/esc`, divergence (2)): `HeaderlessEncode` for the `*Call` set the server sends + `HeaderlessDecode` for the `*Return` set + the **`ScardControlRequest`** DR_CONTROL_REQ envelope; the async `RdpdrHandle::scard_*` methods (establish/list/get-status/connect/status/transmit/disconnect) + the completion-id router are in the vendored `ironrdp-server` (`src/rdpdr.rs`, divergence (11)). The macOS side is **macrdp's own PC/SC IFD handler** (`ifd-handler/` — a from-scratch MIT/Apache cdylib, so **no GPL `vpcd`**) loaded by `com.apple.ifdreader`; `src/rdpdr/smartcard.rs` is a loopback-TCP bridge (port 40242, `MACRDP_SCARD_PORT`) that translates the handler's `POWER_ON`/`POWER_OFF`/`TRANSMIT`/`PRESENCE` protocol into `scard_*` calls. Full flow: macOS app → PC/SC → our IFD handler → bridge → MS-RDPESC over RDPDR → client → physical card → back. **Verified end-to-end on mstsc** with a Windows **TPM virtual smart card** (card-free test path): establish-context / list-readers / get-status-change (ATR) / connect / a full APDU transceive (GIDS `SELECT` → FCI + `90 00`). Six real-Windows NDR conformance edges the offline round-trips couldn't catch were fixed during verification (8-byte pointer-sized `SCARDCONTEXT`/`SCARDHANDLE`; NULL-referent value sections on `mszReaderNames` / `pbExtraBytes` / `pbRecvBuffer` / the **empty embedded Context of a returned handle** — that last one made the connect handle decode `cbHandle=0` and Windows tore down the session); plus the bridge fills the handle's context with the established context for requests, reads the ATR from `GetStatusChange` (real Windows rejects our `Status_Call` params), caches presence 300 ms (macOS CryptoTokenKit polls `IFDHICCPresence` tens of times/sec → would flood RDP), and caps `cbRecvLength` at 8 KiB (Windows rejects 0x10000). **Deployment:** the IFD handler ships embedded in `macrdp.app/Contents/Resources/ifd-macrdp.bundle` and installs (once, privileged — one GUI admin prompt) into `/usr/local/libexec/SmartCardServices/drivers` via `packaging/install-ifd-handler.sh`; macOS loads a third-party IFD driver **only on a USB hotplug** matching its Info.plist VID/PID, so a headless server needs a USB device permanently attached (bind it with `IFD_VID`/`IFD_PID`). macOS-only. See the vendored `ironrdp-rdpdr` (divergence (2)) / `ironrdp-server` (divergence (11)) logs.

**RDP UDP multitransport (MS-RDPEMT / MS-RDPEUDP / MS-RDPEDYC Soft-Sync)** behind `--enable-udp-multitransport` (opt-in, gated by the `multitransport` cargo feature; default OFF). Serves the **EGFX (H.264) channel over a reliable UDP tunnel** instead of TCP. **Scope (confirmed by a lossy-link soak 2026-06-26): this is a clean-link / low-loss feature, NOT a lossy-link win.** A reliable *ordered* RDPEUDP stream head-of-line-blocks on its own loss exactly like TCP, so under real loss EGFX-over-UDP freezes — and so does EGFX-over-*TCP* under the same shaping (the freeze is H.264-under-loss on an ordered stream, not a transport bug). The genuine loss-resilience win needs Phase 2 (lossy `UdpFecL` + FEC), deferred. What Phase 1 *does* deliver: a working server-side UDP data path + channel isolation on a clean link. As far as is known this is the **first open-source RDP *server* with a working UDP multitransport data path** (FreeRDP, the most complete OSS stack, has **no working UDP data path on either side** — its server is a TCP-side bootstrap stub and its client declines UDP with `E_ABORT`; the RDPEUDP/RDPEUDP2 work is out-of-tree prototype only, never merged. Re-verified against FreeRDP git history 2026-06-26. xrdp/ogon/gnome-remote-desktop/Weston are TCP-only). The whole transport (RDPEUDP v1 reliability state machine, RDPEUDP2 codecs) lives in a new sans-I/O crate `vendor/ironrdp-rdpeudp/`; the listener + TLS + tunnel + Soft-Sync live in the vendored `ironrdp-server` (`src/multitransport/`, divergence (12)) and vendored `ironrdp-dvc` (Soft-Sync codec). Flow: the **acceptor** offers multitransport after licensing → the client opens a UDP flow → RDPEUDP **v2 reliable** handshake (mstsc uses v2-carrying-TLS, not EUDP2) → **rustls** TLS over the reliable stream (same cert as TCP) → **MS-RDPEMT tunnel** with **strict cookie binding** (CSPRNG cookie, one-time-use registry, bound to the TCP session) → a **DYNVC Soft-Sync** request migrates the EGFX DVC onto the tunnel → EGFX H.264 PDUs ride the tunnel as `RDP_TUNNEL_DATA` (bare DRDYNVC PDUs), both directions. **EGFX migration itself is additionally gated by the experimental `MACRDP_UDP_MIGRATE_EGFX=1` env var** (default off → EGFX stays on TCP, the proven empty-Soft-Sync safe spike) until it's soaked. **Verified rendering end-to-end on real mstsc (Win11/WiFi).** Only EGFX video rides UDP today — input, audio (RDPSND), clipboard still ride TCP by design for this phase. **Mutually exclusive with `--fork-workers`** (the UDP path needs one persistent socket on the port that survives reconnects, which a per-connection worker can't own — the supervisor owns the port; with both set the supervisor warns, each worker still *offers* multitransport but binds no listener, so the client falls back to TCP — see the `--fork-workers` flag note). Phase 2 (lossy `UdpFecL` + DTLS via `boring` + FEC) is deferred. See `docs/rdp-udp-multitransport-feasibility.md` and the vendored `ironrdp-server` / `ironrdp-rdpeudp` / `ironrdp-dvc` divergence logs.

## Project goal

A native RDP server for macOS written in Rust on top of [`ironrdp`](https://github.com/Devolutions/IronRDP). Functionally analogous to `xrdp` on Linux: Windows / cross-platform RDP clients connect to the Mac and see its desktop, with keyboard/mouse forwarded back.

Not a client, not a VNC bridge, not a proxy — the server terminates the RDP protocol itself and renders/feeds the local macOS session.

@docs/architecture.md

@docs/macos-gotchas.md

@docs/known-quirks.md

## Commands

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
                          #   With the env var MACRDP_UDP_MIGRATE_EGFX=1 the EGFX
                          #   (H.264) channel is migrated onto the reliable UDP
                          #   tunnel via MS-RDPEDYC Soft-Sync (verified rendering on
                          #   mstsc); without it, EGFX stays on TCP (the proven safe
                          #   spike). Input/audio/clipboard always ride TCP. Not
                          #   supported under --fork-workers (falls back to TCP).
                          #   macOS-built but the protocol layer is cross-platform.
                          #   See docs/rdp-udp-multitransport-feasibility.md.
--fork-workers            # EXPERIMENTAL, opt-in (default OFF; FORK_WORKERS=1 in
                          #   config.env). xrdp's model on macOS: a thin supervisor
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
```

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

## Conventions worth keeping

- Keep `ironrdp` as the only crate that touches RDP wire format. Wrappers around it are fine; parallel parsing/emitting of PDUs is not.
- Per-platform code (capture, input, cursor, clipboard) is feature-gated via `#[cfg(target_os = "macos")]` so the protocol layer remains cross-compilable on Linux CI. Each module has a non-macOS stub for that reason.
- Errors that originate from macOS APIs (`OSStatus`, `CGError`, TCC denials, PAM error codes) should be wrapped with enough context that the user knows *which permission or service* is missing — those are the #1 support question.
- Direct FFI via `extern "C"` is preferred over heavyweight wrapper crates when the call surface is small (see `src/auth.rs::pam_impl`).
- Default log level is `info`; reach for `RUST_LOG=debug` when investigating, don't make debug the default.
- **Build new features modular and elegant.** A new capability should be a self-contained module (one concern per file), gated behind a flag (`--enable-…` / an `Option<…>` / a `#[cfg]`), and integrated into the core through a *narrow seam* — never by smearing its specifics across the hot path. Concretely:
  - **Reuse the existing pluggable seams instead of inventing parallel paths.** The server already exposes an optional-factory pattern (`sound_factory` / `cliprdr_factory` / `rdpdr_factory` / `gfx_factory`, all `Option<Box<dyn …>>` defaulting to `None`) and cloneable per-channel writers. New channel-level features should slot into those, the way smart-card redirection rode the existing RDPDR factory + completion-id router rather than building its own I/O path.
  - **Quarantine the messy stuff behind a clear boundary.** Private/undocumented APIs, a new C/FFI dep, or protocol wire-format details go behind one maintenance-boundary file/trait, not sprinkled through callers — e.g. `src/virtual_display/private_api.rs` (all private Obj-C touches) and the separate `ifd-handler/` cdylib + `src/rdpdr/smartcard.rs` bridge (smart-card kept to its own process + gated module). When a feature needs the core changed, prefer a trait hook that is a **zero-overhead / no-op passthrough when the feature is off**, so the default path is byte-for-byte unchanged.
  - **Exemplars to imitate:** smart-card redirection (separate cdylib package + gated `src/rdpdr/smartcard.rs`, dispatched by device type in `rdpdr/mod.rs`), `src/virtual_display/` (private-API boundary), and the modular-integration design in `docs/rdp-udp-multitransport-feasibility.md` (provider trait + transport router).
  - **Tempered by the two rules above it:** this governs *new* code. Do **not** retrofit working code into this shape for cosmetics (see "don't refactor working hot-paths"), and prefer landing reusable seams **upstream** rather than as a vendored divergence.
