// macrdp is a macOS app; the Linux build is only a compile-sanity stub (CI's
// cross-compile check), where the `#[cfg(target_os = "macos")]` paths are gone and
// a large amount of shared code/consts/helpers is naturally unused. Silence that
// class of noise on non-macOS only — the macOS `clippy -D warnings` gate stays
// fully strict (this never relaxes anything on the real build).
#![cfg_attr(
    not(target_os = "macos"),
    allow(dead_code, unused_imports, unused_variables, unused_mut)
)]

mod aac;
mod audio;
mod auth;
mod avc444;
mod capture;
mod clipboard;
#[cfg(test)]
mod conn_test;
mod cursor;
#[cfg(target_os = "macos")]
mod file_promise;
#[cfg(target_os = "macos")]
mod file_promise_lazy;
#[cfg(target_os = "macos")]
mod h264;
mod input;
mod keyboard_layout;
mod multitransport;
mod rdpdr;
#[cfg(target_os = "macos")]
mod runloop_thread;
mod switcher_hud;
mod videotoolbox;
mod virtual_display;

use std::fs;
use std::io::BufReader;
use std::net::SocketAddr;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::io::{FromRawFd, IntoRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use der::Decode;
use ironrdp_pdu::rdp::capability_sets::{
    BitmapCodecs, Codec, CodecProperty, NsCodec, RemoteFxContainer,
};
use ironrdp_server::{Credentials, RdpServer};
use rcgen::{generate_simple_self_signed, CertifiedKey};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, error, info, warn};
use x509_cert::Certificate;
use zeroize::Zeroizing;

use crate::capture::{primary_display_size, CaptureDisplay};
use crate::input::{ensure_accessibility_access, MacInputHandler};

const FALLBACK_WIDTH: u16 = 1280;
const FALLBACK_HEIGHT: u16 = 720;

/// Codecs advertised to the client. The actual encoder picks the one the
/// client also speaks; on conflict, the negotiation prefers (in order)
/// QOIZ > QOI > RemoteFx > NSCodec.
///
/// NSCodec is here for Microsoft Remote Desktop on macOS, which speaks only
/// NSCodec. The encoder is a Phase-1 stub right now — see CLAUDE.md.
fn bitmap_codecs() -> BitmapCodecs {
    BitmapCodecs(vec![
        // NSCodec — for Microsoft Remote Desktop on macOS.
        Codec {
            id: 0,
            property: CodecProperty::NsCodec(NsCodec {
                is_dynamic_fidelity_allowed: false,
                is_subsampling_allowed: false, // Phase 3 will flip this on.
                color_loss_level: 3,
            }),
        },
        // RemoteFx — what mstsc actually uses.
        Codec {
            id: 0,
            property: CodecProperty::RemoteFx(RemoteFxContainer::ServerContainer(1)),
        },
        Codec {
            id: 0,
            property: CodecProperty::ImageRemoteFx(RemoteFxContainer::ServerContainer(1)),
        },
        // QOI/QOIZ — for FreeRDP and any other client that picks them up.
        Codec {
            id: 0,
            property: CodecProperty::Qoi,
        },
        Codec {
            id: 0,
            property: CodecProperty::QoiZ,
        },
    ])
}

/// Bounds of the user's primary display in macOS's global point-coord
/// space — origin is `(0, 0)` by convention. Used to feed input.rs and
/// cursor.rs when we're capturing the primary panel; the virtual-display
/// path queries `VirtualDisplay::{origin_pts, size_pts}` instead.
#[cfg(target_os = "macos")]
fn primary_screen_geometry() -> ((f64, f64), (f64, f64)) {
    use core_graphics::display::CGDisplay;
    // Size in *logical points* (e.g. 1512×982), matching the virtual-display
    // path and the `CGDisplay::bounds()` scaling in input.rs. The cursor's
    // HiDPI scale factor is derived as framebuffer_pixels / these points, so
    // this MUST be points — NOT `pixels_wide/high` (backing pixels), which
    // would make the ratio 1.0 even at Retina and leave the cursor half-size.
    let b = CGDisplay::main().bounds();
    ((b.origin.x, b.origin.y), (b.size.width, b.size.height))
}

/// Backing (Retina) pixel resolution of the main display, for the HiDPI
/// capture path. `CGDisplayMode::pixel_width/height` is the true backing size
/// (e.g. 3024×1964 on a 14" MBP), distinct from the logical point size
/// (`mode.width/height`, ~1512×982) and from `CGDisplay::pixels_wide/high`
/// (the canonical mode, which equals points on many configs). Returns `None`
/// if the mode can't be read or the size doesn't fit u16.
#[cfg(target_os = "macos")]
fn primary_backing_size() -> Option<(u16, u16)> {
    use core_graphics::display::CGDisplay;
    let mode = CGDisplay::main().display_mode()?;
    let w = u16::try_from(mode.pixel_width()).ok()?;
    let h = u16::try_from(mode.pixel_height()).ok()?;
    Some((w, h))
}

#[cfg(not(target_os = "macos"))]
fn primary_screen_geometry() -> ((f64, f64), (f64, f64)) {
    ((0.0, 0.0), (0.0, 0.0))
}

#[cfg(target_os = "macos")]
fn ensure_screen_recording_access() {
    use core_graphics::access::ScreenCaptureAccess;
    let tcc = ScreenCaptureAccess;
    if tcc.preflight() {
        info!("Screen Recording permission already granted");
        return;
    }
    warn!(
        "Screen Recording permission NOT granted. macrdp will appear in \
         System Settings → Privacy & Security → Screen Recording. Enable it, \
         then RESTART macrdp (TCC grants only take effect on next launch)."
    );
    // request() registers the binary with TCC and opens the prompt; the
    // returned bool reflects current state, which is still false on first run.
    let _ = tcc.request();
}

#[derive(Parser, Debug)]
#[command(name = "macrdp", about = "Native RDP server for macOS")]
struct Args {
    /// Address to bind. Defaults to loopback only — pass `0.0.0.0:3390`
    /// explicitly to accept LAN connections. Port 3389 (the standard RDP
    /// port) is privileged; 3390 is the unprivileged dev default.
    #[arg(long, default_value = "127.0.0.1:3390")]
    bind: SocketAddr,

    /// Desktop width in pixels. Defaults to the primary display's native width
    /// (queried via ScreenCaptureKit). Overriding to a non-native size makes
    /// SCK scale internally; we then disable dirty-rect updates and ship a
    /// full frame every tick, so bandwidth is higher than at native size.
    #[arg(long)]
    width: Option<u16>,

    /// Desktop height in pixels. Defaults to the primary display's native height.
    /// Same trade-off as --width when overridden.
    #[arg(long)]
    height: Option<u16>,

    /// Capture the primary display at its backing (Retina) pixel resolution
    /// instead of logical points — e.g. 3024×1964 instead of 1512×982 — so
    /// clients render crisp native pixels instead of upscaling a point-density
    /// frame. Off by default: it's ~4× the pixels (heavier on legacy/slow
    /// links), and the win is biggest with --enable-h264 (compresses cleanly,
    /// the client downscales sharply). Ignored when --width/--height are set or
    /// with --virtual-display (already an explicit resolution). macOS-only.
    #[arg(long)]
    hidpi: bool,

    /// Frame rate cap. Defaults to 15 for the legacy bitmap path, or 60 with
    /// --enable-h264 (H.264 over mstsc holds a ~2-frame presentation buffer, so
    /// 60fps keeps that buffer's wall-clock latency low enough that typing feels
    /// immediate — at 30fps it lags ~2 keystrokes). Set explicitly to override.
    #[arg(long)]
    fps: Option<u32>,

    /// Cursor size multiplier (default 1.0). At 1.0 the pointer is forwarded at
    /// its native macOS size, matching the cursor on the Mac's own screen with
    /// an exact hotspot. Some RDP clients draw the pointer at 1:1 device pixels
    /// while upscaling the desktop image to their window, which can make the
    /// native-size pointer look small; bump this (e.g. 1.5 or 2.0) to enlarge
    /// it for comfort. The hotspot stays accurate at any value.
    #[arg(long, default_value_t = 1.0)]
    cursor_scale: f64,

    /// Mac account the client authenticates as. Defaults to $USER. The
    /// password is validated against the local account via PAM (checkpw
    /// service) at startup, so this must be a real Mac user.
    #[arg(long)]
    username: Option<String>,

    /// Password to use without an interactive prompt. Discouraged and kept only
    /// for compatibility / scripted tests: a command-line password is visible to
    /// any local user via `ps` and may be saved in shell history. Prefer
    /// `--keychain` (headless) or leaving this unset to enter it at the prompt.
    #[arg(long)]
    password: Option<String>,

    /// Skip the PAM check and use the supplied --password verbatim. Useful
    /// for non-macOS dev or scripted tests; never use on a shared network.
    #[arg(long)]
    skip_auth: bool,

    /// Read the password from the macOS Keychain instead of prompting.
    /// Expects a generic-password entry: service `macrdp`, account = the
    /// resolved username. Create it once with:
    ///   security add-generic-password -s macrdp -a $USER -w
    /// Lets launchd start macrdp without an interactive terminal.
    #[arg(long)]
    keychain: bool,

    /// Directory holding cert.pem / key.pem. Generated on first run and
    /// reused thereafter so clients see a stable fingerprint across restarts.
    /// Defaults to ~/Library/Application Support/macrdp.
    #[arg(long)]
    cert_dir: Option<PathBuf>,

    /// Show debug-level logs from macrdp and the underlying ironrdp / rustls
    /// crates. Without this, known-noisy lines (TLS SNI warnings, the
    /// "Encoding bitmap took N ms" timer, the cert-acceptance "Broken pipe"
    /// from ironrdp_server) are suppressed.
    #[arg(long, short = 'v')]
    verbose: bool,

    /// Don't prevent the Mac from going to sleep / auto-locking while macrdp
    /// is running. By default we spawn `caffeinate` so an idle Mac doesn't
    /// tear down the connection mid-session. Pass this to keep normal power
    /// management on.
    #[arg(long)]
    allow_sleep: bool,

    /// Attach a headless virtual display at --width × --height and serve
    /// THAT to the RDP client instead of mirroring the user's primary
    /// panel. Behaves like plugging in an external monitor — the local
    /// screen stays untouched and you can keep working on the Mac
    /// locally while the remote session uses its own desktop.
    ///
    /// Requires --width and --height (the virtual display has no
    /// "native" size to fall back to). Backed by undocumented
    /// CoreGraphics private API (CGVirtualDisplay*); may break on
    /// future macOS releases.
    #[arg(long)]
    virtual_display: bool,

    /// Promote the virtual display to be the system's primary display
    /// for the duration of the run — the one with the menu bar, where
    /// new app windows open. Use this when you want to drive the
    /// remote desktop as your "main" workspace (e.g. headless remote
    /// use). Restored on clean exit; the underlying CGConfigure call is
    /// session-scoped so the layout also reverts on user logout if
    /// macrdp exits uncleanly (signal, crash). Only valid with
    /// --virtual-display.
    #[arg(long)]
    make_primary: bool,

    /// While an RDP client is connected, disable every physical display
    /// (built-in panel + any external monitors) so the Mac is headless
    /// via the virtual display alone: backlights off, no menu bar, no
    /// windows can be placed there, cursor can't cross onto them. As
    /// soon as the last client disconnects, the displays come back and
    /// the local workspace is restored. Also restored on clean exit
    /// and — even if Drop doesn't run (signal, crash) — at the next
    /// user logout via the session-scoped CGConfigure. Only valid with
    /// --virtual-display.
    #[arg(long)]
    detach_primary: bool,

    /// Alternative to --detach-primary: while a client is connected,
    /// take **exclusive capture** of every physical display via
    /// CGDisplayCapture. The panels go solid black (backlight stays
    /// on) and the cursor can't be moved onto them. Drops the capture
    /// on last disconnect. Captures are process-scoped, so a signal /
    /// crash auto-releases — no logout dance. Use this when
    /// --detach-primary doesn't actually go headless on your machine
    /// (e.g. the disable transaction succeeds but the panel keeps
    /// showing the desktop). Mutually exclusive with --detach-primary.
    /// Only valid with --virtual-display.
    #[arg(long)]
    capture_primary: bool,

    /// Serve the display as H.264 video over the EGFX virtual channel
    /// (MS-RDPEGFX, AVC420) instead of legacy RemoteFx/QOI BitmapUpdates.
    /// Hardware-encoded via VideoToolbox. Falls back to the legacy path
    /// automatically for clients that don't negotiate EGFX. Experimental;
    /// macOS-only.
    #[arg(long)]
    enable_h264: bool,

