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
mod auth_guard;
mod avc444;
mod camera;
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
mod health;
mod input;
mod keyboard_layout;
mod logging;
mod multitransport;
mod rdpdr;
mod reaper;
#[cfg(target_os = "macos")]
mod runloop_thread;
mod switcher_hud;
mod usb_redirect;
mod videotoolbox;
mod virtual_display;

use std::fs;
use std::io::{BufReader, IsTerminal};
use std::net::SocketAddr;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
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
use tracing::{info, warn};
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

    /// Operator-supplied TLS certificate (PEM; leaf first, then any chain).
    /// Use this to serve a real CA / ACME / Let's Encrypt cert instead of the
    /// self-signed default. Must be given together with --key; when set, macrdp
    /// uses exactly these files and NEVER falls back to self-signed (a missing
    /// file is a hard error). Default (neither flag): auto self-signed in
    /// --cert-dir. A cert change needs a restart.
    #[arg(long)]
    cert: Option<PathBuf>,

    /// Operator-supplied TLS private key (PEM) for --cert. Must be chmod 600 and
    /// readable by the macrdp user. Required together with --cert.
    #[arg(long)]
    key: Option<PathBuf>,

    /// Directory for the rotating log file (`macrdp.log`, size-bounded via
    /// MACRDP_LOG_MAX_BYTES / MACRDP_LOG_MAX_FILES). If unset, logs go to a
    /// rotating file in ~/Library/Logs when running headless (stdout is not a
    /// TTY, e.g. under the LaunchAgent) and to stdout when interactive.
    #[arg(long)]
    log_dir: Option<PathBuf>,

    /// Additionally write the security **audit** events (connection
    /// accept/reject/disconnect) to this file as one JSON object per line, for a
    /// SIEM/SOC log collector (Vector / Fluent Bit / rsyslog / Splunk UF) to tail
    /// and forward. Off by default; the human-readable audit lines still appear in
    /// the main log. The file self-rotates (MACRDP_AUDIT_LOG_MAX_BYTES /
    /// MACRDP_AUDIT_LOG_MAX_FILES). Setting MACRDP_AUDIT_JSON=1 enables it at the
    /// default path `<log-dir>/macrdp-audit.log` without naming a file here. See
    /// docs/siem-forwarding.md. Config key: AUDIT_FILE.
    #[arg(long)]
    audit_file: Option<PathBuf>,

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

    /// EXPERIMENTAL, opt-in (default OFF). Generic USB redirection (MS-RDPEUSB /
    /// the URBDRC dynamic channel): the connecting client redirects a physical USB
    /// device and macrdp presents it as a REAL local device via a user-space virtual
    /// USB host controller (IOUSBHost UserHCI) — e.g. a redirected flash drive mounts
    /// in Finder. Needs the entitled (signed+provisioned) build (the
    /// com.apple.developer.usb.host-controller-interface entitlement); a plain build
    /// logs "controller unavailable" and no-ops. Mass storage is verified end-to-end;
    /// other device classes are untested. The client must opt in too (FreeRDP: /usb).
    /// mstsc gates USB behind Group Policy: enable "Allow RDP redirection of other
    /// supported RemoteFX USB devices from this computer" (Computer Config -> Admin
    /// Templates -> Windows Components -> Remote Desktop Services -> Remote Desktop
    /// Connection Client -> RemoteFX USB Device Redirection) + reboot, then select the
    /// device under Local Resources -> More -> USB. On mstsc a device now enumerates,
    /// configures, and negotiates its format, but the client doesn't deliver the actual
    /// video/data frames (a webcam's video rides mstsc's separate camera-redirection
    /// channel, not implemented). macOS-only.
    #[arg(long)]
    enable_usb_redirection: bool,

    /// EXPERIMENTAL, opt-in (default OFF). Camera redirection (MS-RDPECAM) —
    /// **Phase 0 protocol gate only**. Advertises the `RDCamera_Device_Enumerator`
    /// DVC and logs the client's camera announcement (`DEVICE_ADDED_NOTIFICATION`)
    /// so we can confirm a modern mstsc/Win11 will hand macrdp a redirected webcam
    /// over MS-RDPECAM. It does NOT present a camera yet (no per-device channel, no
    /// stream, no macOS code). The client must opt in too (mstsc: Local Resources ->
    /// More -> "Video capture devices"). Cross-platform (pure protocol). See
    /// docs/rdp-camera-redirection-feasibility.md.
    #[arg(long)]
    enable_camera_redirection: bool,

    /// EXPERIMENTAL, opt-in (default OFF). Offer RDP UDP multitransport
    /// (MS-RDPEMT over reliable RDPEUDP) to clients that advertise it, and bind a
    /// UDP listener on the same address/port as TCP. On its own, EGFX stays on TCP
    /// (the proven safe spike) — pass --udp-migrate-egfx to actually move the
    /// H.264 video onto the reliable UDP tunnel. Input, audio (RDPSND), and
    /// clipboard always ride TCP. macOS-only build; see
    /// docs/rdp-udp-multitransport-feasibility.md.
    #[arg(long)]
    enable_udp_multitransport: bool,

    /// EXPERIMENTAL, opt-in (default OFF; requires --enable-udp-multitransport).
    /// Migrate the EGFX (H.264) channel onto the reliable UDP tunnel via MS-RDPEDYC
    /// Soft-Sync (verified rendering on mstsc). Without it, EGFX stays on TCP even
    /// when multitransport is offered. **Caveat — clean-link feature only:** the
    /// reliable tunnel is an ordered stream, so under packet loss it head-of-line-
    /// blocks like TCP and, once the client abandons the tunnel, EGFX freezes with
    /// no recovery until reconnect (audio survives on TCP). Use only on a low-loss
    /// link. (Promoted from the MACRDP_UDP_MIGRATE_EGFX env var, which still works
    /// as a fallback.) macOS-only build; see docs/rdp-udp-multitransport-feasibility.md.
    #[arg(long)]
    udp_migrate_egfx: bool,

    /// EXPERIMENTAL, opt-in (default OFF). Congestion-responsive H.264 bitrate for
    /// EGFX-over-UDP: when the reliable UDP tunnel shows packet loss (retransmits),
    /// lower the VideoToolbox bitrate toward a floor (AIMD multiplicative-decrease),
    /// and climb back toward the --bitrate ceiling when the link clears — so video
    /// degrades to "choppy but alive" under loss instead of wedging. Only acts while
    /// EGFX is on a UDP tunnel (no-op on TCP). Tunables: MACRDP_UDP_ADAPTIVE_FLOOR_BPS,
    /// _INCREASE_BPS, _DECREASE, _INTERVAL_MS. macOS-only build.
    #[arg(long)]
    adaptive_bitrate: bool,

    /// EXPERIMENTAL, opt-in (default OFF; implies --enable-udp-multitransport,
    /// requires --enable-aac and --enable-h264). Stream RDPSND audio over a LOSSY
    /// UDP/DTLS tunnel with 1+1 redundancy instead of TCP — the loss-resilient audio
    /// path. The MS-RDPEA format handshake runs on a reliable DVC over TCP; AAC Wave2
    /// data is Soft-Synced onto a lossy (UdpFecL) RDPEUDP flow (deliver-on-arrival, no
    /// retransmit) and each datagram is sent TWICE (the client's DTLS anti-replay
    /// dedups, so audio never double-plays), so an independent-loss link of rate p
    /// drops a payload only at p² — verified smooth on real mstsc at 5/10/15% loss
    /// where the single-send path glitches. Collapses the
    /// MACRDP_UDP_{OFFER_FECL,LOSSY_DELIVERY,LOSSY_AUDIO,LOSSY_AUDIO_DUP} env gates
    /// (which still work as a fallback). macOS-only build; see
    /// docs/rdp-udp-multitransport-feasibility.md.
    #[arg(long)]
    enable_lossy_audio: bool,

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

    /// Cap the resolution a client can request on the auto-adopt path
    /// (defense-in-depth resource bound; e.g. 2560x1440). A request above the
    /// cap is clamped per-dimension and the session is served at the clamped
    /// size. Without it, an authenticated client can request up to the
    /// protocol maximum 8192x8192 — a ~256 MB BGRA framebuffer per frame.
    /// Each dimension must be in the RDP band [200, 8192]. No effect with
    /// --no-client-resolution or an explicit --width/--height/--hidpi/
    /// --virtual-display (those pin the size). Config: MAX_CLIENT_SIZE.
    #[arg(long = "max-client-size", value_name = "WxH", value_parser = parse_max_client_size)]
    max_client_size: Option<(u16, u16)>,

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

    /// Also accept Option+` (Alt+backtick from the client) as a trigger for the
    /// within-app window cycle, in addition to Cmd+`. Off by default. Mirrors
    /// --alt-tab-switch's relationship to Cmd+Tab, for clients/configs that
    /// forward Alt+` but gate the Windows-key combo. Option+Shift+` cycles
    /// backward. When off, Option+` reaches remote apps as a normal key.
    /// macOS-only.
    #[arg(long = "alt-backtick-switch")]
    alt_backtick_switch: bool,

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

    /// EXPERIMENTAL research spike (not a real feature): instantiate a user-space
    /// USB host controller via IOUSBHostControllerInterface to prove the
    /// generic-USB-redirection route works, print GO/NO-GO, then exit. Requires a
    /// signed+provisioned build carrying the com.apple.developer.usb.host-controller-
    /// interface entitlement (packaging/make-app.sh PROVISION_PROFILE=...). macOS-only.
    #[arg(long = "usb-spike")]
    usb_spike: bool,
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

/// Expiry classification for an operator-supplied cert. Pure (testable) split
/// from the logging in [`warn_if_expiring`].
#[derive(Debug, PartialEq, Eq)]
enum CertExpiry {
    Expired,
    /// Within the warn window; carries days remaining.
    Soon(u64),
    Ok,
}

fn classify_expiry(
    not_after: std::time::SystemTime,
    now: std::time::SystemTime,
    warn_days: u64,
) -> CertExpiry {
    match not_after.duration_since(now) {
        Err(_) => CertExpiry::Expired,
        Ok(remaining) => {
            let days = remaining.as_secs() / 86_400;
            if days <= warn_days {
                CertExpiry::Soon(days)
            } else {
                CertExpiry::Ok
            }
        }
    }
}

/// Warn (never refuse) if an operator's leaf cert is expired or near expiry.
/// Refusing would risk locking an operator out of their own box over a stale
/// cert; a loud warning is the right call. Best-effort: a parse failure is
/// silently skipped (the cert already loaded into rustls, so it's structurally
/// fine; only the validity read is advisory).
fn warn_if_expiring(certs: &[CertificateDer<'static>]) {
    let Some(leaf) = certs.first() else { return };
    let Ok(parsed) = Certificate::from_der(leaf.as_ref()) else {
        return;
    };
    let not_after = parsed.tbs_certificate.validity.not_after.to_system_time();
    match classify_expiry(not_after, std::time::SystemTime::now(), 14) {
        CertExpiry::Expired => warn!(
            "operator TLS certificate has EXPIRED — clients will reject it; renew it and restart macrdp"
        ),
        CertExpiry::Soon(days) => warn!(
            days_left = days,
            "operator TLS certificate expires soon — renew before it lapses"
        ),
        CertExpiry::Ok => {}
    }
}

/// Build the TLS material. With `cert`/`key` set (operator-supplied), load
/// exactly those files — a missing/unreadable/bad-permission file is a hard
/// error, NEVER a silent self-signed fallback. With neither set, use the
/// self-signed default in `cert_dir` (load if present, else generate+persist).
/// Callers must validate the both-or-neither invariant first.
fn make_tls_acceptor(
    cert_dir: &Path,
    cert: Option<&Path>,
    key: Option<&Path>,
) -> Result<TlsMaterial> {
    let (certs, key) = match (cert, key) {
        (Some(c), Some(k)) => {
            info!(cert = %c.display(), key = %k.display(), "loading operator-supplied TLS cert");
            let loaded = load_pem_cert_and_key(c, k)?;
            warn_if_expiring(&loaded.0);
            loaded
        }
        (None, None) => {
            let cert_path = cert_dir.join("cert.pem");
            let key_path = cert_dir.join("key.pem");
            if cert_path.exists() && key_path.exists() {
                info!(dir = %cert_dir.display(), "loading persisted TLS cert");
                load_pem_cert_and_key(&cert_path, &key_path)?
            } else {
                info!(dir = %cert_dir.display(), "generating new self-signed TLS cert");
                generate_and_persist(cert_dir, &cert_path, &key_path)?
            }
        }
        _ => anyhow::bail!("--cert and --key must be supplied together (or neither)"),
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

    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("build rustls ServerConfig")?;
    // Honor SSLKEYLOGFILE (Wireshark TLS decryption) for protocol debugging.
    // KeyLogFile is a no-op unless the env var is set. Covers the TCP RDP
    // connection AND the reliable-UDP multitransport flow (same config); the
    // lossy flow's DTLS (boring) is not covered.
    config.key_log = Arc::new(rustls::KeyLogFile::new());
    if std::env::var_os("SSLKEYLOGFILE").is_some() {
        warn!(
            "SSLKEYLOGFILE is set — TLS session keys are being written to that file for \
             debugging; unset it outside of protocol-capture sessions"
        );
    }
    let config = Arc::new(config);
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
/// Parse a `--max-client-size` spec ("WxH", e.g. "2560x1440") into a
/// per-dimension cap, validating each dimension against the RDP desktop band
/// [200, 8192] (MS-RDPBCGR) — the same band the acceptor's sanity check uses,
/// so a clamped size can never fall below the protocol minimum.
fn parse_max_client_size(spec: &str) -> Result<(u16, u16), String> {
    let lower = spec.trim().to_ascii_lowercase();
    let (w, h) = lower
        .split_once('x')
        .ok_or_else(|| format!("expected WxH (e.g. 2560x1440), got '{spec}'"))?;
    let parse_dim = |s: &str, name: &str| -> Result<u16, String> {
        let v: u16 = s
            .trim()
            .parse()
            .map_err(|_| format!("{name} '{}' is not a number in 0..=65535", s.trim()))?;
        if !(200..=8192).contains(&v) {
            return Err(format!(
                "{name} {v} is outside the RDP desktop band 200..=8192 (MS-RDPBCGR)"
            ));
        }
        Ok(v)
    };
    Ok((parse_dim(w, "width")?, parse_dim(h, "height")?))
}

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
    if on("ALT_BACKTICK_SWITCH", false) {
        argv.push("--alt-backtick-switch".into());
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
    if on("ENABLE_USB_REDIRECTION", false) {
        argv.push("--enable-usb-redirection".into());
    }
    if on("ENABLE_CAMERA_REDIRECTION", false) {
        argv.push("--enable-camera-redirection".into());
    }
    if on("ENABLE_UDP_MULTITRANSPORT", false) {
        argv.push("--enable-udp-multitransport".into());
    }
    if on("UDP_MIGRATE_EGFX", false) {
        argv.push("--udp-migrate-egfx".into());
    }
    if on("ADAPTIVE_BITRATE", false) {
        argv.push("--adaptive-bitrate".into());
    }
    if on("ENABLE_LOSSY_AUDIO", false) {
        argv.push("--enable-lossy-audio".into());
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
    if let Some(dir) = cfg.get("LOG_DIR") {
        if !dir.is_empty() {
            argv.push("--log-dir".into());
            argv.push(dir.clone());
        }
    }
    if let Some(path) = cfg.get("AUDIT_FILE") {
        if !path.is_empty() {
            argv.push("--audit-file".into());
            argv.push(path.clone());
        }
    }
    if let Some(path) = cfg.get("TLS_CERT") {
        if !path.is_empty() {
            argv.push("--cert".into());
            argv.push(path.clone());
        }
    }
    if let Some(path) = cfg.get("TLS_KEY") {
        if !path.is_empty() {
            argv.push("--key".into());
            argv.push(path.clone());
        }
    }
    if let Some(sz) = cfg.get("MAX_CLIENT_SIZE") {
        if !sz.is_empty() {
            argv.push("--max-client-size".into());
            argv.push(sz.clone());
        }
    }
    // Env-only tunables (auth guard, health-check watchdog, blank recovery,
    // auto-reconnect cookie — each read lazily after this point). Bridge the
    // friendly config.env keys to their MACRDP_* env vars here so menu-bar /
    // LaunchAgent users can tune them via config.env without editing the
    // plist's EnvironmentVariables. Unset keys keep the on-by-default defaults.
    for (cfg_key, env_var) in [
        ("CONN_GUARD", "MACRDP_CONN_GUARD"),
        ("AUDIT_LOG", "MACRDP_AUDIT_LOG"),
        ("GUARD_RL_MAX", "MACRDP_GUARD_RL_MAX"),
        ("GUARD_RL_WINDOW_SECS", "MACRDP_GUARD_RL_WINDOW_SECS"),
        ("GUARD_FAIL_THRESHOLD", "MACRDP_GUARD_FAIL_THRESHOLD"),
        ("GUARD_FAILFAST_SECS", "MACRDP_GUARD_FAILFAST_SECS"),
        (
            "GUARD_COOLDOWN_BASE_SECS",
            "MACRDP_GUARD_COOLDOWN_BASE_SECS",
        ),
        ("GUARD_COOLDOWN_MAX_SECS", "MACRDP_GUARD_COOLDOWN_MAX_SECS"),
        // Health-check watchdog (env-driven like the auth guard; on by default
        // when headless). See src/health.rs.
        ("HEALTH_CHECK", "MACRDP_HEALTHCHECK"),
        (
            "HEALTHCHECK_INTERVAL_SECS",
            "MACRDP_HEALTHCHECK_INTERVAL_SECS",
        ),
        (
            "HEALTHCHECK_TIMEOUT_SECS",
            "MACRDP_HEALTHCHECK_TIMEOUT_SECS",
        ),
        ("HEALTHCHECK_FAILURES", "MACRDP_HEALTHCHECK_FAILURES"),
        // Blank recovery (the mstsc reconnect-blank auto-heal, needs
        // --enable-h264) + the auto-reconnect cookie. Env-driven like the
        // guard (read in h264.rs per connection / main.rs at server build,
        // both after this bridge runs). BLANK_RECOVERY=0 is the ask that
        // motivated bridging: disabling the detector for an overlay-link
        // profile used to need a plist EnvironmentVariables edit.
        ("BLANK_RECOVERY", "MACRDP_BLANK_RECOVERY"),
        (
            "BLANK_RECOVERY_REACTIVATE",
            "MACRDP_BLANK_RECOVERY_REACTIVATE",
        ),
        (
            "BLANK_RECOVERY_MAX_RTT_MS",
            "MACRDP_BLANK_RECOVERY_MAX_RTT_MS",
        ),
        ("BLANK_RECOVERY_MIN_QOE", "MACRDP_BLANK_RECOVERY_MIN_QOE"),
        (
            "BLANK_RECOVERY_MAX_WAIT_MS",
            "MACRDP_BLANK_RECOVERY_MAX_WAIT_MS",
        ),
        (
            "BLANK_RECOVERY_MIN_WALL_REPORTS",
            "MACRDP_BLANK_RECOVERY_MIN_WALL_REPORTS",
        ),
        ("BLANK_RECOVERY_ARM_MS", "MACRDP_BLANK_RECOVERY_ARM_MS"),
        ("BLANK_RECOVERY_RETRY_MS", "MACRDP_BLANK_RECOVERY_RETRY_MS"),
        (
            "BLANK_RECOVERY_MAX_ATTEMPTS",
            "MACRDP_BLANK_RECOVERY_MAX_ATTEMPTS",
        ),
        (
            "BLANK_RECOVERY_MAX_CONSECUTIVE_DROPS",
            "MACRDP_BLANK_RECOVERY_MAX_CONSECUTIVE_DROPS",
        ),
        ("AUTO_RECONNECT", "MACRDP_AUTO_RECONNECT"),
    ] {
        if let Some(val) = cfg.get(cfg_key) {
            if !val.is_empty() {
                std::env::set_var(env_var, val);
            }
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

    // Research spike (Phase-1b USB-redirection go/no-go): run the UserHCI probe
    // and exit before any server/auth/capture setup. Requires the signed+
    // provisioned entitled build; see src/usb_redirect/.
    if args.usb_spike {
        std::process::exit(usb_redirect::run_spike());
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
    // Resolve the JSON audit-stream sink (SIEM/SOC): an explicit --audit-file /
    // AUDIT_FILE wins; otherwise MACRDP_AUDIT_JSON=1 enables it at the default
    // `<log-dir-or-~/Library/Logs>/macrdp-audit.log`. Off (None) by default, so the
    // logging setup is byte-identical unless an operator opts in.
    let audit_json_env = std::env::var("MACRDP_AUDIT_JSON").ok().is_some_and(|v| {
        !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "no" | "off"
        )
    });
    let audit_file: Option<PathBuf> = args.audit_file.clone().or_else(|| {
        if audit_json_env {
            let dir = args
                .log_dir
                .clone()
                .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Logs")))
                .unwrap_or_else(|| PathBuf::from("."));
            Some(dir.join("macrdp-audit.log"))
        } else {
            None
        }
    });
    logging::init(filter, args.log_dir.as_deref(), audit_file.as_deref());

    // Sweep leftovers from a PRIOR macrdp that died uncleanly (SIGKILL / panic /
    // power-loss skip Drop AND the signal handler, stranding NFS mounts + paste
    // temp dirs). Dead-pid-gated so it's safe with another instance live; on a
    // detached thread so a stale `umount` can never delay startup.
    #[cfg(target_os = "macos")]
    std::thread::spawn(|| {
        rdpdr::reap_stale();
        file_promise::reap_stale();
        file_promise_lazy::reap_stale();
    });

    // Arm the health-check watchdog on the long-lived, launchd-watched process
    // so a hung-but-alive runtime — which KeepAlive can't tell from a healthy
    // one — gets bounced into a restart. Skipped by default when interactive
    // (stdout is a TTY, e.g. `cargo run`, where nothing would restart a bounce).
    // MACRDP_HEALTHCHECK=0/1 overrides; see src/health.rs.
    if health::should_arm(
        std::io::stdout().is_terminal(),
        std::env::var("MACRDP_HEALTHCHECK").ok().as_deref(),
    ) {
        health::spawn(
            tokio::runtime::Handle::current(),
            health::HealthConfig::from_env(),
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
        if !args.allow_sleep {
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
    // Arc<Mutex<>> because the display is shared with `CaptureDisplay` for
    // live client-driven resize: the capture side re-modes it via
    // `VirtualDisplay::resize` when the client's window size changes. This
    // scope still holds a clone for the lifetime/teardown semantics
    // described above.
    let virtual_display: Option<Arc<std::sync::Mutex<virtual_display::VirtualDisplay>>> =
        if args.virtual_display {
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
            Some(Arc::new(std::sync::Mutex::new(vd)))
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
    if args.detach_primary {
        let vd_id = virtual_display
            .as_ref()
            .expect("checked above when --detach-primary")
            .lock()
            .expect("virtual display mutex poisoned")
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
            .lock()
            .expect("virtual display mutex poisoned")
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
            .lock()
            .expect("virtual display mutex poisoned")
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
    // Operator-supplied cert/key are all-or-nothing: one without the other is a
    // config error (we won't guess the missing half or silently self-sign).
    if args.cert.is_some() != args.key.is_some() {
        return Err(anyhow!(
            "--cert and --key must be supplied together (or neither, for the self-signed default)"
        ));
    }
    let TlsMaterial {
        acceptor: tls,
        spki_der,
        config: udp_tls_config,
        cert_der: udp_cert_der,
        key_der: udp_key_der,
    } = make_tls_acceptor(&cert_dir, args.cert.as_deref(), args.key.as_deref())?;

    // Resolve desktop dimensions + geometry. Three paths:
    //   - virtual display: width/height are already enforced as required
    //     above; geometry comes from the vdisplay's CGDisplayBounds.
    //   - primary panel, no --width/--height override: query SCK for
    //     native size and use CGDisplay::main() for the point-space bounds.
    //   - primary panel with override: use the override + main geometry.
    let (width, height, capture_display_id, screen_size_pts) = if let Some(vd) = &virtual_display {
        let vd = vd.lock().expect("virtual display mutex poisoned");
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
    crate::input::set_alt_backtick_switch(args.alt_backtick_switch);
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
        spawn_hud_helper()
    } else {
        None
    };

    // Shared flag flipped true when EGFX is Soft-Synced onto a LOSSY UDP tunnel
    // (UDPFECL). The H.264 pipeline reads it to arm ack-driven IDR recovery; the
    // server flips it at the migration site. Cross-platform so the UDP listener
    // wiring below (which hands the same clone to the server) compiles on CI.
    let egfx_on_lossy_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Companion flag: EGFX migrated onto ANY UDP tunnel (reliable or lossy). The
    // H.264 pipeline reads it to enable frame-ack-lag backpressure (the UDP tunnel
    // has no socket pacing); the server flips it at the same migration site.
    let egfx_on_udp_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // EGFX-over-UDP → TCP watchdog request: the H.264 pipeline sets it true when the
    // reliable UDP tunnel wedges (acks silent while shipping); the server reads it to
    // flip EGFX routing back to TCP. Reset on reconnect (server side).
    let egfx_demigrate_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Adaptive-bitrate loss signal: cumulative reliable-tunnel retransmit count the
    // UDP listener bumps and the H.264 controller samples (deltas) to drive
    // congestion-responsive bitrate. Cross-platform so the listener wiring compiles
    // on CI; only read on the macOS H.264 path.
    let congestion_retransmits = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));

    // Kernel-measured TCP RTT (ms) of the most recently accepted connection,
    // written by the vendored server at accept (divergence 15). Drives the
    // link-aware blank-recovery gate + the adaptive-bitrate seed in h264.rs.
    // 0 = unknown.
    let link_rtt_ms = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));

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
            egfx_on_udp_flag.clone(),
            egfx_demigrate_flag.clone(),
            args.adaptive_bitrate,
            congestion_retransmits.clone(),
            link_rtt_ms.clone(),
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
        // Live in-session resize (client drags its window, sending an
        // MS-RDPEDISP monitor-layout PDU) — the counterpart to the
        // connect-time auto-adopt above. See `CaptureDisplay::request_layout`.
        pending_resize: capture::PendingResize::new(),
        max_client_size: args
            .max_client_size
            .map(|(width, height)| ironrdp_server::DesktopSize { width, height }),
        suppress_next_adopt: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        // Shared so a live client resize can re-mode the display; None on
        // the mirror-primary path (no virtual display to re-mode).
        virtual_display: virtual_display.clone(),
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

    // Server-direction USB redirection (--enable-usb-redirection, opt-in). Phase
    // 3.0 installs the observe-only URBDRC DVC processor (capability exchange +
    // device-announce logging). Ships inert when the flag is off.
    let usb_factory: Option<Box<dyn ironrdp_server::UrbdrcServerFactory>> =
        if args.enable_usb_redirection {
            Some(Box::new(usb_redirect::MacUsb::new()))
        } else {
            None
        };

    // Camera redirection (--enable-camera-redirection, opt-in) — Phase 0 protocol
    // gate. Installs the MS-RDPECAM enumeration processor that advertises
    // RDCamera_Device_Enumerator and logs the client's camera announcement. Ships
    // inert when the flag is off.
    let camera_factory: Option<Box<dyn ironrdp_server::RdCameraServerFactory>> =
        if args.enable_camera_redirection {
            Some(Box::new(camera::MacCamera::new()))
        } else {
            None
        };

    // Auth hardening (Tier 1.2): per-IP rate-limit + lockout + audit log via the
    // server's pre-handshake/post-disconnect ConnectionHandler seam. On by default
    // (MACRDP_CONN_GUARD=0 disables).
    let conn_handler: Option<Box<dyn ironrdp_server::ConnectionHandler>> =
        auth_guard::AuthGuardHandler::from_env();

    let mut server = RdpServer::builder()
        .with_addr(args.bind)
        .with_hybrid(tls, spki_der)
        .with_input_handler(input_handler)
        .with_display_handler(display)
        .with_cliprdr_factory(Some(cliprdr))
        .with_sound_factory(Some(sound))
        .with_rdpdr_factory(rdpdr_factory)
        .with_usb_factory(usb_factory)
        .with_camera_factory(camera_factory)
        .with_bitmap_codecs(bitmap_codecs())
        .with_gfx_factory(gfx_factory)
        .with_connection_handler(conn_handler)
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

    // The server samples the kernel's smoothed TCP RTT for each accepted
    // connection (divergence 15) into this cell; the H.264 pipeline reads it
    // for link-aware blank-recovery gating + adaptive-bitrate seeding.
    server.set_link_rtt_handle(link_rtt_ms.clone());

    // Client-resolution auto-adopt: the vendored acceptor reads the desktop
    // size the client requests in its GCC Client Core Data and negotiates
    // the session at that size from the start (Demand Active); the
    // CaptureDisplay's `request_initial_size` then adopts it into the
    // shared `desktop_size` that capture, input scaling, and the H.264
    // pipeline all read.
    server.set_honor_client_desktop_size(auto_size);
    // Optional operator ceiling for the adopted size (--max-client-size,
    // defense-in-depth): a request above the cap is clamped per-dimension in
    // the acceptor. Meaningless off the auto-adopt path (nothing is adopted),
    // so warn rather than silently ignore.
    if let Some((max_w, max_h)) = args.max_client_size {
        if auto_size {
            server.set_honor_client_desktop_size_max(Some(ironrdp_server::DesktopSize {
                width: max_w,
                height: max_h,
            }));
            info!(
                max_w,
                max_h, "client-requested session size capped at the operator maximum"
            );
        } else {
            warn!(
                "--max-client-size has no effect: client-resolution auto-adopt is off \
                 (--no-client-resolution or an explicit --width/--height/--hidpi/--virtual-display)"
            );
        }
    }

    // Server Auto-Reconnect Cookie (MS-RDPBCGR ARC): provision it so a client
    // (mstsc) auto-reconnects on an ungraceful drop instead of showing
    // "disconnected". This is what makes the EGFX blank-recovery connection drop
    // (src/h264.rs, when a reconnect lands on mstsc's stale surface and never
    // presents) heal seamlessly — the client re-establishes on its own. Default
    // on (harmless + standard RDP server behavior); MACRDP_AUTO_RECONNECT=0
    // disables. The returning ARC_CS cookie is not validated (single console
    // session, NLA re-auths every connection), so a fixed per-process value is
    // fine — it only enables the client's auto-reconnect loop.
    let auto_reconnect = !matches!(
        std::env::var("MACRDP_AUTO_RECONNECT").as_deref(),
        Ok("0") | Ok("false") | Ok("FALSE")
    );
    if auto_reconnect {
        let mut random_bits = [0u8; 16];
        match getrandom::getrandom(&mut random_bits) {
            Ok(()) => {
                // logon_id is informational for us; a stable per-process id.
                let logon_id = std::process::id();
                server.set_auto_reconnect_cookie(logon_id, random_bits);
                info!("server auto-reconnect cookie provisioned (clients auto-reconnect on an ungraceful drop)");
            }
            Err(e) => {
                warn!(error = %e, "could not generate auto-reconnect cookie random bits — skipping")
            }
        }
    }

    // EXPERIMENTAL UDP multitransport (MS-RDPEMT). When enabled, install the
    // provider so the server offers reliable UDP to clients that advertise it,
    // and (M3) bind a real UDP listener on the same address/port as TCP — the
    // client reuses the server address for the auxiliary UDP flow. The handle is
    // held in `_udp_listener` for the process lifetime (Drop aborts its task).
    //
    // M3 scope: the listener answers the RDPEUDP SYN→SYN+ACK handshake (V3/EUDP2
    // negotiation); cookie validation is soft and there's no TLS/EMT tunnel or
    // channel migration yet, so a client that completes the handshake still runs
    // the session over TCP.
    //
    // `--enable-lossy-audio` is the one-switch promotion of the verified lossy-audio
    // path: it bridges the three expert env gates the vendored listener + provider
    // read (offer the lossy UdpFecL transport, use lossy deliver-on-arrival delivery,
    // and 1+1 duplicate sends), and implies the UDP listener below. The macrdp-side
    // `MACRDP_UDP_LOSSY_AUDIO` gate is OR'd directly at the registration site. Setting
    // env here (single-threaded, before the listener task spawns + before any
    // connection reads the provider) is the minimal bridge — no vendored signature
    // change. The env gates still work standalone as a fallback.
    if args.enable_lossy_audio {
        std::env::set_var("MACRDP_UDP_OFFER_FECL", "1");
        std::env::set_var("MACRDP_UDP_LOSSY_DELIVERY", "1");
        std::env::set_var("MACRDP_UDP_LOSSY_AUDIO_DUP", "1");
        if !args.enable_aac || !args.enable_h264 {
            warn!(
                enable_aac = args.enable_aac,
                enable_h264 = args.enable_h264,
                "--enable-lossy-audio needs --enable-aac (MS-RDPEA requires AAC for the lossy DVC) \
                 AND --enable-h264 (the lossy-audio Soft-Sync rides the EGFX dispatch path); without \
                 both, audio stays on TCP"
            );
        }
    }
    let _udp_listener = if args.enable_udp_multitransport || args.enable_lossy_audio {
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
            Some(congestion_retransmits.clone()),
        )
        .await
        {
            Ok(listener) => {
                // EGFX migration is controlled by --udp-migrate-egfx; the legacy
                // MACRDP_UDP_MIGRATE_EGFX env var still works as a fallback (the
                // lossy-isolation test MACRDP_UDP_MIGRATE_EGFX_LOSSY relies on it).
                let migrate_egfx =
                    args.udp_migrate_egfx || multitransport::env_truthy("MACRDP_UDP_MIGRATE_EGFX");
                info!(
                    addr = %args.bind,
                    migrate_egfx,
                    "UDP multitransport listener bound — EGFX migrates to the reliable UDP tunnel \
                     when --udp-migrate-egfx is set (otherwise EGFX stays on TCP); \
                     input/audio/clipboard always ride TCP"
                );
                server
                    .set_multitransport_provider(Some(Box::new(multitransport::MacMultitransport)));
                server.set_migrate_egfx(migrate_egfx);
                server.set_multitransport_cookie_registry(Some(cookie_registry));
                server.set_multitransport_tunnel_sender(Some(tunnel_sender));
                // Ack-driven IDR recovery (EGFX-on-lossy): hand the server the same
                // flag the H.264 pipeline reads. The server flips it true only when
                // it migrates EGFX onto the LOSSY tunnel.
                server.set_egfx_on_lossy_handle(Some(egfx_on_lossy_flag.clone()));
                // Frame-ack-lag backpressure: the server flips this true whenever it
                // migrates EGFX onto a UDP tunnel (reliable or lossy), and the H.264
                // pipeline drops captures when the client's decode backlog runs away.
                server.set_egfx_on_udp_handle(Some(egfx_on_udp_flag.clone()));
                // EGFX-over-UDP → TCP watchdog: the H.264 pipeline sets this true when
                // the reliable UDP tunnel wedges; the server reads it to de-migrate
                // EGFX back to TCP (mstsc renders it post-Soft-Sync). Reset on reconnect.
                server.set_demigrate_request_handle(Some(egfx_demigrate_flag.clone()));
                // (P2.4b) Register the lossy-UDP audio DVC (AUDIO_PLAYBACK_LOSSY_DVC),
                // driven by `--enable-lossy-audio` (or the legacy `MACRDP_UDP_LOSSY_AUDIO`
                // env fallback). Requires AAC (the lossy DVC needs version >= 8 + AAC,
                // MS-RDPEA Appendix A note <2>). Advertises the SAME format list the
                // static RDPSND path encodes; the lossy 1+1 transport is enabled by the
                // env bridge above. Verified on mstsc smooth at 5/10/15% loss.
                if (args.enable_lossy_audio || multitransport::env_truthy("MACRDP_UDP_LOSSY_AUDIO"))
                    && args.enable_aac
                {
                    let formats = audio::server_audio_formats(args.enable_aac, args.aac_bitrate);
                    info!(
                        formats = formats.len(),
                        "P2.4b lossy-UDP audio (--enable-lossy-audio): offering AUDIO_PLAYBACK_LOSSY_DVC \
                         with 1+1 redundancy — AAC negotiation over TCP"
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
mod max_client_size_tests {
    use super::parse_max_client_size;

    #[test]
    fn parses_wxh() {
        assert_eq!(parse_max_client_size("2560x1440"), Ok((2560, 1440)));
        // Case-insensitive separator + surrounding whitespace tolerated.
        assert_eq!(parse_max_client_size(" 1920X1080 "), Ok((1920, 1080)));
        // Band edges are inclusive.
        assert_eq!(parse_max_client_size("200x200"), Ok((200, 200)));
        assert_eq!(parse_max_client_size("8192x8192"), Ok((8192, 8192)));
    }

    #[test]
    fn rejects_out_of_band_and_garbage() {
        // Below the protocol minimum (would let the clamp produce an
        // un-servable size) and above the protocol maximum (meaningless).
        assert!(parse_max_client_size("199x1080").is_err());
        assert!(parse_max_client_size("1920x8193").is_err());
        // Not WxH at all.
        assert!(parse_max_client_size("1920").is_err());
        assert!(parse_max_client_size("axb").is_err());
        assert!(parse_max_client_size("1920x").is_err());
        assert!(parse_max_client_size("-1x1080").is_err());
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
             ENABLE_UDP_MULTITRANSPORT=1\n\
             UDP_MIGRATE_EGFX=1\n\
             MAX_CLIENT_SIZE=2560x1440\n\
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
        assert!(args.enable_udp_multitransport);
        assert!(args.udp_migrate_egfx);
        assert_eq!(args.max_client_size, Some((2560, 1440)));
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
        assert!(!args.enable_udp_multitransport);
        assert!(!args.udp_migrate_egfx);
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

    #[test]
    fn config_maps_tls_cert_and_key() {
        let path = write_temp(
            "tls",
            "TLS_CERT=/etc/ssl/macrdp.pem\nTLS_KEY=/etc/ssl/macrdp.key\n",
        );
        let args = args_from_config(&path).unwrap();
        fs::remove_file(&path).ok();
        assert_eq!(args.cert.as_deref(), Some(Path::new("/etc/ssl/macrdp.pem")));
        assert_eq!(args.key.as_deref(), Some(Path::new("/etc/ssl/macrdp.key")));
    }
}

#[cfg(test)]
mod tls_tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn unique_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "macrdp-tlstest-{tag}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Write a fresh self-signed cert+key as operator-style PEM files (key 0600,
    /// as `load_pem_cert_and_key` requires). Returns (cert_path, key_path).
    fn write_operator_pem(dir: &Path) -> (PathBuf, PathBuf) {
        let ck = rcgen::generate_simple_self_signed(vec!["macrdp.example".to_string()])
            .expect("gen cert");
        let cert_path = dir.join("operator-cert.pem");
        let key_path = dir.join("operator-key.pem");
        fs::write(&cert_path, ck.cert.pem()).unwrap();
        fs::write(&key_path, ck.key_pair.serialize_pem()).unwrap();
        fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600)).unwrap();
        (cert_path, key_path)
    }

    #[test]
    fn classify_expiry_covers_expired_soon_ok() {
        let now = SystemTime::now();
        assert_eq!(
            classify_expiry(now - Duration::from_secs(60), now, 14),
            CertExpiry::Expired
        );
        assert_eq!(
            classify_expiry(now + Duration::from_secs(3 * 86_400), now, 14),
            CertExpiry::Soon(3)
        );
        assert_eq!(
            classify_expiry(now + Duration::from_secs(60 * 86_400), now, 14),
            CertExpiry::Ok
        );
    }

    #[test]
    fn operator_cert_loads_without_self_signing() {
        let store = unique_dir("op");
        let (cert, key) = write_operator_pem(&store);
        // cert_dir is a DIFFERENT, empty dir — proving we don't touch it.
        let cert_dir = unique_dir("op-certdir");

        let mat = make_tls_acceptor(&cert_dir, Some(&cert), Some(&key)).expect("operator load");
        assert!(
            !mat.spki_der.is_empty(),
            "SPKI must be extracted for CredSSP"
        );
        assert!(!mat.cert_der.is_empty() && !mat.key_der.is_empty());
        // No self-signed material written into cert_dir.
        assert!(!cert_dir.join("cert.pem").exists());
        assert!(!cert_dir.join("key.pem").exists());

        fs::remove_dir_all(&store).ok();
        fs::remove_dir_all(&cert_dir).ok();
    }

    #[test]
    fn operator_missing_file_is_hard_error_not_self_sign() {
        let cert_dir = unique_dir("op-missing");
        let missing_cert = cert_dir.join("nope-cert.pem");
        let missing_key = cert_dir.join("nope-key.pem");
        let res = make_tls_acceptor(&cert_dir, Some(&missing_cert), Some(&missing_key));
        assert!(
            res.is_err(),
            "a missing operator cert must error, never self-sign"
        );
        // And it must NOT have generated a fallback self-signed cert.
        assert!(!cert_dir.join("cert.pem").exists());
        fs::remove_dir_all(&cert_dir).ok();
    }

    #[test]
    fn operator_cert_without_key_is_error() {
        let store = unique_dir("op-onearg");
        let (cert, _key) = write_operator_pem(&store);
        let cert_dir = unique_dir("op-onearg-certdir");
        // Only --cert, no --key: make_tls_acceptor rejects (defensive; async_main
        // also validates before calling).
        let res = make_tls_acceptor(&cert_dir, Some(&cert), None);
        assert!(res.is_err());
        fs::remove_dir_all(&store).ok();
        fs::remove_dir_all(&cert_dir).ok();
    }

    #[test]
    fn default_path_still_generates_self_signed() {
        let cert_dir = unique_dir("default");
        let mat = make_tls_acceptor(&cert_dir, None, None).expect("default self-signed");
        assert!(!mat.spki_der.is_empty());
        // Default path persists into cert_dir for stable TOFU.
        assert!(cert_dir.join("cert.pem").exists());
        assert!(cert_dir.join("key.pem").exists());
        fs::remove_dir_all(&cert_dir).ok();
    }
}