    /// Target H.264 bitrate in megabits/sec (only with --enable-h264).
    /// Default 6. Raising it sharpens detail at the cost of bigger per-frame
    /// writes, which can fill the socket buffer and delay audio on a
    /// constrained link (e.g. Wi-Fi); try 8–12 if you have headroom.
    #[arg(long, default_value_t = 6)]
    bitrate: u32,

    /// H.264 periodic keyframe (IDR) interval in seconds (only with
    /// --enable-h264). Default 2. This is a safety net for transient decode
    /// glitches on small changes (e.g. mstsc's lingering garbled text while
    /// typing); large changes (window-to-front, scroll) can also force an
    /// immediate IDR if --keyframe-on-change is set (off by default). Lower self-heals
    /// faster but frequent IDRs cost bandwidth/quality at a fixed bitrate and
    /// can stutter. Fractional values are allowed. First frame is a keyframe.
    #[arg(long, default_value_t = 2.0)]
    keyframe_interval: f32,

    /// Force on-change H.264 keyframes (only with --enable-h264). OFF by
    /// default. When enabled, a keyframe (IDR) is forced when a lot of the
    /// screen changes at once — a window raised to front, a scroll, an app
    /// launch — and briefly after a mouse click, so large updates render
    /// immediately on clients (e.g. mstsc) that apply big P-frames cleanly only
    /// on a keyframe. Left off (the default), the periodic --keyframe-interval
    /// safety net plus the trailing flush-burst (--flush-frames) already drain
    /// mstsc's presentation buffer, so the extra forced IDRs mostly just spend
    /// bitrate/quality at a fixed bitrate for no typing benefit; enable it only
    /// if large updates visibly lag on your client/link.
    #[arg(long = "keyframe-on-change", action = clap::ArgAction::SetTrue)]
    keyframe_on_change: bool,

    /// Deprecated/no-op: on-change keyframes are now OFF by default, so this is
    /// already the default. Accepted so existing command lines that pass
    /// --no-keyframe-on-change keep working; use --keyframe-on-change to enable.
    #[arg(long = "no-keyframe-on-change", action = clap::ArgAction::SetTrue, hide = true)]
    #[allow(dead_code)]
    no_keyframe_on_change_compat: bool,

    /// Dirty-area threshold (percent of the frame) that triggers an on-change
    /// keyframe (only with --enable-h264 + on-change keyframes). Lower catches
    /// smaller updates (more IDRs); higher is more conservative. Default 20.
    #[arg(long, default_value_t = 20)]
    keyframe_change_pct: u64,

    /// Lowered dirty-area threshold (percent) used briefly after a mouse click,
    /// to catch moderate click-driven updates (dropdowns, dialogs). Default 5.
    #[arg(long, default_value_t = 5)]
    keyframe_click_pct: u64,

    /// How long (milliseconds) after a click the --keyframe-click-pct threshold
    /// applies. Default 400.
    #[arg(long, default_value_t = 400)]
    keyframe_click_window_ms: u64,

    /// Max H.264 frames in the encode/ship pipeline before the capture loop
    /// drops to the latest frame (only with --enable-h264). Bounds interactive
    /// latency under sustained load: lower drops-to-latest sooner (snappier
    /// typing/window-switching during bursts) at the cost of more frame-skips;
    /// higher buffers more (smoother video) but lets a backlog build up. This
    /// throttles at capture, before encoding — encoded frames are never dropped
    /// (that would break the H.264 reference chain). Default 2; raise to 3–4 if
    /// video looks too skippy under heavy motion.
    #[arg(long, default_value_t = 2)]
    h264_frames_in_flight: u32,

    /// Number of trailing "flush" frames re-sent after the last on-screen change
    /// (only with --enable-h264). ScreenCaptureKit stops delivering frames on a
    /// static screen, so the last change before a pause (e.g. the final
    /// keystroke) would otherwise sit in mstsc's ~2-frame AVC420 presentation
    /// buffer until the next change or periodic keyframe — the "typing follows
    /// the keyframe" lag. After each change we re-submit the last frame this many
    /// times as cheap skip-P-frames to drain that buffer so the change appears
    /// promptly. mstsc needs ≥2; default 4 gives margin. Raise if a slight
    /// trailing lag remains; set 0 to disable the flush burst entirely.
    #[arg(long, default_value_t = 4)]
    flush_frames: u32,

    /// Disable lazy Windows→Mac file paste. Lazy paste is ON by default:
    /// temp files are pre-sized but empty when the Windows copy lands,
    /// and bytes stream only when the user actually pastes in Finder
    /// (macOS shows its native "Preparing to paste" progress dialog).
    /// Handles single files AND folder trees. Uses fewer parallel chunk
    /// requests than the eager path so the RDP session stays responsive
    /// during the on-paste download. Pass this to fall back to the eager
    /// path (downloads every file the moment Windows announces the copy,
    /// then auto-fires Cmd-V into Finder).
    #[cfg(target_os = "macos")]
    #[arg(long = "no-lazy-paste", action = clap::ArgAction::SetTrue)]
    no_lazy_paste: bool,

    /// Deprecated/no-op: lazy paste is now the default. Accepted so
    /// existing command lines that pass --lazy-paste keep working; use
    /// --no-lazy-paste to disable.
    #[cfg(target_os = "macos")]
    #[arg(long = "lazy-paste", action = clap::ArgAction::SetTrue, hide = true)]
    #[allow(dead_code)]
    lazy_paste_compat: bool,

    /// Default-on. While the client has sent `SuppressOutput { None }`
    /// (i.e., mstsc is minimized), stop emitting RDPSND wave PDUs at the
    /// server so the client's audio renderer drains naturally. Without
    /// this, mstsc keeps queueing waves into `audiodg.exe`'s buffer
    /// during the minimize; on refocus that buffer plays out late and
    /// audio drifts by however many seconds were spent minimized. Pass
    /// `--no-mute-on-minimize` to keep audio flowing through a minimize
    /// (preserves "minimized YouTube keeps playing on the Mac speakers")
    /// at the cost of accepting that drift on refocus.
    #[arg(long = "no-mute-on-minimize", action = clap::ArgAction::SetTrue)]
    no_mute_on_minimize: bool,

    /// Compress forwarded audio as AAC-LC over RDPSND (MS-RDPEA
    /// WAVE_FORMAT_AAC_MS) instead of raw 16-bit PCM — ~11x less audio
    /// bandwidth (~128 kbps vs ~1.4 Mbps). Off by default: AAC adds ~40–50 ms
    /// of encoder priming latency, so PCM stays the zero-latency LAN default.
    /// Clients that don't advertise AAC decode fall back to PCM automatically.
    /// AudioToolbox-encoded; macOS-only.
    #[arg(long)]
    enable_aac: bool,

    /// Target AAC bitrate in bits/sec (only with --enable-aac). Default
    /// 128000. 96000 maximizes savings (audible artifacts on music); 192000
    /// is near-transparent. Advertised to the client and used to configure
    /// the encoder.
    #[arg(long, default_value_t = 128_000)]
    aac_bitrate: u32,

    /// Enable RDPDR drive redirection (MS-RDPEFS): the connecting client
    /// redirects its local drive(s) and the Mac mounts each as a real
    /// read-write NFS volume (an in-process NFSv3 server + the built-in
    /// mount_nfs; no root/kext/FUSE), browsable in Finder. Off by default. The
    /// client must opt in too (mstsc: Local Resources → Drives; FreeRDP:
    /// /drive:NAME,PATH). macOS-only.
    #[arg(long)]
    enable_drive_redirection: bool,

    /// Enable RDPDR smart-card redirection (MS-RDPESC): the connecting client
    /// redirects its smart-card reader and macOS apps can use the card through
    /// it. Requires macrdp's PC/SC IFD handler bundle to be installed
    /// (`ifd-macrdp.bundle` in /usr/local/libexec/SmartCardServices/drivers) and
    /// a USB device present to trigger its load. Off by default. The client must
    /// opt in too (mstsc: Local Resources → More → Smart cards; FreeRDP:
    /// /smartcard). macOS-only.
    #[arg(long)]
    enable_smartcard_redirection: bool,

    /// EXPERIMENTAL, opt-in (default OFF). Offer RDP UDP multitransport
    /// (MS-RDPEMT over reliable RDPEUDP) to clients that advertise it, and bind a
    /// UDP listener on the same address/port as TCP. With the env var
    /// MACRDP_UDP_MIGRATE_EGFX=1 the EGFX (H.264) channel is migrated onto the
    /// reliable UDP tunnel via MS-RDPEDYC Soft-Sync (verified rendering on mstsc);
    /// without it, EGFX stays on TCP (the proven safe spike). Input, audio (RDPSND),
    /// and clipboard always ride TCP. The win is on lossy/high-latency links (no
    /// head-of-line blocking), marginal on a clean LAN. Not supported under
    /// --fork-workers (falls back to TCP). macOS-only build; see
    /// docs/rdp-udp-multitransport-feasibility.md.
    #[arg(long)]
    enable_udp_multitransport: bool,

    /// Don't adopt the client's requested desktop resolution. By default —
    /// when mirroring the primary display without --width/--height/--hidpi —
    /// macrdp reads the resolution the client asked for while connecting
    /// (e.g. mstsc full-screen on a 1920×1080 monitor) and serves the session
    /// at exactly that size, so the client presents the video 1:1 instead of
    /// rescaling it (client-side rescaling on mstsc costs typing latency and,
    /// with --enable-h264, contributes to audio drift). Pass this to always
    /// serve the size resolved at startup (the Mac display's native size)
    /// and let the client scale.
    #[arg(long = "no-client-resolution", action = clap::ArgAction::SetTrue)]
    no_client_resolution: bool,

    /// On the auto-size path, stretch the Mac screen to fill the client frame
    /// instead of preserving the Mac's aspect ratio. By default, when the client
    /// requests a resolution with a different aspect ratio than the Mac display,
    /// macrdp letterboxes/pillarboxes (keeps the picture undistorted, adds black
    /// bars) and maps mouse input into the centered content area. Pass this to
    /// get the old fill-and-distort behavior (no bars). No effect with
    /// --width/--height (those already stretch) or at a matching aspect ratio.
    #[arg(long)]
    stretch: bool,

    /// On Cmd+Tab, un-minimize the target app's window (bring it back from the
    /// Dock) instead of just activating the app. Off by default, which matches
    /// native macOS — Cmd+Tab activates a minimized app but doesn't restore its
    /// window. macOS-only.
    #[arg(long = "unminimize-on-switch")]
    unminimize_on_switch: bool,

    /// Also accept Option+Tab (Alt+Tab from the client) as a trigger for the app
    /// switcher, in addition to Cmd+Tab. Off by default. Useful for clients/
    /// configs that forward Alt+Tab to the session but gate Win+Tab (e.g. mstsc's
    /// "Apply Windows key combinations" when windowed). Same MRU cycle, committing
    /// on Option release; Option+Shift+Tab cycles backward. When off, Option+Tab
    /// reaches remote apps as a normal key. macOS-only.
    #[arg(long = "alt-tab-switch")]
    alt_tab_switch: bool,

    /// Show a visual app-switcher overlay (icon row) on the remote during Cmd+Tab
    /// / Option+Tab, like macOS's native switcher. Off by default. macrdp spawns a
    /// small helper process that draws a real on-screen panel; ScreenCaptureKit
    /// captures it, so the client sees it. The switch behaves identically with or
    /// without this — it only adds the HUD. macOS-only.
    #[arg(long = "app-switcher-hud")]
    app_switcher_hud: bool,

    /// Remap a curated set of Windows editing shortcuts from Ctrl to Cmd so
    /// Windows muscle memory drives macOS: Ctrl+C/V/X/A/Z/S/F/N/T/W/O/P/R/G (and
    /// Shift variants, e.g. Ctrl+Shift+Z → Cmd+Shift+Z redo) fire as the Cmd
    /// equivalent. Off by default (then Ctrl reaches remote apps unchanged, so
    /// macOS shortcuts need Cmd, i.e. the client's Windows/Super key). Cmd+Q is
    /// deliberately NOT produced (Q is excluded); nav keys are untouched. Always
    /// suppressed when a terminal is frontmost so Ctrl+C stays SIGINT. macOS-only.
    #[arg(long = "map-ctrl-to-cmd")]
    map_ctrl_to_cmd: bool,

    /// Bundle ids (comma-separated) where --map-ctrl-to-cmd is suppressed, in
    /// addition to the built-in terminal list. Use this for apps with an embedded
    /// terminal that can't be auto-detected (the frontmost app is the IDE, not a
    /// TTY), e.g. `--no-remap-apps com.microsoft.VSCode`. In a listed app, Ctrl
    /// stays Ctrl (its integrated-terminal Ctrl+C = SIGINT works; editor copy is
    /// Cmd+C, the app's native macOS copy). No effect without --map-ctrl-to-cmd.
    #[arg(long = "no-remap-apps", value_delimiter = ',')]
    no_remap_apps: Vec<String>,

    /// Force QOI BitmapUpdates to be encoded with `Channels::Rgb` instead of
    /// `Channels::Rgba`. The default (off) emits RGBA per the underlying
    /// PixelFormat, matching upstream `ironrdp-server`. Upstream
    /// `ironrdp-session` (every published `ironrdp-viewer` to date) drops
    /// RGBA QOI frames with `WARN: Unsupported RGBA QOI data` — so pass this
    /// flag when you're serving an IronRDP-based client that hasn't picked
    /// up the RGBA-decode patch yet, or your viewer will render blank.
    /// mstsc / Microsoft Remote Desktop / Windows App / FreeRDP don't
    /// advertise QOI and are unaffected either way.
    #[arg(long)]
    qoi_force_rgb: bool,

    /// Keyboard layout to interpret the client's keystrokes as, for non-US
    /// clients. By default the layout is **auto-detected** from the client's
    /// announced KLID (US/unknown keep the plain positional-keycode path, so
    /// the US majority is unaffected); pass this only to force a specific
    /// layout, or `none`/`off` to disable translation entirely. macrdp
    /// translates each key against the layout via `UCKeyTranslate` and posts
    /// the resulting character — without changing the Mac's own input source.
    /// Cmd/Ctrl shortcuts stay on the keycode path. Accepts a macOS
    /// input-source id (`com.apple.keylayout.French`), a short name (`french`,
    /// `de`, `azerty`, `swissgerman`), or a Windows KLID (`0x040C`). The layout
    /// must be installed on the Mac. macOS-only.
    #[arg(long)]
    keyboard_layout: Option<String>,

    /// Read all settings from a `key=value` config file (the same `config.env`
    /// the menu-bar controller writes) instead of from individual flags. When
    /// present, every other flag is ignored and the effective settings come
    /// entirely from the file — this is what the LaunchAgent passes so launchd
    /// runs THIS signed binary directly (stable Background-Task-Management
    /// identity, no unsigned wrapper script). See packaging/config.env.example
    /// for the recognized keys; unknown keys are ignored.
    #[arg(long)]
    config: Option<PathBuf>,

    /// PROOF-OF-CONCEPT (mirror-primary only): run a thin supervisor that spawns
    /// a fresh worker PROCESS per RDP connection (fork+exec of this same binary),
    /// mirroring xrdp's per-connection-frontend model, instead of handling every
    /// connection inside one long-lived process. Built to test whether
    /// process-freshness fixes the mstsc H.264 reconnect blank. Incompatible with
    /// --virtual-display / --capture-primary / --detach-primary (those need a
    /// persistent display owner, out of PoC scope). The accepted socket fd is
    /// handed to each worker via the internal MACRDP_WORKER_FD env var. macOS-only.
    #[arg(long = "fork-workers")]
    fork_workers: bool,
}

/// Env var the `--fork-workers` supervisor sets on each spawned worker, carrying
/// the inherited (already-accepted) client socket fd. Its presence is what marks
/// a process as a worker (survives `--config` re-expansion, unlike a CLI flag).
const WORKER_FD_ENV: &str = "MACRDP_WORKER_FD";

/// Phase-0 spike env var: the supervisor owns one `CGVirtualDisplay` and passes
/// its `display_id` to each worker here. Its presence tells the worker to CAPTURE
/// that supervisor-owned display (by id) instead of creating its own — proving
/// cross-process vdisplay capture, the make-or-break unknown for productization.
const WORKER_VD_ID_ENV: &str = "MACRDP_VD_ID";

/// Max time the supervisor waits for the previous worker to fully exit before
/// spawning the next one (see `run_fork_supervisor`). Bounds the rare case
/// where an old connection lingers, so a reconnect can't hang indefinitely.
const WORKER_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// After the previous worker process dies, give ScreenCaptureKit's capture
/// daemon a beat to release the capture slot before the next worker opens its
/// own SCStream. Process death frees the client side synchronously, but the SCK
/// server side (replayd) can lag — and a *blank* worker (one whose stream never
/// delivered) appears to release more slowly, which is what produces an
/// occasional pair of adjacent blank reconnects. Default chosen conservatively;
/// override at runtime with `MACRDP_WORKER_SCK_SETTLE_MS` to tune the floor on a
/// given machine without a rebuild.
///
/// Default 0: live testing showed the settle never helped (the residual blank is
/// a client-side mstsc re-composite quirk, NOT a capture-slot overlap — the
/// blank connection's capture/encode/ship logs are identical to a rendering
/// one), and a *longer* settle made things WORSE. The serialization wait alone
/// (draining the previous worker) is what matters. Knob kept for experimentation.
const WORKER_SCK_SETTLE_DEFAULT: std::time::Duration = std::time::Duration::from_millis(0);

/// Env var to override `WORKER_SCK_SETTLE_DEFAULT` (milliseconds).
const WORKER_SCK_SETTLE_ENV: &str = "MACRDP_WORKER_SCK_SETTLE_MS";

/// The `--fork-workers` supervisor: bind the listen address, then for every
/// inbound connection `fork+exec` a fresh copy of this binary as a worker,
/// handing it the already-accepted socket fd. The supervisor itself does NO
/// capture/encode/TLS — it only accepts and hands off, so each connection is
/// served by a brand-new process (xrdp's frontend model). Mirror-primary only.
/// Headless-blanking mode the fork-workers supervisor engages on first connect
/// and holds (continuous) for its lifetime. Mirrors the single-process flags.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BlankMode {
    None,
    Detach,
    Capture,
    MakePrimary,
}

/// Install the chosen headless blanking off the supervisor's accept loop and
/// stash the guard so it lives for the supervisor's lifetime. The install can
/// block for seconds (CG / WindowServer), hence the spawned task. Guards are
/// process-scoped, so they auto-restore when the supervisor dies.
fn engage_headless_blank(
    blank: BlankMode,
    vd_id: u32,
    detach_slot: Arc<std::sync::Mutex<Option<virtual_display::DetachedPrimary>>>,
    capture_slot: Arc<std::sync::Mutex<Option<virtual_display::CapturedPrimary>>>,
    primary_slot: Arc<std::sync::Mutex<Option<virtual_display::PrimaryOverride>>>,
) {
    tokio::spawn(async move {
        match blank {
            BlankMode::Detach => match virtual_display::DetachedPrimary::install(vd_id) {
                Ok(g) => {
                    info!(
                        "supervisor: headless engaged via --detach-primary (physical \
                         display disabled; restored when macrdp exits)"
                    );
                    *detach_slot.lock().expect("detach slot poisoned") = Some(g);
                }
                Err(e) => warn!("supervisor: could not engage --detach-primary: {e:#}"),
            },
            BlankMode::Capture => match virtual_display::CapturedPrimary::install(vd_id) {
                Ok(g) => {
                    info!(
                        "supervisor: headless engaged via --capture-primary (physical \
                         display blanked; restored when macrdp exits)"
                    );
                    *capture_slot.lock().expect("capture slot poisoned") = Some(g);
                }
                Err(e) => warn!("supervisor: could not engage --capture-primary: {e:#}"),
            },
            BlankMode::MakePrimary => match virtual_display::PrimaryOverride::install(vd_id) {
                Ok(Some(g)) => {
                    info!("supervisor: virtual display promoted to primary");
                    *primary_slot.lock().expect("primary slot poisoned") = Some(g);
                }
                Ok(None) => {
                    info!("supervisor: virtual display already primary — no override needed")
                }
                Err(e) => warn!("supervisor: could not promote virtual display to primary: {e:#}"),
            },
            BlankMode::None => {}
        }
    });
}

async fn run_fork_supervisor(
    bind: SocketAddr,
    // The supervisor-owned virtual display. Bound for the whole supervisor
    // lifetime so the CGVirtualDisplay registration persists across worker
    // reconnects; its id is handed to each worker via MACRDP_VD_ID. `None`
    // = mirror-primary. The OS reaps the registration when the supervisor dies.
    vd: Option<virtual_display::VirtualDisplay>,
    // Headless blanking the supervisor owns (engaged on first connect, held until
    // exit). Process-scoped, so it auto-restores when the supervisor dies.
    blank: BlankMode,
    // Own the single app-switcher HUD helper here (it LISTENS on :40243). Workers
    // only push SHOW/ADVANCE/HIDE to it (the display id rides in each SHOW), so a
    // per-worker helper would churn + contend on the port across reconnects.
    app_switcher_hud: bool,
) -> Result<()> {
    let vd_id: Option<u32> = vd.as_ref().map(|v| v.display_id());
    // Spawn the one persistent HUD helper (parent = supervisor, so it self-exits
    // when the supervisor dies). Held for the supervisor's lifetime; workers push
    // to it over loopback. macOS-only (no-op helper-locate elsewhere).
    #[cfg(target_os = "macos")]
    let _hud_helper = if app_switcher_hud {
        spawn_hud_helper()
    } else {
        None
    };
    #[cfg(not(target_os = "macos"))]
    let _ = app_switcher_hud;
    let exe = std::env::current_exe().context("resolve current exe for worker spawn")?;
    // Original argv (minus argv[0]) so each worker gets the same config/flags;
    // the worker takes the worker branch because MACRDP_WORKER_FD is set, so
    // re-passing --fork-workers here does NOT recurse.
    let base_args: Vec<String> = std::env::args().skip(1).collect();

    // A plain forced-exit on signal is enough: the supervisor creates no
    // SCStream, and its headless blanking (capture/detach/primary) is
    // PROCESS-SCOPED — macOS auto-restores the physical display when the
    // supervisor process dies (incl. SIGKILL/panic), so process::exit(0) here
    // restores the user's layout without an explicit guard drop. Workers are
    // separate processes that finish or die with their own connection.
    tokio::spawn(async {
        shutdown_signal().await;
        info!(
            "fork-workers supervisor: shutdown signal received — exiting (display auto-restores)"
        );
        std::process::exit(0);
    });

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("bind {bind}"))?;
    info!(
        %bind,
        "fork-workers supervisor listening — spawning a fresh worker process per connection (PoC)"
    );

    // The handle to the most-recently-spawned worker. macrdp mirror-primary is
    // inherently single-session, so we serialize: before spawning the next
    // worker we wait for the previous one to fully exit (+ a short SCK settle),
    // guaranteeing the old capture stream is released before the new worker
    // opens its own. This kills the intermittent reconnect-blank that remained
    // after fixing the worker to process::exit — a fast reconnect could still
    // overlap the new worker's capture with the dying worker's not-yet-released
    // SCStream.
    let mut prev_worker: Option<std::process::Child> = None;

    // SCK capture-slot settle, env-overridable for hardware-in-the-loop tuning.
    let sck_settle = std::env::var(WORKER_SCK_SETTLE_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(std::time::Duration::from_millis)
        .unwrap_or(WORKER_SCK_SETTLE_DEFAULT);
    info!(
        settle_ms = sck_settle.as_millis() as u64,
        "fork-workers: inter-worker SCK settle"
    );

    // Headless blanking is engaged on the FIRST connection and HELD for the
    // supervisor's lifetime (continuous — never disengaged between reconnects).
    // Held in slots because the install can block for seconds (esp. --detach,
    // which awaits a WindowServer commit), so it runs off the accept loop. All
    // three mechanisms are process-scoped to the supervisor, so they auto-restore
    // when it dies (incl. SIGKILL/panic) — no explicit teardown on the signal
    // path. The slots just keep the guards alive so they aren't dropped (which
    // would restore mid-session).
    let detach_slot: Arc<std::sync::Mutex<Option<virtual_display::DetachedPrimary>>> =
        Arc::new(std::sync::Mutex::new(None));
    let capture_slot: Arc<std::sync::Mutex<Option<virtual_display::CapturedPrimary>>> =
        Arc::new(std::sync::Mutex::new(None));
    let primary_slot: Arc<std::sync::Mutex<Option<virtual_display::PrimaryOverride>>> =
        Arc::new(std::sync::Mutex::new(None));
    let mut headless_engaged = false;

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                error!(error = ?e, "supervisor accept failed");
                continue;
            }
        };

        // Engage headless blanking on the first connection (continuous).
        if !headless_engaged && blank != BlankMode::None {
            headless_engaged = true;
            if let Some(id) = vd_id {
                engage_headless_blank(
                    blank,
                    id,
                    detach_slot.clone(),
                    capture_slot.clone(),
                    primary_slot.clone(),
                );
            }
        }

        // Drain the previous worker before this connection captures.
        if let Some(prev) = prev_worker.take() {
            let prev_pid = prev.id();
            let waited = tokio::time::timeout(
                WORKER_WAIT_TIMEOUT,
                tokio::task::spawn_blocking(move || {
                    let mut prev = prev;
                    let _ = prev.wait();
                }),
            )
            .await;
            if waited.is_err() {
                // Timed out: the old connection is still alive. The detached
                // spawn_blocking keeps running and reaps it; proceed anyway so a
                // genuinely stuck old session can't wedge reconnects forever.
                warn!(
                    worker_pid = prev_pid,
                    "previous worker still alive after wait timeout; spawning next anyway"
                );
            } else {
                debug!(
                    worker_pid = prev_pid,
                    "previous worker exited; capture slot freed"
                );
            }
            // Let SCK's daemon release the capture slot before the next SCStream.
            tokio::time::sleep(sck_settle).await;
        }
        // Take ownership of the raw fd and stop tokio/std from closing it; the
        // child inherits it across fork, and we close the parent's copy after
        // spawn. `into_std` then `into_raw_fd` detaches it from Rust's RAII.
        let std_stream = match stream.into_std() {
            Ok(s) => s,
            Err(e) => {
                error!(error = ?e, "supervisor: into_std failed");
                continue;
            }
        };
        let fd: RawFd = std_stream.into_raw_fd();

        let mut cmd = std::process::Command::new(&exe);
        cmd.args(&base_args).env(WORKER_FD_ENV, fd.to_string());
        // Hand the supervisor-owned virtual display id to the worker so it
        // captures that display by id instead of creating its own (Phase-0 spike).
        if let Some(id) = vd_id {
            cmd.env(WORKER_VD_ID_ENV, id.to_string());
        }
        // The fd must survive exec(): clear its close-on-exec flag in the child
        // (between fork and exec) so the worker can adopt it by number.
        unsafe {
            cmd.pre_exec(move || {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        match cmd.spawn() {
            Ok(child) => {
                let pid = child.id();
                info!(
                    ?peer,
                    worker_pid = pid,
                    fd,
                    "spawned worker process for connection"
                );
                // Hold the child so the NEXT accept waits for it to exit before
                // spawning (the serialization above) — that wait is also what
                // reaps it, so it never becomes a lingering zombie.
                prev_worker = Some(child);
            }
            Err(e) => error!(error = ?e, "failed to spawn worker process"),
        }

        // Parent no longer needs its copy of the connection fd (the child owns
        // its own descriptor via fork). Leaving it open would leak fds and hold
        // the socket half-open after the worker closes its side.
        unsafe {
            libc::close(fd);
        }
    }
}

/// Prevent macOS from going to sleep, dimming/sleeping the display, idle-
/// locking, or spinning down disks while macrdp is running. `caffeinate
/// -w PID` exits automatically when the supplied PID exits, so there's
/// nothing to clean up on shutdown.
#[cfg(target_os = "macos")]
fn prevent_sleep() {
    let pid = std::process::id().to_string();
    let res = std::process::Command::new("caffeinate")
        .args(["-dimsu", "-w", &pid])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match res {
        Ok(child) => info!(caffeinate_pid = child.id(), "preventing sleep / auto-lock"),
        Err(e) => warn!("could not spawn caffeinate to prevent sleep: {e}"),
    }
}

/// Locate the `macrdphud` app-switcher overlay helper: `MACRDP_HUD_HELPER` env
/// override, else the bundled copy next to us (`../Resources/macrdphud` from
/// `Contents/MacOS/macrdp`), else the dev build (`gui/.build/release/macrdphud`
/// up the repo tree from `target/<profile>/macrdp`).
#[cfg(target_os = "macos")]
fn locate_hud_helper() -> Option<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("MACRDP_HUD_HELPER") {
        let p = std::path::PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    let exe = std::env::current_exe().ok()?;
    // Bundled: .../macrdp.app/Contents/MacOS/macrdp -> .../Contents/Resources/macrdphud
    if let Some(macos_dir) = exe.parent() {
        let bundled = macos_dir.join("../Resources/macrdphud");
        if bundled.is_file() {
            return Some(bundled);
        }
    }
    // Dev: .../<repo>/target/<profile>/macrdp -> .../<repo>/gui/.build/release/macrdphud
    let dev = exe
        .ancestors()
        .nth(3)
        .map(|root| root.join("gui/.build/release/macrdphud"));
    dev.filter(|p| p.is_file())
}

/// Spawn the app-switcher HUD helper. Passes the loopback port and our pid (so it
/// self-exits if we die). Returns the child handle to hold for our lifetime.
#[cfg(target_os = "macos")]
fn spawn_hud_helper() -> Option<std::process::Child> {
    let Some(path) = locate_hud_helper() else {
        warn!(
            "--app-switcher-hud set but the macrdphud helper was not found \
             (set MACRDP_HUD_HELPER, or build it: gui/make-hud-helper.sh); \
             the switcher works, just without the on-screen HUD"
        );
        return None;
    };
    let mut cmd = std::process::Command::new(&path);
    cmd.env("MACRDP_HUD_PARENT", std::process::id().to_string());
    if let Some(port) = std::env::var_os("MACRDP_HUD_PORT") {
        cmd.env("MACRDP_HUD_PORT", port);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    match cmd.spawn() {
        Ok(child) => {
            info!(hud_pid = child.id(), helper = %path.display(), "app-switcher HUD helper spawned");
            Some(child)
        }
        Err(e) => {
            warn!(
                "could not spawn app-switcher HUD helper {}: {e}",
                path.display()
            );
            None
        }
    }
}

/// Shell out to `security find-generic-password -s macrdp -a <user> -w`,
/// which prints the password on stdout. The Keychain entry has to be
/// created out-of-band; this never prompts the user interactively.
fn read_password_from_keychain(username: &str) -> Result<Zeroizing<String>> {
    let out = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "macrdp",
            "-a",
            username,
            "-w",
        ])
        .output()
        .context("invoke security(1)")?;
    if !out.status.success() {
        return Err(anyhow!(
            "keychain entry not found (run: security add-generic-password -s macrdp -a {username} -w)"
        ));
    }
    // Wrap as soon as we touch the bytes so the underlying allocation is
    // zeroed when this scope drops. `security` appends a single \n.
    let mut s = Zeroizing::new(
        String::from_utf8(out.stdout).map_err(|_| anyhow!("keychain returned non-UTF8 bytes"))?,
    );
    if s.ends_with('\n') {
        s.pop();
    }
    if s.is_empty() {
        return Err(anyhow!("keychain entry for macrdp is empty"));
    }
    Ok(s)
}

fn default_cert_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join("Library/Application Support/macrdp"))
}

fn load_pem_cert_and_key(
    cert_path: &Path,
    key_path: &Path,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    let cert_file =
        fs::File::open(cert_path).with_context(|| format!("open cert {}", cert_path.display()))?;
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut BufReader::new(cert_file))
        .collect::<std::io::Result<_>>()
        .with_context(|| format!("parse cert {}", cert_path.display()))?;
    if certs.is_empty() {
        return Err(anyhow!("no certificates in {}", cert_path.display()));
    }

    let key_meta =
        fs::metadata(key_path).with_context(|| format!("stat key {}", key_path.display()))?;
    let mode = key_meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(anyhow!(
            "private key {} is group/world-accessible (mode {:o}); refusing to use it. \
             Fix with: chmod 600 {}",
            key_path.display(),
            mode,
            key_path.display(),
        ));
    }

    let key_file =
        fs::File::open(key_path).with_context(|| format!("open key {}", key_path.display()))?;
    let key = rustls_pemfile::private_key(&mut BufReader::new(key_file))
        .with_context(|| format!("parse key {}", key_path.display()))?
        .ok_or_else(|| anyhow!("no private key in {}", key_path.display()))?;

    Ok((certs, key))
}

fn generate_and_persist(
    cert_dir: &Path,
    cert_path: &Path,
    key_path: &Path,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    fs::create_dir_all(cert_dir)
        .with_context(|| format!("create cert dir {}", cert_dir.display()))?;

    let CertifiedKey { cert, key_pair } =
        generate_simple_self_signed(vec!["localhost".to_string()])
            .context("generate self-signed cert")?;

    let cert_pem = cert.pem();
    let key_pem = key_pair.serialize_pem();

    fs::write(cert_path, &cert_pem)
        .with_context(|| format!("write cert {}", cert_path.display()))?;

    // Create key with 0600 from the start — never let it briefly exist 0644.
    {
        use std::io::Write as _;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(key_path)
            .with_context(|| format!("create key {}", key_path.display()))?;
        f.write_all(key_pem.as_bytes())
            .with_context(|| format!("write key {}", key_path.display()))?;
    }
    // If the file pre-existed with looser perms, OpenOptions::mode is a no-op
    // on truncate — re-assert 0600 explicitly.
    fs::set_permissions(key_path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod key {}", key_path.display()))?;

    let cert_der = CertificateDer::from(cert.der().to_vec());
    let key_der = PrivateKeyDer::try_from(key_pair.serialize_der())
        .map_err(|e| anyhow!("convert key DER: {e}"))?;
    Ok((vec![cert_der], key_der))
}

/// The TLS material derived from the persisted cert/key, shared across the TCP
/// connection and the UDP multitransport flows.
struct TlsMaterial {
    /// rustls acceptor for the main TCP RDP connection.
    acceptor: TlsAcceptor,
    /// Raw `subjectPublicKey` BIT STRING for CredSSP channel binding.
    spki_der: Vec<u8>,
    /// rustls config reused to secure the reliable (`UdpFecR`) UDP flow.
    config: Arc<ServerConfig>,
    /// Cert DER, for building the lossy (`UdpFecL`) flow's DTLS context (Phase 2).
    cert_der: Vec<u8>,
    /// Private-key DER, same purpose as `cert_der`.
    key_der: Vec<u8>,
}

fn make_tls_acceptor(cert_dir: &Path) -> Result<TlsMaterial> {
    let cert_path = cert_dir.join("cert.pem");
    let key_path = cert_dir.join("key.pem");

    let (certs, key) = if cert_path.exists() && key_path.exists() {
        info!(dir = %cert_dir.display(), "loading persisted TLS cert");
        load_pem_cert_and_key(&cert_path, &key_path)?
    } else {
        info!(dir = %cert_dir.display(), "generating new self-signed TLS cert");
        generate_and_persist(cert_dir, &cert_path, &key_path)?
    };

    // CredSSP's public-key channel-binding hashes the raw `subjectPublicKey`
    // BIT STRING contents from the X.509 cert — i.e. the DER-encoded
    // RSAPublicKey itself, NOT the full SubjectPublicKeyInfo wrapper. See
    // sspi's `raw_peer_public_key()`. Re-deriving from a keypair, or
    // passing the SPKI sequence, both produce different bytes and the
    // server/client hashes disagree.
    let cert_der_bytes = certs
        .first()
        .ok_or_else(|| anyhow!("empty cert chain"))?
        .as_ref();
    let parsed = Certificate::from_der(cert_der_bytes).context("parse cert DER for SPKI")?;
    let spki_der = parsed
        .tbs_certificate
        .subject_public_key_info
        .subject_public_key
        .raw_bytes()
        .to_vec();

    // Capture the cert + key DER before they're moved into the rustls config —
    // the lossy UDP flow's DTLS server (Phase 2) is built from the same bytes.
    let cert_der = cert_der_bytes.to_vec();
    let key_der = key.secret_der().to_vec();

    let config = Arc::new(
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .context("build rustls ServerConfig")?,
    );
    // The same cert/config also secures the auxiliary UDP multitransport (MS-RDPEMT
    // over TLS for the reliable flow; DTLS for the lossy flow) — the client trusts
    // it via the main connection's TOFU. Returned so the UDP listener can reuse it
    // without re-loading the cert.
    Ok(TlsMaterial {
        acceptor: TlsAcceptor::from(Arc::clone(&config)),
        spki_der,
        config,
        cert_der,
        key_der,
    })
}

/// Spawn the session-transition watcher used by `--detach-primary`
/// and `--capture-primary`. On the 0→≥1 client transition (after a
/// short debounce that absorbs mstsc's cert-trust flap) it calls
/// `install(vd_id)` and stows the returned guard in `slot`; on
/// ≥1→0 it takes the guard back out and drops it (which is what
/// runs the actual re-enable / release). Edge-triggered: changes
/// between two non-zero counts are no-ops.
fn spawn_primary_overlay_watcher<T: Send + 'static>(
    label: &'static str,
    vd_id: u32,
    tracker: capture::SessionTracker,
    slot: Arc<std::sync::Mutex<Option<T>>>,
    install: fn(u32) -> Result<T>,
) {
    tokio::spawn(async move {
        use std::sync::atomic::Ordering;
        use std::time::{Duration, Instant};
        // Minimum dwell time on the engaged state before honoring a
        // re-attach. The WindowServer commits configure transactions
        // asynchronously — a disable→enable cycle that races the
        // first commit's propagation through SkyLight can be rejected
        // with CGError 1001 by the second tx. Conservative for capture
        // too where the API is synchronous; unifies the loop shape.
        const ENGAGED_SETTLE: Duration = Duration::from_secs(3);
        // mstsc's cert-trust handshake does a 0→1→0→1 flap within a
        // few seconds. Waiting briefly before acting on a 0→≥1
        // notification lets that bounce collapse to a no-op so we
        // don't kick off a 10-second blocking install for a session
        // that's already gone.
        const CONNECT_DEBOUNCE: Duration = Duration::from_millis(750);
        let mut was_zero = true;
        let mut installed_at: Option<Instant> = None;
        loop {
            tracker.notify.notified().await;
            let count = tracker.count.load(Ordering::SeqCst);
            let now_zero = count == 0;
            info!(label, was_zero, now_zero, count, "overlay watcher woke");
            match (was_zero, now_zero) {
                (true, false) => {
                    tokio::time::sleep(CONNECT_DEBOUNCE).await;
                    if tracker.count.load(Ordering::SeqCst) == 0 {
                        info!(
                            label,
                            "connection flapped during debounce — skipping install"
                        );
                        was_zero = true;
                        continue;
                    }
                    match install(vd_id) {
                        Ok(ovr) => {
                            installed_at = Some(Instant::now());
                            info!(
                                label,
                                "RDP client connected — Mac is now headless via the \
                                 virtual display"
                            );
                            *slot.lock().expect("overlay mutex poisoned") = Some(ovr);
                        }
                        Err(e) => warn!(
                            label,
                            "could not engage headless overlay on client connect: {e:#}"
                        ),
                    }
                }
                (false, true) => {
                    if let Some(installed) = installed_at {
                        let elapsed = installed.elapsed();
                        if elapsed < ENGAGED_SETTLE {
                            let wait = ENGAGED_SETTLE - elapsed;
                            info!(
                                label,
                                ?wait,
                                "waiting for WindowServer to settle before disengaging"
                            );
                            tokio::time::sleep(wait).await;
                        }
                    }
                    if let Some(ovr) = slot.lock().expect("overlay mutex poisoned").take() {
                        // Drop runs the actual restore + emits its own
                        // success/warn logs; don't claim a result here.
                        drop(ovr);
                        installed_at = None;
                        info!(label, "last RDP client disconnected");
                    }
                }
                _ => {}
            }
            was_zero = now_zero;
        }
    });
}

/// Boost the calling pthread to `QOS_CLASS_USER_INITIATED` so the macOS
/// scheduler keeps macrdp's worker threads ahead of background work
/// (rustc, Spotlight indexer, Time Machine, etc.) under CPU contention.
/// Without this, running a heavy compile on the same host produces
/// audible audio glitches on the RDP session because tokio's
/// default-QoS workers lose CPU time to nice-0 background processes.
///
/// USER_INITIATED is the right level here, not USER_INTERACTIVE —
/// macrdp is an interactive server the user is actively engaged with,
/// but USER_INTERACTIVE is reserved for UI animation / hard real-time
/// (e.g. WindowServer) and would starve the OS itself under contention.
///
/// **Important:** this is applied to tokio worker threads ONLY, not to
/// the main thread. An earlier attempt boosted the main thread too;
/// `CGVirtualDisplay initWithDescriptor:` runs on that thread, and
/// boosting it broke SCK's display enumeration — SCK couldn't see the
/// freshly-registered virtual display for up to a minute on mstsc
/// connection attempts. Workers-only works because the actual async
/// work happens on workers; main thread just sits in
/// `Runtime::block_on`.
///
/// `pthread_set_qos_class_self_np` has been stable since macOS 10.10
/// (2014). Best-effort: errors are silently ignored — losing the boost
/// is degraded behavior, not a failure.
#[cfg(target_os = "macos")]
fn boost_thread_qos() {
    use std::os::raw::{c_int, c_uint};
    // From <sys/qos.h>:
    //   QOS_CLASS_USER_INTERACTIVE = 0x21
    //   QOS_CLASS_USER_INITIATED   = 0x19
    //   QOS_CLASS_DEFAULT          = 0x15
    //   QOS_CLASS_UTILITY          = 0x11
    //   QOS_CLASS_BACKGROUND       = 0x09
    const QOS_CLASS_USER_INITIATED: c_uint = 0x19;
    unsafe extern "C" {
        fn pthread_set_qos_class_self_np(qos_class: c_uint, relative_priority: c_int) -> c_int;
    }
    unsafe {
        let _ = pthread_set_qos_class_self_np(QOS_CLASS_USER_INITIATED, 0);
    }
}

fn main() -> Result<()> {
    // Build the tokio runtime explicitly so every worker thread starts
    // at boosted QoS (see `boost_thread_qos`). `#[tokio::main]`'s
    // generated runtime gives us no hook for this. Crucially: the main
    // thread itself is NOT boosted — see the warning in
    // `boost_thread_qos` about CGVirtualDisplay + SCK enumeration.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .on_thread_start(|| {
            #[cfg(target_os = "macos")]
            boost_thread_qos();
        })
        .build()?;
    rt.block_on(async_main())
}

/// Translate a `config.env` (plain `key=value`, the format the menu-bar
/// controller writes and `packaging/config.env.example` documents) into the
/// equivalent CLI argv and parse it back into `Args`. This lets the LaunchAgent
/// launch the *signed* `macrdp` binary directly with a single `--config <file>`
/// argument — giving macOS Background Task Management a stable Developer-ID
/// identity to approve once — instead of going through an unsigned wrapper
/// script that BTM re-flags on every rebuild. Mirrors the old
/// `packaging/macrdp-launch` translation exactly (same keys, same defaults).
fn args_from_config(path: &Path) -> Result<Args> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("reading config file {}", path.display()))?;

    let mut cfg: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Tolerate a leading `export ` (the file used to be shell-sourced).
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, val)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().to_string();
        let mut val = val.trim();
        // Strip one layer of matching surrounding quotes.
        for q in ['"', '\''] {
            if val.len() >= 2 && val.starts_with(q) && val.ends_with(q) {
                val = &val[1..val.len() - 1];
                break;
            }
        }
        cfg.insert(key, val.to_string());
    }

    let on = |key: &str, default: bool| cfg.get(key).map(|v| v == "1").unwrap_or(default);
    let get =
        |key: &str, default: &str| cfg.get(key).cloned().unwrap_or_else(|| default.to_string());

    let mut argv: Vec<String> = vec!["macrdp".to_string()];
    argv.push("--bind".into());
    argv.push(get("BIND", "127.0.0.1:3390"));

    // USERNAME defaults to $USER, matching the wrapper.
    let username = cfg
        .get("USERNAME")
        .cloned()
        .unwrap_or_else(|| std::env::var("USER").unwrap_or_default());
    if !username.is_empty() {
        argv.push("--username".into());
        argv.push(username);
    }

    if on("USE_KEYCHAIN", true) {
        argv.push("--keychain".into());
    }
    if on("ENABLE_H264", false) {
        argv.push("--enable-h264".into());
    }
    if on("ENABLE_AAC", false) {
        argv.push("--enable-aac".into());
    }
    if on("HIDPI", false) {
        argv.push("--hidpi".into());
    }
    if on("UNMINIMIZE", false) {
        argv.push("--unminimize-on-switch".into());
    }
    if on("ALT_TAB_SWITCH", false) {
        argv.push("--alt-tab-switch".into());
    }
    if on("APP_SWITCHER_HUD", false) {
        argv.push("--app-switcher-hud".into());
    }
    if on("MAP_CTRL_TO_CMD", false) {
        argv.push("--map-ctrl-to-cmd".into());
    }
    if let Some(list) = cfg.get("NO_REMAP_APPS") {
        if !list.is_empty() {
            argv.push("--no-remap-apps".into());
            argv.push(list.clone());
        }
    }
    if on("ENABLE_DRIVE_REDIRECTION", false) {
        argv.push("--enable-drive-redirection".into());
    }
    if on("ENABLE_SMARTCARD_REDIRECTION", false) {
        argv.push("--enable-smartcard-redirection".into());
    }
    if on("ENABLE_UDP_MULTITRANSPORT", false) {
        argv.push("--enable-udp-multitransport".into());
    }
    if on("FORK_WORKERS", false) {
        argv.push("--fork-workers".into());
    }
    if on("VIRTUAL_DISPLAY", false) {
        argv.push("--virtual-display".into());
        argv.push("--width".into());
        argv.push(get("VD_WIDTH", "1920"));
        argv.push("--height".into());
        argv.push(get("VD_HEIGHT", "1080"));
        // Back-compat: old configs set CAPTURE_PRIMARY=1 with no PRIMARY_MODE.
        let mut primary_mode = get("PRIMARY_MODE", "none");
        if primary_mode == "none" && on("CAPTURE_PRIMARY", false) {
            primary_mode = "capture".to_string();
        }
        match primary_mode.as_str() {
            "detach" => argv.push("--detach-primary".into()),
            "capture" => argv.push("--capture-primary".into()),
            _ => {}
        }
    }
    if let Some(layout) = cfg.get("KEYBOARD_LAYOUT") {
        if !layout.is_empty() {
            argv.push("--keyboard-layout".into());
            argv.push(layout.clone());
        }
    }
    // EXTRA_FLAGS: space-separated escape hatch, appended verbatim.
    if let Some(extra) = cfg.get("EXTRA_FLAGS") {
        for tok in extra.split_whitespace() {
            argv.push(tok.to_string());
        }
    }

    // Re-parse through clap so every value is validated exactly as a CLI arg
    // would be (SocketAddr, numeric ranges, mutually-exclusive checks).
    Args::try_parse_from(&argv)
        .with_context(|| format!("config file {} produced invalid settings", path.display()))
}

async fn async_main() -> Result<()> {
    let mut args = Args::parse();
    // If launched with `--config <file>` (the LaunchAgent path), the file is the
    // sole source of truth — rebuild Args from it.
    if let Some(cfg_path) = args.config.clone() {
        args = args_from_config(&cfg_path)?;
    }

    // RUST_LOG (if set) always wins. Otherwise: --verbose turns on debug
    // everywhere; without it we apply a targeted filter that quiets known
    // noisy modules:
    //   - rustls SNI warnings (clients legally put IPs there)
    //   - the ironrdp_server::encoder "took N ms" timer (fires above 10ms)
    //   - ironrdp_server::server WARN ("Unexpected share data pdu"); ERRORs
    //     are still shown — that's where the cert-acceptance "Broken pipe"
    //     comes from, which we can't suppress without losing real errors.
    let filter = if let Ok(env) = tracing_subscriber::EnvFilter::try_from_default_env() {
        env
    } else if args.verbose {
        tracing_subscriber::EnvFilter::new("debug")
    } else {
        tracing_subscriber::EnvFilter::new(
            "info,rustls=error,ironrdp_server::encoder=error,ironrdp_server::server=error",
        )
    };
    tracing_subscriber::fmt().with_env_filter(filter).init();

    // --fork-workers PoC. A process is a *worker* iff the supervisor set
    // MACRDP_WORKER_FD (the already-accepted client socket); that takes
    // precedence over --fork-workers so a re-passed flag can't recurse.
    let worker_fd: Option<RawFd> = std::env::var(WORKER_FD_ENV)
        .ok()
        .and_then(|s| s.parse::<RawFd>().ok());
    // Phase-0 spike: if set, this worker captures the SUPERVISOR-owned virtual
    // display by this id instead of creating its own (see WORKER_VD_ID_ENV).
    let supervisor_vd_id: Option<u32> = std::env::var(WORKER_VD_ID_ENV)
        .ok()
        .and_then(|s| s.parse::<u32>().ok());
    if args.fork_workers && worker_fd.is_none() {
        // Supervisor role.
        //
        // The SUPERVISOR owns the virtual display (created here, persisting across
        // worker reconnects) — each worker captures it by id via MACRDP_VD_ID
        // (validated by the Phase-0 spike: cross-process capture works, id stable).
        // The supervisor ALSO owns the headless blanking (--capture-primary /
        // --detach-primary / --make-primary) so it persists across reconnects;
        // workers never touch it. Blanking is process-scoped to the supervisor, so
        // it auto-restores when the supervisor dies (incl. SIGKILL/panic).
        if (args.capture_primary || args.detach_primary || args.make_primary)
            && !args.virtual_display
        {
            return Err(anyhow!(
                "--capture-primary/--detach-primary/--make-primary require \
                 --virtual-display (you'd be left with no usable display otherwise)"
            ));
        }
        if args.capture_primary && args.detach_primary {
            return Err(anyhow!(
                "--capture-primary and --detach-primary are mutually exclusive \
                 (pick one mechanism for going headless)"
            ));
        }
        if args.enable_smartcard_redirection {
            // Smart-card redirection is per-connection under --fork-workers: the
            // bridge can't be supervisor-owned (the card lives on the client and
            // the bridge talks to the live worker's RdpdrHandle), so each worker
            // binds :40242 for its own connection. Verified end-to-end (establish
            // / list / get-status+ATR / connect / transmit) including reconnect —
            // slotd cleanly re-attaches to the fresh worker — so this is just an
            // informational note, not a warning.
            info!(
                "smart-card redirection runs per-connection under --fork-workers \
                 (the :40242 bridge re-binds in each worker); verified incl. reconnect."
            );
        }
        let vd = if args.virtual_display {
            let w = args
                .width
                .ok_or_else(|| anyhow!("--virtual-display requires --width"))?;
            let h = args
                .height
                .ok_or_else(|| anyhow!("--virtual-display requires --height"))?;
            let vd = virtual_display::VirtualDisplay::new(u32::from(w), u32::from(h), 60)
                .context("supervisor: attaching virtual display")?;
            info!(
                display_id = vd.display_id(),
                origin = ?vd.origin_pts(),
                size = ?vd.size_pts(),
                "supervisor: virtual display attached — workers capture it by id"
            );
            Some(vd)
        } else {
            None
        };
        let blank = if args.detach_primary {
            BlankMode::Detach
        } else if args.capture_primary {
            BlankMode::Capture
        } else if args.make_primary {
            BlankMode::MakePrimary
        } else {
            BlankMode::None
        };
        // Prevent sleep at the SUPERVISOR level (one `caffeinate -w <supervisor
        // pid>` for the whole session) rather than per-worker — an always-on
        // headless server must stay awake across the brief gaps between worker
        // reconnects too, and the workers skip their own prevent_sleep (gated
        // below on worker_fd). caffeinate exits with the supervisor.
        #[cfg(target_os = "macos")]
        if !args.allow_sleep {
            prevent_sleep();
        }
        if args.enable_udp_multitransport {
            // The persistent UDP socket would have to live in the supervisor (one
            // socket per port, demuxed across worker reconnects); that's not wired
            // yet. The M1 offer still happens in each worker, so clients fall back
            // to TCP. Deferred — see the multitransport plan.
            warn!(
                "UDP multitransport listener is not started under --fork-workers yet; \
                 clients will fall back to TCP"
            );
        }
        return run_fork_supervisor(args.bind, vd, blank, args.app_switcher_hud).await;
    }
    if let Some(fd) = worker_fd {
        info!(
            fd,
            "worker process: serving one connection on the inherited socket"
        );
    }

    if args.make_primary && !args.virtual_display {
        return Err(anyhow!(
            "--make-primary requires --virtual-display (it promotes the \
             virtual display to be the system's primary)"
        ));
    }
    if args.detach_primary && !args.virtual_display {
        return Err(anyhow!(
            "--detach-primary requires --virtual-display (you'd be left \
             with no usable display otherwise)"
        ));
    }
    if args.capture_primary && !args.virtual_display {
        return Err(anyhow!(
            "--capture-primary requires --virtual-display (you'd be left \
             with no usable display otherwise)"
        ));
    }
    if args.capture_primary && args.detach_primary {
        return Err(anyhow!(
            "--capture-primary and --detach-primary are mutually exclusive \
             (pick one mechanism for going headless)"
        ));
    }

    // Shared slots so the signal handler and the session-transition
    // watcher can drop the display RAII guards before process::exit and
    // actually restore the user's setup. Populated later if the
    // corresponding flag is set.
    let primary_override: std::sync::Arc<
        std::sync::Mutex<Option<virtual_display::PrimaryOverride>>,
    > = std::sync::Arc::new(std::sync::Mutex::new(None));
    let detached_primary: std::sync::Arc<
        std::sync::Mutex<Option<virtual_display::DetachedPrimary>>,
    > = std::sync::Arc::new(std::sync::Mutex::new(None));
    let captured_primary: std::sync::Arc<
        std::sync::Mutex<Option<virtual_display::CapturedPrimary>>,
    > = std::sync::Arc::new(std::sync::Mutex::new(None));

    // Install signal handling before anything touches ScreenCaptureKit. Once an
    // SCK capture stream is live, macOS framework threads can leave the process
    // unkillable by Ctrl-C; a forced exit on SIGINT/SIGTERM sidesteps that — and
    // any other non-cooperative native threads — instead of relying on the
    // tokio runtime to unwind cleanly.
    //
    // The display RAII guards (PrimaryOverride / DetachedPrimary) are
    // dropped here first so the user's layout is restored before exit.
    // Without this, Ctrl-C would leave the virtual display promoted or
    // the built-in panel disabled until logout.
    let cleanup_primary = primary_override.clone();
    let cleanup_detach = detached_primary.clone();
    let cleanup_capture = captured_primary.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        info!("shutdown signal received — exiting");
        if let Some(ovr) = cleanup_detach.lock().expect("detach mutex poisoned").take() {
            drop(ovr); // re-enables the built-in display
        }
        if let Some(ovr) = cleanup_capture
            .lock()
            .expect("capture mutex poisoned")
            .take()
        {
            drop(ovr); // releases captured displays
        }
        if let Some(ovr) = cleanup_primary
            .lock()
            .expect("primary mutex poisoned")
            .take()
        {
            drop(ovr); // restores display arrangement
        }
        // Lazy paste leaves NSFilePresenters registered + a temp dir on
        // disk + URLs on NSPasteboard. Process::exit skips Drop on the
        // cliprdr backend, so flush that state explicitly. No-op if
        // MacCliprdr was never constructed.
        #[cfg(target_os = "macos")]
        file_promise_lazy::shutdown_cleanup();
        // RDPDR NFS volumes are unmounted on disconnect by Surface::Drop, but
        // process::exit (below) skips Drop — so unmount any still-mounted ones
        // here, or a signal stop strands them pointing at the dead server.
        rdpdr::shutdown_cleanup();
        std::process::exit(0);
    });

    #[cfg(target_os = "macos")]
    {
        // Fork-workers: the SUPERVISOR already holds a `caffeinate` for the whole
        // session, so workers skip their own (avoids per-reconnect caffeinate
        // churn). Single-process (worker_fd None, non-fork) keeps it.
        if !args.allow_sleep && worker_fd.is_none() {
            prevent_sleep();
        }
        ensure_screen_recording_access();
        if !ensure_accessibility_access() {
            warn!(
                "Accessibility permission NOT granted. macrdp will appear in \
                 System Settings → Privacy & Security → Accessibility. Enable \
                 it, then RESTART macrdp. Without it, keyboard/mouse input \
                 from RDP clients is silently dropped."
            );
        } else {
            info!("Accessibility permission already granted");
        }
    }
    #[cfg(not(target_os = "macos"))]
    tracing::warn!("Built for a non-macOS target — capture is a static-rectangle stub.");

    // Allocate the virtual display BEFORE anything else queries SCK, so
    // it's visible in SCShareableContent's enumeration when capture
    // resolves the displayID. Held in main's scope so its Drop runs on
    // normal exit (signal-driven exit goes through std::process::exit
    // and skips Drop, but macOS reaps virtual displays when the owning
    // process dies, so cleanup still happens — just not via Drop).
    // When `supervisor_vd_id` is set we're a fork-worker capturing the
    // supervisor-owned virtual display by id — do NOT create our own (it would
    // be a second, redundant display that dies each reconnect; the whole point
    // of the spike is to capture the persistent supervisor one). Geometry comes
    // from the supervisor display's CGDisplayBounds in the size block below.
    let virtual_display: Option<virtual_display::VirtualDisplay> =
        if args.virtual_display && supervisor_vd_id.is_none() {
            let w = args
                .width
                .ok_or_else(|| anyhow!("--virtual-display requires --width"))?;
            let h = args
                .height
                .ok_or_else(|| anyhow!("--virtual-display requires --height"))?;
            // 60 Hz: real displays bottom out around 24 Hz. Refresh rate is
            // metadata here (capture cadence is governed by --fps); pass a
            // safe value so CGVirtualDisplay doesn't reject the mode.
            let vd = virtual_display::VirtualDisplay::new(u32::from(w), u32::from(h), 60)
                .context("attaching virtual display")?;
            info!(
                display_id = vd.display_id(),
                origin = ?vd.origin_pts(),
                size = ?vd.size_pts(),
                "virtual display attached — the RDP session uses this surface; \
                 your primary panel is untouched"
            );
            Some(vd)
        } else {
            None
        };

    // --detach-primary / --capture-primary are lazy: the headless
    // mechanism is only engaged once a client actually connects. A
    // session-transition watcher installs the corresponding RAII
    // guard on 0→≥1 client transitions and drops it on ≥1→0. Both
    // flags subsume --make-primary (each puts the virtual display
    // at (0,0)), so if both are set, only the lazy path runs.
    let session_tracker = capture::SessionTracker::default();
    if worker_fd.is_some() {
        // Fork-worker: the SUPERVISOR owns headless blanking (it persists across
        // reconnects and is engaged there). A worker has no virtual_display of
        // its own — it captures the supervisor's by id — so the `.expect()`s
        // below would panic. Skip entirely; blanking is not the worker's job.
    } else if args.detach_primary {
        let vd_id = virtual_display
            .as_ref()
            .expect("checked above when --detach-primary")
            .display_id();
        spawn_primary_overlay_watcher(
            "detach",
            vd_id,
            session_tracker.clone(),
            detached_primary.clone(),
            virtual_display::DetachedPrimary::install,
        );
    } else if args.capture_primary {
        let vd_id = virtual_display
            .as_ref()
            .expect("checked above when --capture-primary")
            .display_id();
        spawn_primary_overlay_watcher(
            "capture",
            vd_id,
            session_tracker.clone(),
            captured_primary.clone(),
            virtual_display::CapturedPrimary::install,
        );
    } else if args.make_primary {
        let vd_id = virtual_display
            .as_ref()
            .expect("checked above when --make-primary")
            .display_id();
        match virtual_display::PrimaryOverride::install(vd_id)
            .context("promoting virtual display to primary")?
        {
            Some(ovr) => {
                info!(
                    "virtual display promoted to primary — menu bar and new \
                     windows move there. Original layout restored on exit \
                     (or at next logout)."
                );
                *primary_override.lock().expect("override mutex poisoned") = Some(ovr);
            }
            None => {
                info!(
                    "virtual display is already the system primary (macOS \
                     auto-placed it at origin) — no override needed"
                );
            }
        }
    }

    let username = args
        .username
        .clone()
        .or_else(|| std::env::var("USER").ok())
        .ok_or_else(|| anyhow!("no username: pass --username or set $USER"))?;
    let password: Zeroizing<String> = if args.keychain {
        read_password_from_keychain(&username)?
    } else if let Some(p) = args.password.take() {
        warn!(
            "--password is visible to any local user via `ps` and may be saved in \
             shell history; prefer --keychain (headless) or the interactive prompt. \
             Kept only for compatibility / scripted tests."
        );
        Zeroizing::new(p)
    } else {
        Zeroizing::new(
            rpassword::prompt_password(format!("Password for {username}: "))
                .context("read password from terminal")?,
        )
    };
    if !args.skip_auth {
        auth::authenticate(&username, password.as_str())
            .with_context(|| format!("PAM auth failed for {username}"))?;
        info!(user = %username, "PAM auth ok");
    } else {
        // --skip-auth is a dev-only escape hatch. Refuse it whenever the
        // listener is reachable beyond loopback so a fat-fingered config
        // can't accidentally expose an unauth'd RDP server to the LAN.
        if !args.bind.ip().is_loopback() {
            return Err(anyhow!(
                "--skip-auth refused on non-loopback bind {} — \
                 remove --skip-auth or rebind to 127.0.0.1",
                args.bind,
            ));
        }
        warn!("--skip-auth set; using --password verbatim without PAM check (loopback only)");
    }

    let cert_dir = match args.cert_dir.clone() {
        Some(p) => p,
        None => default_cert_dir()?,
    };
    let TlsMaterial {
        acceptor: tls,
        spki_der,
        config: udp_tls_config,
        cert_der: udp_cert_der,
        key_der: udp_key_der,
    } = make_tls_acceptor(&cert_dir)?;

    // Resolve desktop dimensions + geometry. Three paths:
    //   - virtual display: width/height are already enforced as required
    //     above; geometry comes from the vdisplay's CGDisplayBounds.
    //   - primary panel, no --width/--height override: query SCK for
    //     native size and use CGDisplay::main() for the point-space bounds.
    //   - primary panel with override: use the override + main geometry.
    let (width, height, capture_display_id, screen_size_pts) = if let Some(vd_id) = supervisor_vd_id
    {
        // Phase-0 spike: capture the SUPERVISOR-owned virtual display by id.
        // Geometry: requested w/h for the session; size_pts from the display's
        // CGDisplayBounds (its origin is picked up by input.rs the same way).
        let w = args
            .width
            .ok_or_else(|| anyhow!("--virtual-display requires --width"))?;
        let h = args
            .height
            .ok_or_else(|| anyhow!("--virtual-display requires --height"))?;
        #[cfg(target_os = "macos")]
        let size = {
            let b = core_graphics::display::CGDisplay::new(vd_id).bounds();
            (b.size.width, b.size.height)
        };
        #[cfg(not(target_os = "macos"))]
        let size = (f64::from(w), f64::from(h));
        info!(
            width = w,
            height = h,
            display_id = vd_id,
            "worker: capturing supervisor-owned virtual display (Phase-0 spike)"
        );
        (w, h, Some(vd_id), size)
    } else if let Some(vd) = &virtual_display {
        // Both required earlier, so the unwraps can't fire.
        let w = args
            .width
            .expect("checked above when --virtual-display set");
        let h = args
            .height
            .expect("checked above when --virtual-display set");
        // Re-query CGDisplayBounds rather than trusting the cached
        // values from VirtualDisplay creation: if --make-primary
        // moved the display to (0, 0), the cached size is fine but
        // we want a fresh read for parity with the input handler.
        #[cfg(target_os = "macos")]
        let size = {
            let b = core_graphics::display::CGDisplay::new(vd.display_id()).bounds();
            (b.size.width, b.size.height)
        };
        #[cfg(not(target_os = "macos"))]
        let size = vd.size_pts();
        info!(
            width = w,
            height = h,
            display_id = vd.display_id(),
            "desktop size (virtual display)"
        );
        (w, h, Some(vd.display_id()), size)
    } else {
        let detected = primary_display_size().await?;
        let mut w = args
            .width
            .or(detected.map(|(w, _)| w))
            .unwrap_or(FALLBACK_WIDTH);
        let mut h = args
            .height
            .or(detected.map(|(_, h)| h))
            .unwrap_or(FALLBACK_HEIGHT);
        // --hidpi: capture at the display's backing (Retina) pixel resolution
        // instead of logical points, unless the user pinned an explicit
        // --width/--height (in which case they've chosen the size themselves).
        #[cfg(target_os = "macos")]
        if args.hidpi && args.width.is_none() && args.height.is_none() {
            if let Some((bw, bh)) = primary_backing_size() {
                info!(
                    points_w = w,
                    points_h = h,
                    backing_w = bw,
                    backing_h = bh,
                    "--hidpi: capturing at backing pixel resolution"
                );
                w = bw;
                h = bh;
            } else {
                warn!("--hidpi: could not read backing pixel size; staying at logical points");
            }
        }
        if let Some((dw, dh)) = detected {
            info!(
                width = w,
                height = h,
                detected_w = dw,
                detected_h = dh,
                "desktop size"
            );
        } else {
            info!(width = w, height = h, "desktop size (no display detected)");
        }
        let (_origin, size) = primary_screen_geometry();
        (w, h, None, size)
    };

    // Frame rate: explicit --fps wins; otherwise 60 for H.264 (mstsc holds a
    // ~2-frame presentation buffer, so 60fps keeps typing latency low) or 15 for
    // the legacy bitmap path (which has no such buffer and is bandwidth-heavier).
    let fps = args.fps.unwrap_or(if args.enable_h264 { 60 } else { 15 });

    // Live session desktop size, shared by capture, input scaling, and the
    // H.264 pipeline. Starts at the size resolved above; when `auto_size`
    // is on, `CaptureDisplay::request_initial_size` overwrites it with the
    // client's requested resolution at connect time.
    let desktop_size = capture::SharedDesktopSize::new(width, height);

    // Adopt the client's requested resolution only on the mirror-primary
    // path with no explicit size choice: --width/--height pin a size,
    // --hidpi pins backing pixels, and --virtual-display owns its own
    // fixed-size display.
    let auto_size = !args.no_client_resolution
        && !args.virtual_display
        && !args.hidpi
        && args.width.is_none()
        && args.height.is_none();
    if auto_size {
        info!(
            "client-resolution auto-adopt enabled — the session will be served at \
             the resolution the client requests (--no-client-resolution disables)"
        );
    }

    // Video-path heads-up. With --enable-h264 the active vs AVC420-fallback cases
    // are logged later (h264.rs, once the client's EGFX caps are known); the only
    // gap is the h264-disabled case, which never reaches that code — so name the
    // legacy path and point at the crisp option here. Answers the common "the
    // picture is grainy / not sure what codec it negotiated" question.
    if !args.enable_h264 {
        info!(
            "video codec: legacy bitmap path (RemoteFX/QOI/NSCodec/raw, client-negotiated). \
             For crisp H.264 on capable clients (mstsc, Microsoft Remote Desktop, FreeRDP, \
             Thincast) pass --enable-h264 (AVC420 over EGFX)."
        );
    }

    ironrdp_server::set_qoi_force_rgb(args.qoi_force_rgb);
    crate::input::set_unminimize_on_switch(args.unminimize_on_switch);
    crate::input::set_alt_tab_switch(args.alt_tab_switch);
    crate::input::set_app_switcher_hud(args.app_switcher_hud);
    crate::input::set_map_ctrl_to_cmd(args.map_ctrl_to_cmd);
    crate::input::set_no_remap_apps(args.no_remap_apps.clone());
    if args.map_ctrl_to_cmd {
        info!(
            no_remap_apps = ?args.no_remap_apps,
            "Ctrl→Cmd Windows-shortcut remap enabled (--map-ctrl-to-cmd)"
        );
        // Event-driven frontmost-app tracking for the terminal/app suppression
        // (catches click-focus + Electron apps that AX polling misses).
        crate::input::init_focus_observer();
    }

    // App-switcher HUD overlay helper (macOS-only). Tell it which display to
    // center on (the captured one) and spawn it; it idles until a Cmd+Tab. Held
    // for the process lifetime; the helper also self-exits if we die.
    #[cfg(target_os = "macos")]
    let _hud_helper = if args.app_switcher_hud {
        let did =
            capture_display_id.unwrap_or_else(|| core_graphics::display::CGDisplay::main().id);
        crate::switcher_hud::set_display_id(did);
        // Fork-workers: the SUPERVISOR owns the single persistent HUD helper
        // (binds :40243); a worker only PUSHES to it (the display id rides in
        // each SHOW), so it must NOT spawn its own — that would churn the helper
        // process and contend on the port across reconnects. The single-process
        // (non-fork) path still spawns + owns its own helper here.
        if worker_fd.is_none() {
            spawn_hud_helper()
        } else {
            None
        }
    } else {
        None
    };

    // Shared flag flipped true when EGFX is Soft-Synced onto a LOSSY UDP tunnel
    // (UDPFECL). The H.264 pipeline reads it to arm ack-driven IDR recovery; the
    // server flips it at the migration site. Cross-platform so the UDP listener
    // wiring below (which hands the same clone to the server) compiles on CI.
    let egfx_on_lossy_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    // EGFX/H.264 video pipeline (macOS-only; opt-in via --enable-h264). One
    // clone drives the builder's GfxServerFactory (protocol side); another
    // rides on CaptureDisplay, where the capture loop feeds it BGRA frames.
    #[cfg(target_os = "macos")]
    let gfx = args.enable_h264.then(|| {
        h264::Gfx::new(
            desktop_size.clone(),
            fps,
            args.bitrate.max(1).saturating_mul(1_000_000),
            args.keyframe_interval,
            args.h264_frames_in_flight.max(1),
            egfx_on_lossy_flag.clone(),
        )
    });

    // On-change keyframes are off by default; --keyframe-on-change opts in.
    let keyframe_on_change = crate::capture::KeyframeOnChange {
        enabled: args.keyframe_on_change,
        change_pct: args.keyframe_change_pct,
        click_pct: args.keyframe_click_pct,
        click_window: std::time::Duration::from_millis(args.keyframe_click_window_ms),
    };

    // Mouse-click hint shared by the input handler (which records clicks) and
    // the H.264 capture path (which lowers its keyframe threshold briefly after
    // a click). Only allocated when the on-demand-keyframe feature is enabled.
    let click_signal =
        (args.enable_h264 && keyframe_on_change.enabled).then(crate::capture::ClickSignal::new);

    // Shared "client minimized / SuppressOutput" flag — created here so
    // the same `Arc<AtomicBool>` can be handed to both the capture
    // backend (which reads it to gate frame emission) and the vendor
    // server (whose per-connection PDU handler writes it). See
    // `vendor/ironrdp-server` `display_suppressed` plumbing.
    let display_suppressed = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let display = CaptureDisplay {
        desktop_size: desktop_size.clone(),
        auto_size,
        stretch: args.stretch,
        fps,
        display_id: capture_display_id,
        screen_size_pts,
        // Virtual-display sessions drive the cursor into the virtual display's
        // off-panel coordinate region; warp it back to the primary on
        // disconnect so the local Mac mouse isn't stranded.
        warp_cursor_home: virtual_display.is_some(),
        cursor_scale: args.cursor_scale,
        // Only attach the tracker when the watchdog needs it. Saves
        // a useless atomic increment/decrement per session otherwise.
        session_tracker: (args.detach_primary || args.capture_primary)
            .then(|| session_tracker.clone()),
        #[cfg(target_os = "macos")]
        gfx: gfx.clone(),
        keyframe_on_change,
        click_signal: click_signal.clone(),
        flush_frames: args.flush_frames,
        display_suppressed: Some(display_suppressed.clone()),
    };

    // Shared cell the server fills with the connecting client's keyboard-layout
    // id, for auto-detecting a non-US layout when --keyboard-layout isn't given.
    let keyboard_layout_klid: crate::input::SharedKeyboardLayout =
        std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let input_handler = MacInputHandler::new(
        desktop_size.clone(),
        capture_display_id,
        click_signal,
        args.keyboard_layout.clone(),
        Some(keyboard_layout_klid.clone()),
    )?;
    let cliprdr: Box<dyn ironrdp_server::CliprdrServerFactory> = {
        #[cfg(target_os = "macos")]
        {
            Box::new(clipboard::MacCliprdr::new(!args.no_lazy_paste))
        }
        #[cfg(not(target_os = "macos"))]
        {
            Box::new(clipboard::MacCliprdr::new())
        }
    };
    let sound: Box<dyn ironrdp_server::SoundServerFactory> = Box::new(audio::MacRdpsnd::new(
        Some(display_suppressed.clone()),
        !args.no_mute_on_minimize,
        args.enable_aac,
        args.aac_bitrate,
        // Bind audio to the same display the video captures so --detach-primary
        // / --capture-primary (which disable/capture the physical panel) don't
        // kill the audio stream's content source.
        capture_display_id,
    ));

    // with_hybrid advertises HYBRID | HYBRID_EX so clients run CredSSP/NLA
    // over TLS. ironrdp_acceptor's accept_credssp reads our set_credentials
    // and validates the client's NTLM response against it.
    #[cfg(target_os = "macos")]
    let gfx_factory: Option<Box<dyn ironrdp_server::GfxServerFactory>> =
        gfx.map(|g| Box::new(g) as Box<dyn ironrdp_server::GfxServerFactory>);
    #[cfg(not(target_os = "macos"))]
    let gfx_factory: Option<Box<dyn ironrdp_server::GfxServerFactory>> = None;

    // RDPDR is opt-in. Drive redirection (--enable-drive-redirection) lets the
    // Mac browse/read the client's redirected drive; smart-card redirection
    // (--enable-smartcard-redirection) lets macOS apps use the client's reader.
    // Both ride the one RDPDR channel, so attach the factory if either is on.
    let rdpdr_factory: Option<Box<dyn ironrdp_server::RdpdrServerFactory>> =
        if args.enable_drive_redirection || args.enable_smartcard_redirection {
            Some(Box::new(rdpdr::MacRdpdr::new(
                args.enable_drive_redirection,
                args.enable_smartcard_redirection,
            )))
        } else {
            None
        };

    let mut server = RdpServer::builder()
        .with_addr(args.bind)
        .with_hybrid(tls, spki_der)
        .with_input_handler(input_handler)
        .with_display_handler(display)
        .with_cliprdr_factory(Some(cliprdr))
        .with_sound_factory(Some(sound))
        .with_rdpdr_factory(rdpdr_factory)
        .with_bitmap_codecs(bitmap_codecs())
        .with_gfx_factory(gfx_factory)
        .build();

    // Hand the shared suppress flag to the server so its per-connection
    // PDU handler writes to the same `AtomicBool` the capture backend
    // reads from. Without this, the server uses an internally-created
    // flag the display never sees.
    server.set_display_suppressed_handle(display_suppressed);

    // The acceptor records the client's announced keyboard-layout id (KLID) in
    // its Client Core Data; the server publishes it here so the input handler
    // can auto-select a matching non-US layout (when --keyboard-layout is unset).
    server.set_keyboard_layout_handle(keyboard_layout_klid);

    // Client-resolution auto-adopt: the vendored acceptor reads the desktop
    // size the client requests in its GCC Client Core Data and negotiates
    // the session at that size from the start (Demand Active); the
    // CaptureDisplay's `request_initial_size` then adopts it into the
    // shared `desktop_size` that capture, input scaling, and the H.264
    // pipeline all read.
    server.set_honor_client_desktop_size(auto_size);

    // EXPERIMENTAL UDP multitransport (MS-RDPEMT). When enabled, install the
    // provider so the server offers reliable UDP to clients that advertise it,
    // and (M3) bind a real UDP listener on the same address/port as TCP — the
    // client reuses the server address for the auxiliary UDP flow. The handle is
    // held in `_udp_listener` for the process lifetime (Drop aborts its task).
    //
    // M3 scope: the listener answers the RDPEUDP SYN→SYN+ACK handshake (V3/EUDP2
    // negotiation); cookie validation is soft and there's no TLS/EMT tunnel or
    // channel migration yet, so a client that completes the handshake still runs
    // the session over TCP. Single-process path only — under --fork-workers the
    // persistent UDP socket would have to live in the supervisor (deferred; we
    // warn above and fall back to TCP). worker_fd.is_some() ⇒ a fork worker.
    let _udp_listener = if args.enable_udp_multitransport && worker_fd.is_none() {
        // The server ISN isn't client-validated; seed it from the clock to avoid a
        // new RNG dependency (the security-relevant value is the cookie, not this).
        let isn_seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let cfg = ironrdp_server::ListenerConfig {
            server_isn_seed: isn_seed,
            ..Default::default()
        };
        // M5a: a shared cookie registry binds an inbound UDP tunnel to a real TCP
        // session. The server registers each issued cookie here; the listener
        // accepts a tunnel CREATEREQUEST only if its echoed cookie matches.
        let cookie_registry = ironrdp_server::CookieRegistry::new();
        // M5c: the server→listener handoff. The server pushes EGFX frames (when
        // migrated onto the tunnel) through the sender; the listener owns the rx
        // and ships them as RDP_TUNNEL_DATA over the bound peer's UDP tunnel.
        let (tunnel_sender, tunnel_rx) = ironrdp_server::tunnel_channel();
        // Phase 2 (P2.1a): a DTLS 1.2 server context for the LOSSY (UdpFecL) flow,
        // built from the same cert as the TCP/reliable path. Non-fatal if it fails
        // to build (the lossy flow then falls back to observe-only); the reliable
        // flow is unaffected.
        let udp_dtls_config = match ironrdp_server::DtlsServerContext::from_der(
            &udp_cert_der,
            &udp_key_der,
        ) {
            Ok(ctx) => Some(ctx),
            Err(e) => {
                warn!(error = %e, "could not build DTLS server context for the lossy UDP flow; lossy stays observe-only");
                None
            }
        };
        match ironrdp_server::UdpMultitransportListener::bind(
            args.bind,
            cfg,
            Some(udp_tls_config.clone()),
            Some(cookie_registry.clone()),
            Some(tunnel_rx),
            udp_dtls_config,
        )
        .await
        {
            Ok(listener) => {
                let migrate_egfx = multitransport::env_truthy("MACRDP_UDP_MIGRATE_EGFX");
                info!(
                    addr = %args.bind,
                    migrate_egfx,
                    "UDP multitransport listener bound — EGFX migrates to the reliable UDP tunnel \
                     when MACRDP_UDP_MIGRATE_EGFX is set (otherwise EGFX stays on TCP); \
                     input/audio/clipboard always ride TCP"
                );
                server
                    .set_multitransport_provider(Some(Box::new(multitransport::MacMultitransport)));
                server.set_multitransport_cookie_registry(Some(cookie_registry));
                server.set_multitransport_tunnel_sender(Some(tunnel_sender));
                // Ack-driven IDR recovery (EGFX-on-lossy): hand the server the same
                // flag the H.264 pipeline reads. The server flips it true only when
                // it migrates EGFX onto the LOSSY tunnel.
                server.set_egfx_on_lossy_handle(Some(egfx_on_lossy_flag.clone()));
                // (P2.4b, EXPERIMENTAL) Register the lossy-UDP audio DVC
                // (AUDIO_PLAYBACK_LOSSY_DVC). Behind its OWN dedicated env gate
                // (`MACRDP_UDP_LOSSY_AUDIO=1`) — NOT the lossy-offer flag — so the
                // proven lossy transport path runs unbroken by default; this
                // sub-step is opt-in until the handshake is verified on mstsc.
                // Requires AAC (the lossy DVC needs version >= 8 + AAC,
                // MS-RDPEA Appendix A note <2>). Advertises the SAME format list
                // the static RDPSND path encodes.
                if multitransport::env_truthy("MACRDP_UDP_LOSSY_AUDIO") && args.enable_aac {
                    let formats = audio::server_audio_formats(args.enable_aac, args.aac_bitrate);
                    info!(
                        formats = formats.len(),
                        "P2.4b (EXPERIMENTAL, MACRDP_UDP_LOSSY_AUDIO): offering AUDIO_PLAYBACK_LOSSY_DVC \
                         (lossy-UDP audio) — AAC negotiation over TCP"
                    );
                    server.set_multitransport_lossy_audio_formats(Some(formats));
                }
                Some(listener)
            }
            Err(e) => {
                // Non-fatal: fall back to TCP-only. Most common cause is the UDP
                // port already being in use.
                warn!(error = %e, addr = %args.bind, "could not bind UDP multitransport listener; continuing TCP-only");
                None
            }
        }
    } else {
        if args.enable_udp_multitransport {
            // Fork worker: the provider still offers UDP (M1), but the listener is
            // the supervisor's job (deferred), so the client falls back to TCP.
            server.set_multitransport_provider(Some(Box::new(multitransport::MacMultitransport)));
        }
        None
    };

    // ironrdp_server::Credentials holds a plain String, so this copy is
    // outside our control and won't be zeroed when the server shuts down.
    // Our Zeroizing<String> still wipes its own allocation at scope exit.
    server.set_credentials(Some(Credentials {
        username: username.clone(),
        password: (*password).clone(),
        domain: None,
    }));

    if let Some(fd) = worker_fd {
        // --fork-workers worker: the supervisor already accepted the TCP
        // connection and handed us its fd. Adopt it, serve exactly ONE
        // connection, then exit — so every (re)connect is a brand-new process
        // (xrdp's frontend model), the whole point of the PoC.
        // SAFETY: `fd` is a live, owned socket fd inherited from the supervisor
        // (CLOEXEC cleared before exec); we take sole ownership here.
        let std_stream = unsafe { std::net::TcpStream::from_raw_fd(fd) };
        std_stream
            .set_nonblocking(true)
            .context("set inherited worker socket non-blocking")?;
        let stream = tokio::net::TcpStream::from_std(std_stream)
            .context("adopt inherited worker socket into tokio")?;
        info!(fd, user = %username, "worker: serving one connection on inherited socket");
        let res = server.run_connection(stream).await;
        // CRITICAL (fork-workers): force-exit the worker PROCESS here instead of
        // returning and letting the runtime unwind. Once an SCStream is live,
        // ScreenCaptureKit's framework threads keep the process from actually
        // terminating on a normal return — the very hazard the SIGINT handler
        // dodges with process::exit. A lingering worker holds its capture stream
        // (and VideoToolbox session) open; after ~2 concurrent capturers macOS
        // starves frame delivery to the newer ones, so the 3rd+ reconnect renders
        // BLANK ("first two render, then blank"). Exiting tears down the
        // SCStream/VT immediately (and the `caffeinate -w <pid>` child exits with
        // us), so every reconnect's fresh worker captures cleanly.
        let code = match &res {
            Ok(()) => {
                info!("worker: connection ended cleanly, exiting");
                0
            }
            Err(e) => {
                info!(error = ?e, "worker: connection ended with error, exiting");
                1
            }
        };
        // process::exit skips Drop, so run the per-connection cleanup explicitly
        // (same as the signal handler): unmount any RDPDR NFS volumes and remove
        // clipboard paste temp dirs. Without this a worker LEAKS a mount + temp
        // dir on every reconnect (Drop normally handles it on a graceful return).
        #[cfg(target_os = "macos")]
        {
            file_promise_lazy::shutdown_cleanup();
            rdpdr::shutdown_cleanup();
        }
        std::process::exit(code);
    }

    info!(
        addr = %args.bind,
        user = %username,
        "macrdp listening — connect with: mstsc / xfreerdp at port {} as {}",
        args.bind.port(),
        username,
    );
    server.run().await
}

/// Resolves when the process receives SIGINT (Ctrl-C) or, on Unix, SIGTERM.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut sigterm) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = sigterm.recv() => {}
                }
            }
            Err(e) => {
                warn!("could not install SIGTERM handler: {e}");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod config_tests {
    use super::*;

    fn write_temp(name: &str, body: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "macrdp-cfgtest-{}-{}.env",
            std::process::id(),
            name
        ));
        fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn parses_full_config_into_flags() {
        let path = write_temp(
            "full",
            "# a comment, then a blank line\n\
             \n\
             export BIND=0.0.0.0:3390\n\
             USERNAME=alice\n\
             ENABLE_H264=1\n\
             ENABLE_AAC=0\n\
             HIDPI=1\n\
             UNMINIMIZE=1\n\
             VIRTUAL_DISPLAY=1\n\
             VD_WIDTH=2560\n\
             VD_HEIGHT=1440\n\
             PRIMARY_MODE=detach\n\
             FORK_WORKERS=1\n\
             ENABLE_UDP_MULTITRANSPORT=1\n\
             EXTRA_FLAGS=\"--fps 30\"\n",
        );
        let args = args_from_config(&path).unwrap();
        fs::remove_file(&path).ok();

        assert_eq!(args.bind.to_string(), "0.0.0.0:3390");
        assert_eq!(args.username.as_deref(), Some("alice"));
        assert!(args.enable_h264);
        assert!(!args.enable_aac);
        assert!(args.hidpi);
        assert!(args.unminimize_on_switch);
        assert!(args.virtual_display);
        assert_eq!(args.width, Some(2560));
        assert_eq!(args.height, Some(1440));
        assert!(args.detach_primary);
        assert!(!args.capture_primary);
        assert!(args.fork_workers);
        assert!(args.enable_udp_multitransport);
        // USE_KEYCHAIN defaults on (matches the old wrapper).
        assert!(args.keychain);
        // EXTRA_FLAGS is parsed as real CLI tokens.
        assert_eq!(args.fps, Some(30));
    }

    #[test]
    fn empty_config_matches_wrapper_defaults() {
        let path = write_temp("empty", "\n# nothing set\n");
        let args = args_from_config(&path).unwrap();
        fs::remove_file(&path).ok();

        assert_eq!(args.bind.to_string(), "127.0.0.1:3390");
        assert!(args.keychain);
        assert!(!args.virtual_display);
        assert!(!args.enable_h264);
        assert!(!args.fork_workers);
        assert!(!args.enable_udp_multitransport);
    }

    #[test]
    fn capture_primary_backcompat() {
        let path = write_temp(
            "bc",
            "VIRTUAL_DISPLAY=1\n\
             VD_WIDTH=1920\n\
             VD_HEIGHT=1080\n\
             CAPTURE_PRIMARY=1\n\
             USE_KEYCHAIN=0\n",
        );
        let args = args_from_config(&path).unwrap();
        fs::remove_file(&path).ok();

        assert!(args.capture_primary);
        assert!(!args.detach_primary);
        assert!(!args.keychain);
    }

    #[test]
    fn primary_mode_wins_over_legacy_capture_primary() {
        let path = write_temp(
            "winner",
            "VIRTUAL_DISPLAY=1\nVD_WIDTH=1920\nVD_HEIGHT=1080\nPRIMARY_MODE=detach\nCAPTURE_PRIMARY=1\n",
        );
        let args = args_from_config(&path).unwrap();
        fs::remove_file(&path).ok();

        assert!(args.detach_primary);
        assert!(!args.capture_primary);
    }
}
