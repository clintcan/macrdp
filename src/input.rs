//! Input forwarding: RDP keyboard/mouse PDUs → macOS CGEvents.
//!
//! `ironrdp_server` hands us scancodes (PS/2 Set 1) and absolute mouse coords
//! in desktop-pixel space. We translate to macOS virtual keycodes and post via
//! `CGEventPost(kCGHIDEventTap)`. Non-macOS targets get a logging stub.

use ironrdp_server::{KeyboardEvent, MouseEvent, RdpServerInputHandler};
#[cfg(not(target_os = "macos"))]
use tracing::trace;

/// Shared cell the vendored server fills with the connected client's Windows
/// keyboard-layout identifier (KLID); 0 = unknown / not announced. Read by the
/// input handler to auto-select a non-US layout when `--keyboard-layout` isn't
/// given. Mirrors the `display_suppressed` shared-flag pattern.
pub type SharedKeyboardLayout = std::sync::Arc<std::sync::atomic::AtomicU32>;

pub struct MacInputHandler {
    /// Live session desktop size, shared with `CaptureDisplay`. Read per
    /// mouse event (not copied at construction) so coordinate scaling stays
    /// correct when the client-resolution auto-adopt resizes the session.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    desktop_size: crate::capture::SharedDesktopSize,
    /// Records mouse-button-down timestamps so the H.264 path can lower its
    /// keyframe threshold briefly after a click. `None` unless `--enable-h264`.
    click_signal: Option<crate::capture::ClickSignal>,
    #[cfg(target_os = "macos")]
    inner: macos::Inner,
}

impl MacInputHandler {
    /// `target_display_id` identifies the macOS display we're posting
    /// events into: `None` means the current primary panel; `Some(id)`
    /// is the `CGDirectDisplayID` we should re-query bounds for on
    /// every event. Re-querying matters because the target's global
    /// origin can move *after* this handler is constructed — e.g.
    /// `--detach-primary` disables the built-in panel mid-session,
    /// which shifts the virtual display to `(0, 0)`.
    pub fn new(
        desktop_size: crate::capture::SharedDesktopSize,
        target_display_id: Option<u32>,
        click_signal: Option<crate::capture::ClickSignal>,
        keyboard_layout: Option<String>,
        keyboard_layout_klid: Option<SharedKeyboardLayout>,
    ) -> anyhow::Result<Self> {
        #[cfg(target_os = "macos")]
        let inner = macos::Inner::new(
            target_display_id,
            keyboard_layout.as_deref(),
            keyboard_layout_klid,
        )?;
        #[cfg(not(target_os = "macos"))]
        {
            let _ = target_display_id;
            let _ = keyboard_layout;
            let _ = keyboard_layout_klid;
        }
        Ok(Self {
            desktop_size,
            click_signal,
            #[cfg(target_os = "macos")]
            inner,
        })
    }
}

impl RdpServerInputHandler for MacInputHandler {
    fn keyboard(&mut self, event: KeyboardEvent) {
        #[cfg(target_os = "macos")]
        self.inner.keyboard(event);
        #[cfg(not(target_os = "macos"))]
        trace!(?event, "keyboard event (stub)");
    }

    fn mouse(&mut self, event: MouseEvent) {
        // A button-down is the "user intent" signal the H.264 path uses to lower
        // its keyframe threshold for a moment (a click usually precedes a UI
        // change). Record before delegating, since `inner.mouse` takes `event`.
        if let Some(sig) = &self.click_signal {
            if matches!(
                event,
                MouseEvent::LeftPressed | MouseEvent::RightPressed | MouseEvent::MiddlePressed
            ) {
                sig.record_click();
            }
        }
        #[cfg(target_os = "macos")]
        {
            let (width, height) = self.desktop_size.get();
            let letterbox = self.desktop_size.letterbox();
            self.inner.mouse(event, width, height, letterbox);
        }
        #[cfg(not(target_os = "macos"))]
        trace!(?event, "mouse event (stub)");
    }
}

/// Map an RDP client coordinate (`x`,`y` in a `dw`×`dh` client frame) to a point
/// *offset* within the target Mac display (`sw`×`sh` points; caller adds the
/// display origin).
///
/// - `letterbox = false` (stretch/fill): the whole client frame maps linearly
///   onto the whole Mac screen.
/// - `letterbox = true`: the Mac occupies a centered, aspect-preserved sub-rect
///   of the client frame (matching capture's `preserves_aspect_ratio`), so we map
///   into that sub-rect; coords landing in the black bars clamp to the nearest
///   screen edge rather than off-screen.
///
/// Pure (no platform deps) so it's unit-tested on every target.
fn map_client_to_display(
    x: u16,
    y: u16,
    dw: u16,
    dh: u16,
    sw: f64,
    sh: f64,
    letterbox: bool,
) -> (f64, f64) {
    let dwf = f64::from(dw.max(1));
    let dhf = f64::from(dh.max(1));
    if letterbox {
        let mac_aspect = sw / sh.max(1.0);
        let out_aspect = dwf / dhf;
        let (cw, ch) = if out_aspect > mac_aspect {
            (dhf * mac_aspect, dhf) // pillarbox: bars left/right
        } else {
            (dwf, dwf / mac_aspect) // letterbox: bars top/bottom
        };
        let off_x = (dwf - cw) / 2.0;
        let off_y = (dhf - ch) / 2.0;
        let fx = ((f64::from(x) - off_x) / cw).clamp(0.0, 1.0);
        let fy = ((f64::from(y) - off_y) / ch).clamp(0.0, 1.0);
        (fx * sw, fy * sh)
    } else {
        (f64::from(x) * sw / dwf, f64::from(y) * sh / dhf)
    }
}

/// macOS virtual keycodes whose `Ctrl+<key>` combo is remapped to `Cmd+<key>`
/// under --map-ctrl-to-cmd: C V X A Z S F N T W O P R G (copy / paste / cut /
/// select-all / undo / save / find / new / new-tab / close / open / print /
/// reload / find-next). Deliberately EXCLUDES Q (`Cmd+Q` quits — a nasty
/// surprise from `Ctrl+Q`) and all nav keys (Mac word-nav is Option+arrow, not
/// Cmd+arrow). Windows redo (`Ctrl+Y`) is intentionally not here — Mac redo is
/// `Cmd+Shift+Z`, reachable via `Ctrl+Shift+Z` through the Z mapping.
///
/// Pure (no platform deps) so it lives at file scope and is unit-tested on
/// every target; `mod macos` reaches it via `use super::is_remappable_shortcut`.
fn is_remappable_shortcut(vk: u16) -> bool {
    matches!(
        vk,
        0x08 // C
            | 0x09 // V
            | 0x07 // X
            | 0x00 // A
            | 0x06 // Z
            | 0x01 // S
            | 0x03 // F
            | 0x2D // N
            | 0x11 // T
            | 0x0D // W
            | 0x1F // O
            | 0x23 // P
            | 0x0F // R
            | 0x05 // G
    )
}

#[cfg(test)]
mod coord_tests {
    use super::{is_remappable_shortcut, map_client_to_display};

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 0.5
    }

    #[test]
    fn curated_keys_remap_and_q_does_not() {
        // Curated editing keys (C, V, X, A, Z, S) remap.
        for vk in [0x08u16, 0x09, 0x07, 0x00, 0x06, 0x01] {
            assert!(is_remappable_shortcut(vk), "vk {vk:#x} should remap");
        }
        // Q (0x0C) is deliberately excluded so Ctrl+Q can't become Cmd+Q.
        assert!(!is_remappable_shortcut(0x0C));
        // Arrows / nav keys are untouched (e.g. left arrow 0x7B).
        assert!(!is_remappable_shortcut(0x7B));
    }

    #[test]
    fn stretch_maps_full_frame() {
        // 16:9 client onto a 16:10 Mac, fill mode: corners map to corners.
        let (mx, my) = map_client_to_display(1920, 1080, 1920, 1080, 1512.0, 982.0, false);
        assert!(approx(mx, 1512.0) && approx(my, 982.0));
        let (mx, my) = map_client_to_display(960, 540, 1920, 1080, 1512.0, 982.0, false);
        assert!(approx(mx, 756.0) && approx(my, 491.0)); // center
    }

    #[test]
    fn pillarbox_when_client_wider_than_mac() {
        // Mac 1512x982 (~1.54) into 1920x1080 (~1.78, wider) → bars left/right.
        // content_w = 1080 * 1.54 = 1663.7, off_x = (1920-1663.7)/2 = 128.1.
        let sw = 1512.0;
        let sh = 982.0;
        // center stays center
        let (mx, my) = map_client_to_display(960, 540, 1920, 1080, sw, sh, true);
        assert!(approx(mx, sw / 2.0) && approx(my, sh / 2.0));
        // left bar (x=50 < off_x) clamps to the left edge
        let (mx, _) = map_client_to_display(50, 540, 1920, 1080, sw, sh, true);
        assert!(approx(mx, 0.0));
        // right bar clamps to the right edge
        let (mx, _) = map_client_to_display(1900, 540, 1920, 1080, sw, sh, true);
        assert!(approx(mx, sw));
        // top/bottom fill the height → no vertical clamp at extremes
        let (_, my) = map_client_to_display(960, 0, 1920, 1080, sw, sh, true);
        assert!(approx(my, 0.0));
        let (_, my) = map_client_to_display(960, 1080, 1920, 1080, sw, sh, true);
        assert!(approx(my, sh));
    }

    #[test]
    fn letterbox_when_client_taller_than_mac() {
        // Mac 1512x982 (~1.54) into 1280x1024 (1.25, taller) → bars top/bottom.
        let sw = 1512.0;
        let sh = 982.0;
        let (mx, my) = map_client_to_display(640, 512, 1280, 1024, sw, sh, true);
        assert!(approx(mx, sw / 2.0) && approx(my, sh / 2.0)); // center
        let (_, my) = map_client_to_display(640, 5, 1280, 1024, sw, sh, true);
        assert!(approx(my, 0.0)); // top bar clamps up
        let (mx, _) = map_client_to_display(0, 512, 1280, 1024, sw, sh, true);
        assert!(approx(mx, 0.0)); // full width → left edge maps to 0
    }
}

/// When true, Cmd+Tab un-minimizes the target app's window (sets `AXMinimized`
/// to false) instead of leaving it minimized in the Dock. Off by default, which
/// matches native macOS Cmd+Tab (it activates the app but doesn't restore a
/// minimized window). Set once at startup from `--unminimize-on-switch`.
static UNMINIMIZE_ON_SWITCH: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub fn set_unminimize_on_switch(on: bool) {
    UNMINIMIZE_ON_SWITCH.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// When true, Option+Tab (i.e. Alt+Tab forwarded from the client) is accepted as
/// an *additional* trigger for the same app-cycle as Cmd+Tab — for clients/configs
/// that pass Alt+Tab through to the session but gate Win+Tab (mstsc's "Apply
/// Windows key combinations"). Opt-in via `--alt-tab-switch`; off by default so
/// Option+Tab otherwise reaches remote apps as a normal key. Cmd+Tab is
/// unaffected and always works. Reuses the same MRU session, committing on
/// Option release instead of Cmd release.
static ALT_TAB_SWITCH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_alt_tab_switch(on: bool) {
    ALT_TAB_SWITCH.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// When true, drive the `macrdphud` overlay helper so the remote client sees a
/// visual app switcher during Cmd+Tab / Option+Tab. Opt-in via
/// `--app-switcher-hud`; off by default. The switch itself works the same either
/// way — this only adds the (best-effort) on-screen HUD.
static APP_SWITCHER_HUD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_app_switcher_hud(on: bool) {
    APP_SWITCHER_HUD.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// When true, rewrite a curated set of `Ctrl+<key>` editing shortcuts to
/// `Cmd+<key>` so Windows muscle memory (Ctrl+C/V/X/…) drives macOS copy/paste.
/// Opt-in via `--map-ctrl-to-cmd`; off by default (the default keeps Ctrl as Ctrl,
/// so the US majority + Ctrl-based shortcuts are unaffected). Suppressed when a
/// terminal app is frontmost (see `NO_REMAP_APPS`).
static MAP_CTRL_TO_CMD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_map_ctrl_to_cmd(on: bool) {
    MAP_CTRL_TO_CMD.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Bundle ids whose frontmost focus suppresses the Ctrl→Cmd remap, in ADDITION to
/// the built-in standalone-terminal list. The lever for embedded terminals that the
/// front-app check can't auto-detect (e.g. add `com.microsoft.VSCode`). Set once at
/// startup from `--no-remap-apps`.
static NO_REMAP_APPS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();

pub fn set_no_remap_apps(bundles: Vec<String>) {
    let _ = NO_REMAP_APPS.set(bundles);
}

/// Start the event-driven frontmost-app observer used by the Ctrl→Cmd remap's
/// terminal/app suppression. Call once at startup when `--map-ctrl-to-cmd` is on.
/// See `macos::init_focus_observer`.
#[cfg(target_os = "macos")]
pub fn init_focus_observer() {
    macos::init_focus_observer();
}

#[cfg(not(target_os = "macos"))]
pub fn init_focus_observer() {}

#[cfg(target_os = "macos")]
mod scancodes;

#[cfg(target_os = "macos")]
mod macos {
    use std::collections::HashSet;
    use std::process::Command;
    use std::time::{Duration, Instant};

    use super::is_remappable_shortcut;
    use super::scancodes::{is_numeric_pad_vk, scancode_to_cgkeycode};

    use anyhow::{anyhow, Result};
    use core_graphics::display::CGDisplay;
    use core_graphics::event::{
        CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGMouseButton, EventField,
        ScrollEventUnit,
    };
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
    use core_graphics::geometry::CGPoint;
    use ironrdp_pdu::input::fast_path::SynchronizeFlags;
    use ironrdp_server::{KeyboardEvent, MouseEvent};
    use objc2_app_kit::{NSApplicationActivationPolicy, NSRunningApplication, NSWorkspace};
    use tracing::{debug, trace, warn};

    // kVK_* constants we match on for symbolic-hotkey interception.
    const VK_TAB: u16 = 0x30;
    const VK_SPACE: u16 = 0x31;
    const VK_3: u16 = 0x14;
    const VK_4: u16 = 0x15;
    const VK_5: u16 = 0x17;
    const VK_GRAVE: u16 = 0x32;
    const VK_CAPS_LOCK: u16 = 0x39;
    const VK_G: u16 = 0x05; // kVK_ANSI_G — the on-demand "gather windows" chord

    // macOS virtual keycodes for the left/right halves of each modifier.
    // Used by ModifierState to track which physical key is held so we can
    // accurately re-derive both the masked CGEventFlag bits (Shift, Ctrl,
    // …) and the device-dependent NX_DEVICE{L,R}*KEYMASK bits a real
    // keyboard would put on a flagsChanged event.
    const VK_LSHIFT: u16 = 0x38;
    const VK_RSHIFT: u16 = 0x3C;
    const VK_LCTRL: u16 = 0x3B;
    const VK_RCTRL: u16 = 0x3E;
    const VK_LALT: u16 = 0x3A;
    const VK_RALT: u16 = 0x3D;
    const VK_LCMD: u16 = 0x37;
    const VK_RCMD: u16 = 0x36;

    // Device-dependent modifier bits — lifted from <IOKit/hidsystem/IOLLEvent.h>
    // (NX_DEVICE{L,R}{SHIFT,CTL,ALT,CMD}KEYMASK). They sit below the public
    // CGEventFlag bits (which start at 0x0100), so `from_bits_retain` keeps them
    // through CGEventFlags. Apps that distinguish left vs. right modifiers
    // (Karabiner, some games, accessibility tools) read these — a real
    // keyboard's flagsChanged event has them set; ours wouldn't without this.
    const NX_DEVICE_L_CTRL: u64 = 0x0001;
    const NX_DEVICE_L_SHIFT: u64 = 0x0002;
    const NX_DEVICE_R_SHIFT: u64 = 0x0004;
    const NX_DEVICE_L_CMD: u64 = 0x0008;
    const NX_DEVICE_R_CMD: u64 = 0x0010;
    const NX_DEVICE_L_ALT: u64 = 0x0020;
    const NX_DEVICE_R_ALT: u64 = 0x0040;
    const NX_DEVICE_R_CTRL: u64 = 0x2000;

    /// macOS's default `NSEvent.doubleClickInterval` is 0.5 s. We don't read
    /// the per-user setting (would need a CFPreferences call) — the default
    /// matches what most users have, and using less than the real threshold
    /// just means an aggressive double-click occasionally counts as two
    /// single clicks (the worse alternative is missing all double-clicks).
    const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(500);
    /// Pixels of cursor movement allowed between consecutive clicks for them
    /// to still count as a multi-click. macOS's slop is a few px; 5 is safe.
    const DOUBLE_CLICK_SLOP_PX: f64 = 5.0;

    #[derive(Clone, Copy)]
    struct ClickState {
        time: Instant,
        x: f64,
        y: f64,
        count: i64,
    }

    // CGEventSource wraps a thread-safe Core Foundation object; Apple documents
    // CF types as safe to send between threads. The crate doesn't impl Send
    // because the raw NonNull pointer isn't, but our usage is single-threaded
    // anyway (RdpServer serializes input callbacks).
    unsafe impl Send for Inner {}

    /// Per-side modifier state. We track left/right halves of each modifier
    /// separately for two reasons:
    ///   1) When the user holds *both* left and right Shift and releases
    ///      one, the masked `CGEventFlagShift` bit must stay set. A single
    ///      `flags |= / -= CGEventFlagShift` toggle can't represent that —
    ///      releasing one half wrongly drops the bit while the other half
    ///      is still down.
    ///   2) Real keyboards include device-dependent left/right bits on
    ///      `flagsChanged` events (`NX_DEVICE{L,R}*KEYMASK`). Apps that
    ///      key off left-only vs. right-only modifiers (Karabiner,
    ///      remappers, some games) read those. Without per-side tracking
    ///      we can't produce them.
    ///
    /// `caps_lock` is a *toggle*, not a held-down bool: pressing the key
    /// flips it; the release is a no-op. That matches how real Mac
    /// keyboards report Caps Lock and what Cocoa apps assume when they
    /// check `CGEventFlagAlphaShift`.
    #[derive(Default, Clone, Copy)]
    struct ModifierState {
        l_shift: bool,
        r_shift: bool,
        l_ctrl: bool,
        r_ctrl: bool,
        l_alt: bool,
        r_alt: bool,
        l_cmd: bool,
        r_cmd: bool,
        caps_lock: bool,
    }

    impl ModifierState {
        /// Bitfield to put on every CGEvent. Combines the public masked
        /// bits (the ones apps query via `[NSEvent modifierFlags] &
        /// NSEventModifierFlagShift` and friends) with the
        /// device-dependent NX_DEVICE* left/right bits below 0x100.
        fn cg_flags(&self) -> CGEventFlags {
            let mut bits = 0u64;
            if self.l_shift || self.r_shift {
                bits |= CGEventFlags::CGEventFlagShift.bits();
            }
            if self.l_ctrl || self.r_ctrl {
                bits |= CGEventFlags::CGEventFlagControl.bits();
            }
            if self.l_alt || self.r_alt {
                bits |= CGEventFlags::CGEventFlagAlternate.bits();
            }
            if self.l_cmd || self.r_cmd {
                bits |= CGEventFlags::CGEventFlagCommand.bits();
            }
            if self.caps_lock {
                bits |= CGEventFlags::CGEventFlagAlphaShift.bits();
            }
            if self.l_shift {
                bits |= NX_DEVICE_L_SHIFT;
            }
            if self.r_shift {
                bits |= NX_DEVICE_R_SHIFT;
            }
            if self.l_ctrl {
                bits |= NX_DEVICE_L_CTRL;
            }
            if self.r_ctrl {
                bits |= NX_DEVICE_R_CTRL;
            }
            if self.l_alt {
                bits |= NX_DEVICE_L_ALT;
            }
            if self.r_alt {
                bits |= NX_DEVICE_R_ALT;
            }
            if self.l_cmd {
                bits |= NX_DEVICE_L_CMD;
            }
            if self.r_cmd {
                bits |= NX_DEVICE_R_CMD;
            }
            CGEventFlags::from_bits_retain(bits)
        }

        fn has_shift(&self) -> bool {
            self.l_shift || self.r_shift
        }
        fn has_ctrl(&self) -> bool {
            self.l_ctrl || self.r_ctrl
        }
        fn has_alt(&self) -> bool {
            self.l_alt || self.r_alt
        }
        fn has_cmd(&self) -> bool {
            self.l_cmd || self.r_cmd
        }

        /// True if `vk` is a known modifier (incl. Caps Lock). The caller
        /// uses this to branch into the FlagsChanged path.
        fn is_modifier_vk(vk: u16) -> bool {
            matches!(
                vk,
                VK_LSHIFT
                    | VK_RSHIFT
                    | VK_LCTRL
                    | VK_RCTRL
                    | VK_LALT
                    | VK_RALT
                    | VK_LCMD
                    | VK_RCMD
                    | VK_CAPS_LOCK
            )
        }

        /// Update state for a press/release of a modifier or Caps Lock.
        /// Returns true if anything changed (callers skip emitting a
        /// FlagsChanged event when nothing did — e.g. a Caps Lock release).
        fn apply(&mut self, vk: u16, down: bool) -> bool {
            // Caps Lock is a toggle: flip on press, ignore release. This is
            // what real Mac keyboards do — the up event carries no new
            // information, and emitting one would unset AlphaShift mid-press.
            if vk == VK_CAPS_LOCK {
                if down {
                    self.caps_lock = !self.caps_lock;
                    return true;
                }
                return false;
            }
            let slot = match vk {
                VK_LSHIFT => &mut self.l_shift,
                VK_RSHIFT => &mut self.r_shift,
                VK_LCTRL => &mut self.l_ctrl,
                VK_RCTRL => &mut self.r_ctrl,
                VK_LALT => &mut self.l_alt,
                VK_RALT => &mut self.r_alt,
                VK_LCMD => &mut self.l_cmd,
                VK_RCMD => &mut self.r_cmd,
                _ => return false,
            };
            if *slot == down {
                return false; // idempotent — auto-repeat of a held modifier
            }
            *slot = down;
            true
        }
    }

    pub struct Inner {
        source: CGEventSource,
        // Secondary source used *only* to mirror modifier FlagsChanged
        // events. See `Inner::new` for why one source isn't enough.
        source_hid: CGEventSource,
        last_x: f64,
        last_y: f64,
        left_down: bool,
        right_down: bool,
        middle_down: bool,
        mods: ModifierState,
        // `None` → use CGDisplay::main() bounds; `Some(id)` → look up
        // that specific display. Re-queried (through a short TTL cache —
        // see `target_bounds`) so a mid-session bounds change (e.g.
        // --detach-primary disabling the built-in mid-flight) doesn't
        // strand events on stale coords.
        target_display_id: Option<u32>,
        // TTL cache for `target_bounds`: the CoreGraphics bounds query sits
        // on the busiest input path (mouse-move fires hundreds of times/s
        // during a drag) while display geometry changes only on a rare
        // reconfiguration. The cache sheds ~99% of the queries; worst case a
        // reconfig mis-maps moves for < the TTL before self-correcting.
        cached_bounds: Option<(f64, f64, f64, f64)>,
        bounds_cached_at: Instant,
        // Per-button click history so a quick second press at (roughly) the
        // same spot becomes click_count=2 and Finder recognises a double-
        // click. Without this every press has click_count=1 implicitly and
        // double-click actions never fire.
        click_left: Option<ClickState>,
        click_right: Option<ClickState>,
        click_middle: Option<ClickState>,
        // Virtual keycodes whose key-down we intercepted as a symbolic
        // hotkey (Cmd+Tab, Cmd+Space, screencapture combos). The matching
        // key-up gets swallowed too so the focused app doesn't see a bare
        // release with no preceding press.
        consumed_keys: HashSet<u16>,
        // Virtual keycodes currently held that we remapped Ctrl→Cmd on the way
        // down (--map-ctrl-to-cmd). The matching key-up is posted with the same
        // Cmd swap + restores the real (Ctrl) modifier state.
        remapped_keys: HashSet<u16>,
        // Optional non-US keyboard layout. When set, ordinary typing keys are
        // translated to characters against this layout and posted as Unicode
        // strings instead of positional keycodes. `None` → keycode path only.
        layout: Option<crate::keyboard_layout::KeyboardLayout>,
        // Auto-detect plumbing: when no explicit --keyboard-layout was given,
        // `auto_layout` is true and we (re)resolve `layout` from the client's
        // announced KLID published in `klid_handle`, re-checking when it changes
        // (e.g. a reconnect from a differently-configured client).
        klid_handle: Option<super::SharedKeyboardLayout>,
        last_klid: u32,
        auto_layout: bool,
    }

    impl Inner {
        pub fn new(
            target_display_id: Option<u32>,
            keyboard_layout: Option<&str>,
            klid_handle: Option<super::SharedKeyboardLayout>,
        ) -> Result<Self> {
            // Two sources because macOS has two independent modifier-state
            // machines and different consumers read from different ones:
            //
            //   CombinedSessionState backs `[NSEvent modifierFlags]` — the
            //   query Cocoa apps use to decide "is Cmd held right now?" for
            //   app-level shortcuts (Cmd+C, Cmd+Q, Cmd+~).
            //
            //   HIDSystemState backs the HID-level modifier view that the
            //   WindowServer's *symbolic hotkey* dispatcher (Dock, Spotlight,
            //   screenshot, Mission Control, Show Desktop) checks before
            //   firing. Cmd+Tab is the canonical case.
            //
            // A flagsChanged event posted from one source does NOT update the
            // other. So we maintain both: the session source is the canonical
            // one used for every event, and `source_hid` exists solely to
            // mirror modifier flagsChanged so symbolic hotkeys also see Cmd
            // as held. Without the mirror, Cmd+Tab is a no-op; with it, both
            // classes of shortcut work.
            let source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState)
                .map_err(|_| anyhow!("CGEventSource::new(CombinedSessionState) failed"))?;
            let source_hid = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
                .map_err(|_| anyhow!("CGEventSource::new(HIDSystemState) failed"))?;
            // Start the workspace-frontmost poller so the global MRU
            // tracks ALL focus changes (Dock clicks, app launches, etc.),
            // not just our Cmd+Tab commits. Idempotent — only one thread
            // is ever spawned across handler reconstructions.
            start_workspace_mru_poller();
            // Layout selection precedence:
            //   --keyboard-layout <spec>  → force that layout (no auto-detect)
            //   --keyboard-layout none/off → force positional keycodes
            //   (unset) + a KLID handle    → auto-detect from the client
            let explicit = keyboard_layout.map(str::trim);
            let disabled = matches!(explicit, Some("none") | Some("off") | Some(""));
            let (layout, auto_layout) = if disabled {
                (None, false)
            } else if let Some(spec) = explicit {
                (crate::keyboard_layout::KeyboardLayout::resolve(spec), false)
            } else {
                (None, klid_handle.is_some())
            };
            if let Some(l) = &layout {
                tracing::info!(layout = %l.label(), "non-US keyboard layout translation active");
            } else if auto_layout {
                tracing::info!("keyboard layout will auto-detect from the connecting client");
            }
            Ok(Self {
                source,
                source_hid,
                last_x: 0.0,
                last_y: 0.0,
                left_down: false,
                right_down: false,
                middle_down: false,
                mods: ModifierState::default(),
                target_display_id,
                cached_bounds: None,
                bounds_cached_at: Instant::now(),
                click_left: None,
                click_right: None,
                click_middle: None,
                consumed_keys: HashSet::new(),
                remapped_keys: HashSet::new(),
                layout,
                klid_handle,
                last_klid: 0,
                auto_layout,
            })
        }

        /// How long a `target_bounds` result may be served from cache. Long
        /// enough to shed the per-mouse-move CoreGraphics query (hundreds/s
        /// during a drag), short enough that a display reconfiguration (rare;
        /// e.g. --detach-primary engaging at connect) mis-maps coords for at
        /// most a quarter second before self-correcting.
        const BOUNDS_CACHE_TTL: Duration = Duration::from_millis(250);

        /// Current global-coord bounds of the target display. Re-queried
        /// through a short TTL cache so a layout change since this handler
        /// was built (vd moving to (0,0) when --detach-primary disables the
        /// built-in panel) doesn't leave us posting to a stale rectangle,
        /// without paying a CG display lookup on every mouse move.
        fn target_bounds(&mut self) -> (f64, f64, f64, f64) {
            if let Some(b) = self.cached_bounds {
                if self.bounds_cached_at.elapsed() < Self::BOUNDS_CACHE_TTL {
                    return b;
                }
            }
            let d = match self.target_display_id {
                Some(id) => CGDisplay::new(id),
                None => CGDisplay::main(),
            };
            let b = d.bounds();
            let bounds = (b.origin.x, b.origin.y, b.size.width, b.size.height);
            self.cached_bounds = Some(bounds);
            self.bounds_cached_at = Instant::now();
            bounds
        }

        pub fn keyboard(&mut self, event: KeyboardEvent) {
            mark_input_activity();
            match event {
                KeyboardEvent::Pressed { code, extended } => self.key(code, extended, true),
                KeyboardEvent::Released { code, extended } => self.key(code, extended, false),
                KeyboardEvent::UnicodePressed(c) => self.unicode(c, true),
                KeyboardEvent::UnicodeReleased(c) => self.unicode(c, false),
                KeyboardEvent::Synchronize(sync) => self.synchronize(sync),
            }
        }

        /// MS-RDPBCGR Synchronize event — the client tells us its current
        /// lock-key state (Caps Lock, Num Lock, etc.). Only Caps Lock has a
        /// CGEventFlag equivalent on macOS, so we reconcile that one: if our
        /// internal toggle disagrees with the client, flip ours and emit a
        /// FlagsChanged so any focused app's AlphaShift query is correct
        /// from the first keystroke. (Num/Scroll/Kana Lock are kernel-side
        /// concepts on Windows that macOS doesn't model.)
        fn synchronize(&mut self, sync: SynchronizeFlags) {
            let want_caps = sync.contains(SynchronizeFlags::CAPS_LOCK);
            if want_caps != self.mods.caps_lock {
                debug!(
                    have = self.mods.caps_lock,
                    want = want_caps,
                    "syncing CapsLock to client state"
                );
                self.mods.caps_lock = want_caps;
                self.post_flags_changed(VK_CAPS_LOCK);
            }
        }

        /// When auto-detecting (no explicit `--keyboard-layout`), (re)resolve
        /// the translation layout from the client's announced KLID. Cheap when
        /// unchanged (one atomic load); only re-resolves on a new KLID, so a
        /// reconnect from a differently-configured client is picked up. US
        /// English (0x0409) and unknown (0) keep the positional keycode path.
        fn refresh_auto_layout(&mut self) {
            if !self.auto_layout {
                return;
            }
            let Some(handle) = &self.klid_handle else {
                return;
            };
            let klid = handle.load(std::sync::atomic::Ordering::Relaxed);
            if klid == self.last_klid {
                return;
            }
            self.last_klid = klid;
            self.layout = if klid != 0 && (klid & 0xFFFF) != 0x0409 {
                let spec = format!("0x{:04x}", klid & 0xFFFF);
                let resolved = crate::keyboard_layout::KeyboardLayout::resolve(&spec);
                match &resolved {
                    Some(l) => tracing::info!(
                        klid = format!("0x{klid:04X}"),
                        layout = %l.label(),
                        "auto-selected the client's keyboard layout"
                    ),
                    None => tracing::warn!(
                        klid = format!("0x{klid:04X}"),
                        "client announced a keyboard layout with no installed macOS match; using positional keycodes"
                    ),
                }
                resolved
            } else {
                None
            };
        }

        fn key(&mut self, scancode: u8, extended: bool, down: bool) {
            let Some(vk) = scancode_to_cgkeycode(scancode, extended) else {
                tracing::debug!(scancode, extended, down, "unmapped scancode");
                return;
            };
            self.refresh_auto_layout();

            // Modifier path: update per-side state and emit FlagsChanged
            // mirrored to both sources. We do this BEFORE any of the
            // symbolic-hotkey logic so the held-modifier check in
            // try_symbolic_hotkey sees the just-pressed modifier.
            if ModifierState::is_modifier_vk(vk) {
                let changed = self.mods.apply(vk, down);
                tracing::debug!(
                    scancode = format!("0x{scancode:02X}"),
                    extended,
                    down,
                    vk = format!("0x{vk:02X}"),
                    is_modifier = true,
                    flags = format!("0x{:08X}", self.mods.cg_flags().bits()),
                    changed,
                    "key post (modifier)"
                );
                if changed {
                    self.post_flags_changed(vk);
                }
                // Commit any active Cmd+Tab session the moment both
                // Cmd halves are released. Matches native macOS: the
                // app the cursor landed on becomes MRU front only on
                // release, not on each intermediate Tab press.
                //
                // Also bump CMD_RELEASE_GENERATION so the next Cmd+Tab
                // can detect this release happened — a session whose
                // stored generation differs from the current value
                // started in a *previous* Cmd hold and shouldn't be
                // resumed. This replaces the old time-based grace,
                // which falsely split sessions whenever the user
                // paused mid-hold longer than 2 s.
                let cmd_released = matches!(vk, VK_LCMD | VK_RCMD)
                    && !down
                    && !self.mods.l_cmd
                    && !self.mods.r_cmd;
                // With --alt-tab-switch, an Opt+Tab session commits on Option
                // release, mirroring the Cmd path (same release-generation
                // counter — you can't hold both switchers at once).
                let opt_released = super::ALT_TAB_SWITCH.load(std::sync::atomic::Ordering::Relaxed)
                    && matches!(vk, VK_LALT | VK_RALT)
                    && !down
                    && !self.mods.l_alt
                    && !self.mods.r_alt;
                if cmd_released || opt_released {
                    CMD_RELEASE_GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    commit_cycle_session();
                }
                return;
            }

            tracing::debug!(
                scancode = format!("0x{scancode:02X}"),
                extended,
                down,
                vk = format!("0x{vk:02X}"),
                is_modifier = false,
                flags = format!("0x{:08X}", self.mods.cg_flags().bits()),
                "key post"
            );

            // Symbolic-hotkey interception: WindowServer's internal hotkey
            // dispatcher (which fires Cmd+Tab, Cmd+Space, Cmd+Shift+3/4/5,
            // Mission Control, etc.) only triggers on kernel-injected HID
            // events — user-space CGEventPost cannot wake it, regardless of
            // source state or tap location. For the common combos we
            // re-implement the action in user space and swallow the
            // keystroke. Triggers on key-down; the matching key-up is
            // tracked in `consumed_keys` so the bare key-up doesn't reach
            // the focused app either.
            if down && self.try_symbolic_hotkey(vk) {
                self.consumed_keys.insert(vk);
                return;
            }
            if !down && self.consumed_keys.remove(&vk) {
                return;
            }

            // Ctrl→Cmd remap (--map-ctrl-to-cmd): rewrite a curated set of editing
            // shortcuts so Windows muscle memory (Ctrl+C/V/X/…) drives macOS
            // copy/paste. The key-down swaps Ctrl→Cmd (with matching session
            // FlagsChanged) so the app sees a clean Cmd+key; the matching key-up
            // is posted the same way and restores the held-Ctrl state. Suppressed
            // when a terminal / excluded app is frontmost so Ctrl+C stays SIGINT.
            // Checked before the layout branch so a remapped key-up isn't swallowed
            // there.
            if !down && self.remapped_keys.remove(&vk) {
                self.post_ctrl_as_cmd(vk, false);
                return;
            }
            if down
                && super::MAP_CTRL_TO_CMD.load(std::sync::atomic::Ordering::Relaxed)
                && self.mods.has_ctrl()
                && !self.mods.has_cmd()
                && !self.mods.has_alt()
                && is_remappable_shortcut(vk)
                && !frontmost_is_excluded()
            {
                self.post_ctrl_as_cmd(vk, true);
                self.remapped_keys.insert(vk);
                return;
            }

            // Non-US layout translation: for ordinary typing keys with no
            // Cmd/Ctrl held, produce the character this key yields in the
            // configured layout and post it as a Unicode string, leaving the
            // Mac's own input source untouched. Cmd/Ctrl combos fall through to
            // the keycode path below so shortcuts still work. Once a layout is
            // active these keys are fully owned here — the matching key-up is
            // swallowed too (Unicode string events have no release side, like
            // the RDP Unicode-keyboard path).
            if self.layout.is_some()
                && !self.mods.has_cmd()
                && !self.mods.has_ctrl()
                && crate::keyboard_layout::is_translatable_keycode(vk)
            {
                if down {
                    let shift = self.mods.has_shift();
                    let option = self.mods.has_alt();
                    let caps = self.mods.caps_lock;
                    if let Some(text) = self
                        .layout
                        .as_mut()
                        .and_then(|l| l.translate(vk, shift, option, caps))
                    {
                        // Empty string = a pending dead key; swallow it and let
                        // the composed character arrive on the next keystroke.
                        if !text.is_empty() {
                            let utf16: Vec<u16> = text.encode_utf16().collect();
                            if let Ok(ev) =
                                CGEvent::new_keyboard_event(self.source.clone(), 0, true)
                            {
                                ev.set_string_from_utf16_unchecked(&utf16);
                                ev.post(CGEventTapLocation::HID);
                            }
                        }
                    }
                }
                return;
            }

            let Ok(ev) = CGEvent::new_keyboard_event(self.source.clone(), vk, down) else {
                warn!(vk, down, "CGEvent::new_keyboard_event failed");
                return;
            };
            let mut flags = self.mods.cg_flags();
            // macOS adds CGEventFlagNumericPad on events from the numeric
            // keypad. Some apps (Finder for arrow navigation, games) key off
            // it. The keypad vk range is in the `scancodes` module.
            if is_numeric_pad_vk(vk) {
                flags |= CGEventFlags::CGEventFlagNumericPad;
            }
            ev.set_flags(flags);
            ev.post(CGEventTapLocation::HID);
        }

        /// Emit a FlagsChanged event for `vk` from both sources. We send
        /// the same event twice so two different parts of macOS see the
        /// modifier as held:
        ///   - CombinedSessionState backs `[NSEvent modifierFlags]` —
        ///     what Cocoa apps query for Cmd+C / Cmd+Q / Cmd+~.
        ///   - HIDSystemState backs the modifier view that WindowServer's
        ///     symbolic-hotkey dispatcher (Dock, Spotlight, screencapture,
        ///     Mission Control, Show Desktop) checks before firing.
        ///
        /// A FlagsChanged posted from one source does NOT update the other,
        /// so without the mirror you get either app shortcuts or symbolic
        /// hotkeys but not both.
        fn post_flags_changed(&self, vk: u16) {
            let flags = self.mods.cg_flags();
            for source in [&self.source, &self.source_hid] {
                // `down` parameter is ignored for FlagsChanged: macOS
                // derives press-vs-release purely from the diff between
                // the prior flags state and the new flags carried on the
                // event. We pass `true` for shape only.
                let Ok(ev) = CGEvent::new_keyboard_event(source.clone(), vk, true) else {
                    warn!(vk, "CGEvent::new_keyboard_event failed (modifier)");
                    continue;
                };
                ev.set_flags(flags);
                ev.set_type(CGEventType::FlagsChanged);
                ev.post(CGEventTapLocation::HID);
            }
        }

        /// Post a curated `Ctrl+<vk>` shortcut as `Cmd+<vk>` (--map-ctrl-to-cmd).
        /// On **down**: present Cmd-held (not Ctrl) to both modifier views via a
        /// FlagsChanged carrying the swapped flags, then post the key-down with
        /// those flags — so the focused app sees a clean Cmd+key. On **up**: post
        /// the key-up with the swapped flags, then restore the real (Ctrl) state
        /// via `post_flags_changed` (the user is still physically holding Ctrl, so
        /// `self.mods` already reflects it). Shift/Caps in `self.mods` carry
        /// through unchanged, so `Ctrl+Shift+Z` → `Cmd+Shift+Z` (redo) works.
        fn post_ctrl_as_cmd(&self, vk: u16, down: bool) {
            // Swap Control→Command in both the public CGEventFlag bits and the
            // device-dependent NX bits, leaving every other modifier intact.
            let mut bits = self.mods.cg_flags().bits();
            bits &= !CGEventFlags::CGEventFlagControl.bits();
            bits &= !(NX_DEVICE_L_CTRL | NX_DEVICE_R_CTRL);
            bits |= CGEventFlags::CGEventFlagCommand.bits();
            bits |= NX_DEVICE_L_CMD;
            let swapped = CGEventFlags::from_bits_retain(bits);

            if down {
                for source in [&self.source, &self.source_hid] {
                    let Ok(ev) = CGEvent::new_keyboard_event(source.clone(), VK_LCMD, true) else {
                        warn!("CGEvent::new_keyboard_event failed (ctrl→cmd flags)");
                        continue;
                    };
                    ev.set_flags(swapped);
                    ev.set_type(CGEventType::FlagsChanged);
                    ev.post(CGEventTapLocation::HID);
                }
            }
            if let Ok(ev) = CGEvent::new_keyboard_event(self.source.clone(), vk, down) {
                ev.set_flags(swapped);
                ev.post(CGEventTapLocation::HID);
            } else {
                warn!(vk, "CGEvent::new_keyboard_event failed (ctrl→cmd key)");
            }
            if !down {
                // Restore the real modifier state (Ctrl still held).
                self.post_flags_changed(VK_LCMD);
            }
        }

        /// Match the current modifier state + non-modifier vk against the
        /// set of symbolic hotkeys we re-implement. Returns true if we
        /// handled it (and the caller should suppress the keystroke).
        fn try_symbolic_hotkey(&self, vk: u16) -> bool {
            let f = self.mods.cg_flags();
            let cmd = f.contains(CGEventFlags::CGEventFlagCommand);
            let shift = f.contains(CGEventFlags::CGEventFlagShift);
            let ctrl = f.contains(CGEventFlags::CGEventFlagControl);
            let opt = f.contains(CGEventFlags::CGEventFlagAlternate);

            // Ctrl+Option+G: on-demand "gather windows" — sweep any window stranded
            // off the session display (e.g. an app opened on the now-blanked
            // physical panel under --capture-primary) back onto the display the RDP
            // client sees. From a Windows client this is Ctrl+Alt+G — deliberately
            // Win-key-free so mstsc forwards it, and it's not a system shortcut on
            // either OS. Only acts when there's a virtual display to gather onto;
            // runs off-thread so input never stalls.
            if ctrl && opt && !cmd && vk == VK_G {
                if let Some(id) = self.target_display_id {
                    std::thread::spawn(move || {
                        let moved = gather_windows_onto_display(id);
                        tracing::info!(
                            moved,
                            display_id = id,
                            "on-demand gather-windows hotkey (Ctrl+Option+G)"
                        );
                    });
                    return true;
                }
                // No virtual display — nothing to gather onto; let the key through.
                return false;
            }

            // Opt+Tab / Opt+Shift+Tab as an alternative app-cycle trigger
            // (opt-in via --alt-tab-switch). Only plain Option (no Cmd/Ctrl) so
            // it doesn't shadow Option-as-AltGr typing or Cmd shortcuts; the
            // session commits on Option release (see the modifier-release block).
            if super::ALT_TAB_SWITCH.load(std::sync::atomic::Ordering::Relaxed)
                && opt
                && !cmd
                && !ctrl
                && vk == VK_TAB
            {
                cycle_apps(shift);
                return true;
            }

            if !cmd {
                return false;
            }
            // Only Cmd / Cmd+Shift are interesting here — any other extra
            // modifier means the combo isn't one of the symbolic hotkeys
            // WindowServer would have caught.
            if ctrl || opt {
                return false;
            }

            match (shift, vk) {
                (false, VK_TAB) => {
                    cycle_apps(false);
                    true
                }
                (true, VK_TAB) => {
                    cycle_apps(true);
                    true
                }
                (false, VK_SPACE) => {
                    invoke_spotlight();
                    true
                }
                (true, VK_3) => {
                    screencapture(&["-x"]);
                    true
                }
                (true, VK_4) => {
                    screencapture(&["-i"]);
                    true
                }
                (true, VK_5) => {
                    open_screenshot_app();
                    true
                }
                // Cmd+` / Cmd+Shift+` — cycle windows of the currently
                // frontmost app, native macOS semantics. Lets the user
                // explicitly step through windows within (e.g.) VSCode
                // without affecting the inter-app cycle. Whether the
                // RDP client forwards the backtick scancode at all
                // depends on its key-passthrough setting; mstsc treats
                // Win+` as a local "switch input source" combo by
                // default in some Windows versions, so it may need to
                // be re-bound on the client side to reach us.
                (false, VK_GRAVE) => {
                    ax_cycle_windows_of_front(false);
                    true
                }
                (true, VK_GRAVE) => {
                    ax_cycle_windows_of_front(true);
                    true
                }
                _ => false,
            }
        }

        fn unicode(&self, c: u16, down: bool) {
            // For unicode, send a "null" keycode and set the string. Only fire
            // on key-down — Mac doesn't have a release-side for typed text.
            if !down {
                return;
            }
            let Ok(ev) = CGEvent::new_keyboard_event(self.source.clone(), 0, true) else {
                warn!("unicode CGEvent create failed");
                return;
            };
            ev.set_string_from_utf16_unchecked(&[c]);
            ev.post(CGEventTapLocation::HID);
        }

        pub fn mouse(
            &mut self,
            event: MouseEvent,
            desktop_w: u16,
            desktop_h: u16,
            letterbox: bool,
        ) {
            mark_input_activity();
            match event {
                MouseEvent::Move { x, y } => self.move_to(x, y, desktop_w, desktop_h, letterbox),
                MouseEvent::LeftPressed => self.button(CGMouseButton::Left, true),
                MouseEvent::LeftReleased => self.button(CGMouseButton::Left, false),
                MouseEvent::RightPressed => self.button(CGMouseButton::Right, true),
                MouseEvent::RightReleased => self.button(CGMouseButton::Right, false),
                MouseEvent::MiddlePressed => self.button(CGMouseButton::Center, true),
                MouseEvent::MiddleReleased => self.button(CGMouseButton::Center, false),
                MouseEvent::VerticalScroll { value } => self.scroll(i32::from(value), 0),
                MouseEvent::Scroll { x, y } => self.scroll(y, x),
                MouseEvent::Button4Pressed
                | MouseEvent::Button4Released
                | MouseEvent::Button5Pressed
                | MouseEvent::Button5Released => {
                    trace!(?event, "extra mouse buttons not implemented");
                }
                MouseEvent::RelMove { .. } => {
                    trace!(?event, "relative move not implemented");
                }
            }
        }

        fn move_to(&mut self, x: u16, y: u16, desktop_w: u16, desktop_h: u16, letterbox: bool) {
            // Scale desktop coords → screen points, then translate into the
            // target display's slot in the global coord space. The origin
            // offset is what makes CGEventPost route events to a non-primary
            // display (virtual or external) — the WindowServer dispatches by
            // which display contains the global coord.
            let (ox, oy, sw, sh) = self.target_bounds();
            let (mx, my) =
                super::map_client_to_display(x, y, desktop_w, desktop_h, sw, sh, letterbox);
            let sx = ox + mx;
            let sy = oy + my;
            self.last_x = sx;
            self.last_y = sy;

            let etype = if self.left_down {
                CGEventType::LeftMouseDragged
            } else if self.right_down {
                CGEventType::RightMouseDragged
            } else if self.middle_down {
                CGEventType::OtherMouseDragged
            } else {
                CGEventType::MouseMoved
            };
            let button = if self.middle_down {
                CGMouseButton::Center
            } else if self.right_down {
                CGMouseButton::Right
            } else {
                CGMouseButton::Left
            };
            let Ok(ev) =
                CGEvent::new_mouse_event(self.source.clone(), etype, CGPoint::new(sx, sy), button)
            else {
                return;
            };
            ev.post(CGEventTapLocation::HID);
        }

        fn button(&mut self, button: CGMouseButton, down: bool) {
            let etype = match (button, down) {
                (CGMouseButton::Left, true) => CGEventType::LeftMouseDown,
                (CGMouseButton::Left, false) => CGEventType::LeftMouseUp,
                (CGMouseButton::Right, true) => CGEventType::RightMouseDown,
                (CGMouseButton::Right, false) => CGEventType::RightMouseUp,
                (CGMouseButton::Center, true) => CGEventType::OtherMouseDown,
                (CGMouseButton::Center, false) => CGEventType::OtherMouseUp,
            };
            match button {
                CGMouseButton::Left => self.left_down = down,
                CGMouseButton::Right => self.right_down = down,
                CGMouseButton::Center => self.middle_down = down,
            }

            // Compute the click count: increment on a down event close in
            // time + space to the previous click; reset to 1 otherwise. The
            // matching up event reuses the count from the most recent down
            // so Finder sees a paired {down, up} with identical click_state.
            let state_slot = match button {
                CGMouseButton::Left => &mut self.click_left,
                CGMouseButton::Right => &mut self.click_right,
                CGMouseButton::Center => &mut self.click_middle,
            };
            let click_count = if down {
                let now = Instant::now();
                let count = match *state_slot {
                    Some(prev)
                        if now.duration_since(prev.time) <= DOUBLE_CLICK_INTERVAL
                            && (self.last_x - prev.x).abs() <= DOUBLE_CLICK_SLOP_PX
                            && (self.last_y - prev.y).abs() <= DOUBLE_CLICK_SLOP_PX =>
                    {
                        prev.count + 1
                    }
                    _ => 1,
                };
                *state_slot = Some(ClickState {
                    time: now,
                    x: self.last_x,
                    y: self.last_y,
                    count,
                });
                count
            } else {
                state_slot.map(|s| s.count).unwrap_or(1)
            };

            let Ok(ev) = CGEvent::new_mouse_event(
                self.source.clone(),
                etype,
                CGPoint::new(self.last_x, self.last_y),
                button,
            ) else {
                warn!("CGEvent mouse button create failed");
                return;
            };
            ev.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, click_count);
            ev.post(CGEventTapLocation::HID);

            // On a button-down, record which app's window is under the cursor so
            // the Ctrl→Cmd suppression knows the real focus target — the only way
            // to catch an Electron app (VSCode) clicked into in a headless session,
            // which takes key focus without activating. Gated to when the remap is
            // on (the only consumer); the hit-test runs off-thread, best-effort.
            if down && super::MAP_CTRL_TO_CMD.load(std::sync::atomic::Ordering::Relaxed) {
                update_focus_from_click(self.last_x, self.last_y);
            }
        }

        fn scroll(&self, vertical: i32, horizontal: i32) {
            // RDP wheel rotation units are 120 per "tick"; macOS wheel deltas
            // are unit-less but ~1-3 per tick feels normal. Divide by 40.
            let v = (vertical / 40).clamp(-10, 10);
            let h = (horizontal / 40).clamp(-10, 10);
            let Ok(ev) =
                CGEvent::new_scroll_event(self.source.clone(), ScrollEventUnit::LINE, 2, v, h, 0)
            else {
                return;
            };
            ev.post(CGEventTapLocation::HID);
        }
    }

    /// Per-bundle "most recently in front" PID. Used during dedup to
    /// pick the right instance when multiple processes share a bundle
    /// ID (two VSCode windows opened as separate projects, two Firefox
    /// profiles, etc.) — without this we'd keep the first-launched
    /// instance and the cycle would route the user to the "wrong"
    /// VSCode. Distinct from the *bundle-level* MRU in `mru_bundles`
    /// (which orders the cycle list): this one only resolves
    /// bundle → which-pid.
    fn mru_map() -> &'static std::sync::Mutex<std::collections::HashMap<String, libc::pid_t>> {
        use std::collections::HashMap;
        use std::sync::OnceLock;
        static MRU: OnceLock<std::sync::Mutex<HashMap<String, libc::pid_t>>> = OnceLock::new();
        MRU.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
    }

    /// Global most-recently-used ordering of bundle identifiers, front
    /// of the vec = most recent. Drives the cycle order so Cmd+Tab
    /// behaves like native macOS: one press goes to the previous app,
    /// two presses to the one before, etc. — independent of process
    /// launch order. Updated only when (a) a new bundle is first seen,
    /// (b) a fresh cycle session starts (current frontmost bumped to
    /// the front), or (c) a cycle session commits on Cmd release
    /// (cursor target bumped to the front). Critically NOT updated by
    /// each intra-session Cmd+Tab press — that would reshuffle the
    /// list and ping-pong the cursor between two apps.
    fn mru_bundles() -> &'static std::sync::Mutex<Vec<String>> {
        use std::sync::OnceLock;
        static MRU: OnceLock<std::sync::Mutex<Vec<String>>> = OnceLock::new();
        MRU.get_or_init(|| std::sync::Mutex::new(Vec::new()))
    }

    fn promote_bundle_to_front(bundle: &str) {
        if bundle == "<no-bundle-id>" {
            return;
        }
        let mru = mru_bundles();
        let mut guard = mru.lock().expect("MRU bundles mutex poisoned");
        guard.retain(|b| b != bundle);
        guard.insert(0, bundle.to_string());
    }

    /// Active Cmd+Tab session. `snapshot` is the MRU-ordered candidate
    /// list frozen at session start; `cursor_pid` advances through it
    /// as the user holds Cmd and taps Tab. The snapshot does NOT
    /// reshuffle mid-session even though we activate each step —
    /// without that freeze, the activation would move the target to
    /// MRU front, and the next press would cycle right back to where
    /// we started.
    ///
    /// `cmd_release_gen` captures `CMD_RELEASE_GENERATION` at session
    /// start. Every observed Cmd release bumps the global counter;
    /// if the session's stored value still matches on the next press
    /// we know Cmd has been held continuously since this session
    /// began (no matter how long the user paused between Tab taps)
    /// and the session continues. When it differs, the user released
    /// Cmd between presses — commit the cursor to MRU and start fresh.
    /// This replaced an earlier 2 s time-based grace that incorrectly
    /// split sessions whenever a user paused mid-hold.
    ///
    /// `last_press_at` only serves as a hard upper bound on session
    /// lifetime (`CYCLE_RESUME_GRACE`) — a safety net for the rare
    /// case where a Cmd-release event is eaten before reaching us
    /// (RDP client focus loss taking the keyup with it).
    struct CycleSession {
        snapshot: Vec<(String, String, libc::pid_t)>,
        cursor_pid: libc::pid_t,
        last_press_at: Instant,
        cmd_release_gen: u64,
    }

    static CYCLE_SESSION: std::sync::Mutex<Option<CycleSession>> = std::sync::Mutex::new(None);

    /// Incremented every time both Cmd halves transition to released
    /// (see `commit_cycle_session` callers). A `CycleSession` stamps
    /// its start-time value; on later presses, a match means "Cmd has
    /// not been released since this session started" — so we
    /// continue, regardless of pause length.
    static CMD_RELEASE_GENERATION: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(0);

    /// `NSWorkspace.frontmostApplication()` does NOT update for app
    /// activations driven through the Accessibility API — it keeps
    /// returning the app that was front BEFORE our first AX activation,
    /// even after we've activated several others successfully. So we
    /// can't trust workspace alone to know "what app is the user
    /// actually in" on session start, which means every fresh Cmd+Tab
    /// would re-promote the stale workspace value to MRU front and
    /// undo the previous session's commit.
    ///
    /// `LAST_AX_ACTIVATED_PID` is our authoritative "this is what's
    /// really front" — updated after every successful AX activation,
    /// survives across cycle sessions.
    ///
    /// `WORKSPACE_LIE_FRONT` is the value `frontmostApplication()` was
    /// returning at the moment we did our first AX activation. While
    /// workspace keeps returning this value we treat it as stale. When
    /// workspace returns *anything else*, the OS has observed a focus
    /// change through a non-AX path (Dock click, menu, app self-
    /// activation) — we reset both statics and trust workspace again.
    static LAST_AX_ACTIVATED_PID: std::sync::Mutex<Option<libc::pid_t>> =
        std::sync::Mutex::new(None);
    static WORKSPACE_LIE_FRONT: std::sync::Mutex<Option<libc::pid_t>> = std::sync::Mutex::new(None);

    /// Enumerate every running process by PID directly from the kernel.
    ///
    /// We can't use `NSWorkspace.runningApplications()` for this — that
    /// API returns a cache that's only refreshed by Cocoa notifications
    /// delivered through the main thread's runloop, and we don't pump
    /// one (tokio owns the main thread). Apps launched after macrdp
    /// starts never appear there. `proc_listallpids` reads the proc
    /// table directly via the BSD-style libproc API, so it always
    /// reflects current kernel state.
    ///
    /// Note on the libproc API shape: `proc_listallpids` returns the
    /// **count of PIDs** (XNU wraps `proc_listpids` and divides by
    /// `sizeof(int)` before returning), and the buffer size passed in
    /// must be in **bytes**. Earlier versions of this function divided
    /// the return value by 4 again, producing `count=13` and clipping
    /// most processes from view.
    fn list_all_pids() -> Vec<libc::pid_t> {
        extern "C" {
            fn proc_listallpids(buffer: *mut libc::c_int, buffersize: libc::c_int) -> libc::c_int;
        }
        unsafe {
            // Sizing call: NULL buffer / 0 size returns the pid count
            // the kernel would have written. Add headroom for procs
            // that may launch between this call and the populated one.
            let probe = proc_listallpids(std::ptr::null_mut(), 0);
            if probe <= 0 {
                return Vec::new();
            }
            let capacity = probe as usize + 64;
            let mut buffer: Vec<libc::pid_t> = vec![0; capacity];
            let pid_count = proc_listallpids(
                buffer.as_mut_ptr() as *mut libc::c_int,
                (buffer.len() * std::mem::size_of::<libc::c_int>()) as libc::c_int,
            );
            if pid_count <= 0 {
                return Vec::new();
            }
            buffer.truncate(pid_count as usize);
            buffer.retain(|&p| p > 0);
            buffer
        }
    }

    /// Reconcile NSWorkspace's view of "frontmost app" with our own AX
    /// activation history. Returns the pid we believe is actually
    /// front (drives cycle decisions, MRU promotions, snapshot
    /// exclusion). See LAST_AX_ACTIVATED_PID / WORKSPACE_LIE_FRONT
    /// docs above.
    ///
    /// `workspace_pid` is whatever `frontmostApplication()` just
    /// returned. `is_alive` validates our tracked AX pid against the
    /// caller's notion of "this pid still runs" — `cycle_apps` already
    /// has `metas` so passes a `metas`-backed predicate; the Cmd+`
    /// window cycler doesn't and passes `kill(pid, 0)` instead.
    fn effective_front_pid(
        workspace_pid: libc::pid_t,
        is_alive: impl FnOnce(libc::pid_t) -> bool,
    ) -> libc::pid_t {
        let mut lie = WORKSPACE_LIE_FRONT
            .lock()
            .expect("workspace lie front mutex poisoned");
        let mut last_ax = LAST_AX_ACTIVATED_PID
            .lock()
            .expect("last AX activated pid mutex poisoned");
        if let (Some(lie_pid), Some(ax_pid)) = (*lie, *last_ax) {
            if lie_pid == workspace_pid && is_alive(ax_pid) {
                return ax_pid;
            }
        }
        // Workspace moved past whatever it was lying about (or never
        // lied yet, or our AX target died) — reset and trust workspace.
        *lie = None;
        *last_ax = None;
        workspace_pid
    }

    /// Spawn a background thread that polls the Accessibility system-wide
    /// focused application and promotes its bundle to `mru_bundles[0]`
    /// whenever it changes. Idempotent — only one thread is ever spawned.
    ///
    /// We originally polled `NSWorkspace.frontmostApplication()` for this,
    /// but in `--capture-primary` / virtual-display setups it stops tracking
    /// dock-click activations entirely (the property pins to whatever was
    /// front when the capture engaged and never updates, even though apps
    /// really are becoming frontmost when the user clicks the dock on the
    /// virtual display). AX's system-wide `kAXFocusedApplicationAttribute`
    /// reflects WindowServer's actual focus state directly and tracks
    /// these activations correctly.
    ///
    /// Skips promotion when a Cmd+Tab cycle session is active so that the
    /// AX activations *we* drive don't get re-promoted (mid-cycle reshuffles
    /// would corrupt the frozen snapshot's MRU ordering on the next session
    /// start). Our cycle commits already promote the cursor target on Cmd
    /// release; intra-cycle targets shouldn't bake into MRU.
    fn start_workspace_mru_poller() {
        use std::sync::OnceLock;
        static STARTED: OnceLock<()> = OnceLock::new();
        STARTED.get_or_init(|| {
            let _ = std::thread::Builder::new()
                .name("ax-focus-mru-poller".into())
                .spawn(ax_focus_mru_poller_loop);
        });
    }

    /// Millis-since-process-start of the last forwarded RDP input event
    /// (`u64::MAX` = none yet). One relaxed store per event from
    /// `Inner::{keyboard,mouse}`; read by the MRU focus poller so its 5×/s
    /// system-wide AX query parks while the session is input-idle — the MRU /
    /// focus data it maintains only matters while a user is actually driving
    /// input (Cmd+Tab, Ctrl→Cmd remap gating), and without the gate the
    /// poller kept waking the process forever, including after the client
    /// disconnected.
    static LAST_INPUT_MS: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(u64::MAX);

    /// Monotonic millis since process start (first call anchors the epoch).
    fn monotonic_ms() -> u64 {
        use std::sync::OnceLock;
        static START: OnceLock<Instant> = OnceLock::new();
        START.get_or_init(Instant::now).elapsed().as_millis() as u64
    }

    /// Record that an RDP input event was just forwarded.
    fn mark_input_activity() {
        LAST_INPUT_MS.store(monotonic_ms(), std::sync::atomic::Ordering::Relaxed);
    }

    /// True while input was seen within the last minute — the window during
    /// which the MRU poller keeps its full 5 Hz cadence. Generous on purpose:
    /// the poller's fallback focus data may be consulted by the very first
    /// keystroke after a pause, and a poller parked ≤1 s behind (see the loop)
    /// combined with the event-driven feeders (NSWorkspace observer + click
    /// hit-test) covers that first keystroke fine.
    fn input_recently_active() -> bool {
        let last = LAST_INPUT_MS.load(std::sync::atomic::Ordering::Relaxed);
        last != u64::MAX && monotonic_ms().saturating_sub(last) < 60_000
    }

    /// Read the pid of the AX system-wide focused application. Returns
    /// None if AX can't resolve it (permission missing, focus on a non-
    /// AX-bearing element, etc.).
    fn ax_focused_application_pid() -> Option<libc::pid_t> {
        use core_foundation::base::{CFRelease, CFTypeRef, TCFType};
        use core_foundation::string::CFString;
        use std::ffi::c_void;
        use std::ptr;

        unsafe {
            let systemwide = AXUIElementCreateSystemWide();
            if systemwide.is_null() {
                return None;
            }
            let attr = CFString::from_static_string("AXFocusedApplication");
            let mut focused: CFTypeRef = ptr::null();
            let err = AXUIElementCopyAttributeValue(
                systemwide,
                attr.as_concrete_TypeRef().cast(),
                &mut focused,
            );
            CFRelease(systemwide as *const c_void);
            if err != AX_ERROR_SUCCESS || focused.is_null() {
                return None;
            }
            let mut pid: libc::pid_t = 0;
            let pid_err = AXUIElementGetPid(focused as *mut c_void, &mut pid);
            CFRelease(focused);
            if pid_err != AX_ERROR_SUCCESS || pid <= 0 {
                return None;
            }
            Some(pid)
        }
    }

    /// Look up the bundle identifier of a running app by pid. Used by
    /// the AX poller to translate the focused-app pid into the bundle
    /// string our MRU is keyed by. Uses
    /// `NSRunningApplication.runningApplicationWithProcessIdentifier`
    /// directly — that path does a fresh per-call lookup against
    /// LaunchServices and sees newly-launched apps, unlike
    /// `NSWorkspace.runningApplications()` which is cached and stale
    /// in our (non-Cocoa-runloop) process.
    fn bundle_identifier_for_pid(pid: libc::pid_t) -> Option<String> {
        // Fast path: a normal app resolves directly via NSRunningApplication.
        let direct = unsafe {
            NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
                .and_then(|a| a.bundleIdentifier())
                .map(|s| s.to_string())
        };
        if let Some(bid) = direct {
            if !bid.is_empty() {
                return Some(bid);
            }
        }
        // Helper path: AXFocusedApplication hands us Electron *helper* process
        // pids (VSCode's "Code Helper (Renderer)", Slack, etc.) that
        // NSRunningApplication returns no bundle id for — so a focused Electron
        // app would otherwise be invisible to both the focus poller and the
        // Ctrl→Cmd exclusion check. Derive the OWNING app's bundle id from the
        // process executable path's OUTERMOST `.app` bundle, so e.g. a focused
        // VSCode resolves to `com.microsoft.VSCode` regardless of which helper
        // holds focus.
        bundle_id_from_pid_path(pid)
    }

    /// Resolve a pid to the bundle id of the outermost `.app` containing its
    /// executable (via `proc_pidpath` + `NSBundle`). Used as the helper-process
    /// fallback in `bundle_identifier_for_pid`. Returns None if the path can't be
    /// read or isn't inside an app bundle.
    fn bundle_id_from_pid_path(pid: libc::pid_t) -> Option<String> {
        extern "C" {
            fn proc_pidpath(
                pid: libc::c_int,
                buffer: *mut libc::c_void,
                buffersize: u32,
            ) -> libc::c_int;
        }
        // PROC_PIDPATHINFO_MAXSIZE = 4 * MAXPATHLEN.
        let mut buf = vec![0u8; 4096];
        let n = unsafe {
            proc_pidpath(
                pid as libc::c_int,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len() as u32,
            )
        };
        if n <= 0 {
            return None;
        }
        let path = std::str::from_utf8(&buf[..n as usize]).ok()?;
        // The outermost app bundle is the FIRST ".app" component in the path
        // (e.g. ".../Visual Studio Code.app/Contents/Frameworks/Code Helper.app/
        // .../Code Helper" → ".../Visual Studio Code.app").
        let idx = path.find(".app/")?;
        let app_path = &path[..idx + 4];
        unsafe {
            let ns = objc2_foundation::NSString::from_str(app_path);
            let bundle = objc2_foundation::NSBundle::bundleWithPath(&ns)?;
            bundle
                .bundleIdentifier()
                .map(|s| s.to_string())
                .filter(|s| !s.is_empty())
        }
    }

    /// Standalone-terminal bundle ids where the Ctrl→Cmd remap is always
    /// suppressed, so `Ctrl+C` keeps reaching the shell as SIGINT. Embedded
    /// terminals (VSCode's integrated TTY etc.) can't be auto-detected — the
    /// frontmost app is the IDE, not a terminal — so they're covered by the
    /// user-extensible --no-remap-apps list instead.
    const BUILTIN_TERMINAL_BUNDLES: &[&str] = &[
        "com.apple.Terminal",
        "com.googlecode.iterm2",
        "io.alacritty",
        "net.kovidgoyal.kitty",
        "com.github.wez.wezterm",
        "com.mitchellh.ghostty",
        "co.zeit.hyper",
        "dev.warp.Warp-Stable",
    ];

    /// The app the user is currently interacting with (bundle id), as the Ctrl→Cmd
    /// suppression's primary "what's focused right now" signal. Updated, last-wins,
    /// by TWO sources because no single macOS API covers every case in this
    /// headless/virtual-display context:
    ///   - the `NSWorkspace` activation observer (`init_focus_observer`) — fires on
    ///     Cmd+Tab and on clicking apps that activate normally; and
    ///   - a **click hit-test** (`update_focus_from_click`) — resolves the app whose
    ///     window is under the cursor at mouse-down, which is the ONLY way to catch
    ///     an Electron app (VSCode) clicked into in this setup: it takes key focus
    ///     (keystrokes land in it) WITHOUT emitting an activation, so neither
    ///     `AXFocusedApplication` nor `NSWorkspace` ever reports it.
    static LAST_FOCUS_BUNDLE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

    /// Register an `NSWorkspaceDidActivateApplicationNotification` observer so we
    /// always know the current frontmost app. It fires on **every** activation
    /// (including mouse-click focus and Electron apps), and the activated app is
    /// carried in the notification's `userInfo` as an `NSRunningApplication`, whose
    /// `bundleIdentifier` is the **main app** id (not an Electron helper). Runs on
    /// the dedicated CFRunLoop thread because `NSWorkspace` notifications need a
    /// live, pumped runloop — macrdp's main thread (owned by tokio) has none. The
    /// observer token + block are leaked intentionally (process-lifetime).
    ///
    /// This replaces the unreliable AX-poll / MRU-front guess that the Ctrl→Cmd
    /// suppression used before: AX polling never reported VSCode focused by click,
    /// so the remap wrongly fired in its integrated terminal.
    pub(super) fn init_focus_observer() {
        crate::runloop_thread::submit(|| unsafe {
            use objc2::rc::Retained;
            use objc2::runtime::AnyObject;
            use objc2_app_kit::{
                NSWorkspace, NSWorkspaceApplicationKey,
                NSWorkspaceDidActivateApplicationNotification,
            };
            use objc2_foundation::{NSNotification, NSString};
            use std::ptr::NonNull;

            let center = NSWorkspace::sharedWorkspace().notificationCenter();
            let block = block2::RcBlock::new(move |notif: NonNull<NSNotification>| {
                let notif: &NSNotification = notif.as_ref();
                let Some(info) = notif.userInfo() else { return };
                let key: &AnyObject = NSWorkspaceApplicationKey.as_ref();
                // The value is the activated NSRunningApplication; read its
                // bundleIdentifier directly (the main app id, not a helper).
                let Some(obj) = info.objectForKey(key) else {
                    return;
                };
                let bid: Option<Retained<NSString>> = objc2::msg_send_id![&*obj, bundleIdentifier];
                if let Some(bid) = bid {
                    let s = bid.to_string();
                    if !s.is_empty() {
                        debug!(bundle = %s, "ctrl→cmd activation focus");
                        if let Ok(mut g) = LAST_FOCUS_BUNDLE.lock() {
                            *g = Some(s);
                        }
                    }
                }
            });
            let token = center.addObserverForName_object_queue_usingBlock(
                Some(NSWorkspaceDidActivateApplicationNotification),
                None,
                None,
                &block,
            );
            std::mem::forget(token);
            std::mem::forget(block);
            debug!("ctrl→cmd: NSWorkspace activation observer registered");
        });
    }

    /// True when the frontmost app is a built-in terminal or appears in the user's
    /// --no-remap-apps list — in which case the Ctrl→Cmd remap is suppressed (so
    /// Ctrl+C stays SIGINT). Called on a remappable Ctrl-shortcut key-down.
    ///
    /// Frontmost resolution priority: (1) `LAST_FOCUS_BUNDLE` — the last-wins merge
    /// of the `NSWorkspace` activation observer AND the click hit-test, which
    /// together cover Cmd+Tab, normal click-activation, AND Electron click-focus;
    /// (2) a direct `AXFocusedApplication` lookup (returns None on this input
    /// thread, but kept as a belt-and-suspenders); (3) the MRU front
    /// (`mru_bundles()[0]`) the focus poller maintains.
    ///
    /// Electron apps focused via a helper can surface a `.helper[.Renderer]`
    /// suffixed id, so we match an exclusion entry by **equality OR a dotted-prefix**
    /// (`<entry>.`) — listing `com.microsoft.VSCode` also covers its helpers; the
    /// trailing dot keeps it from matching a sibling like `…VSCodeInsiders`.
    fn frontmost_is_excluded() -> bool {
        let bid = LAST_FOCUS_BUNDLE
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .or_else(|| ax_focused_application_pid().and_then(bundle_identifier_for_pid))
            .or_else(|| mru_bundles().lock().ok().and_then(|m| m.first().cloned()));
        let Some(bid) = bid else {
            return false;
        };
        let matches = |entry: &str| bid == entry || bid.starts_with(&format!("{entry}."));
        let excluded = BUILTIN_TERMINAL_BUNDLES.iter().any(|e| matches(e))
            || super::NO_REMAP_APPS
                .get()
                .is_some_and(|list| list.iter().any(|e| matches(e)));
        debug!(bundle = %bid, excluded, "ctrl→cmd frontmost check");
        excluded
    }

    /// Update `LAST_FOCUS_BUNDLE` from a mouse-down at the given GLOBAL screen
    /// point (top-left origin — the same space `input.rs` posts mouse events in).
    /// Hit-tests the window under the cursor via `AXUIElementCopyElementAtPosition`
    /// and resolves its owning app's bundle id. This is the only signal that
    /// catches an Electron app (VSCode) clicked into in a headless/virtual-display
    /// session: it takes key focus without emitting an activation, so the
    /// activation observer and `AXFocusedApplication` never see it. Best-effort —
    /// runs on the dedicated CFRunLoop thread (where AX is reliable, unlike the
    /// input thread) and updates the cell before the user's next keystroke. Only
    /// armed when `--map-ctrl-to-cmd` is on (the only consumer). No-op if AX can't
    /// resolve a window/app at the point (menu bar, empty space).
    fn update_focus_from_click(x: f64, y: f64) {
        crate::runloop_thread::submit(move || unsafe {
            use std::ffi::c_void;
            let systemwide = AXUIElementCreateSystemWide();
            if systemwide.is_null() {
                return;
            }
            let mut element: *mut c_void = std::ptr::null_mut();
            let err =
                AXUIElementCopyElementAtPosition(systemwide, x as f32, y as f32, &mut element);
            CFRelease(systemwide as *const c_void);
            if err != AX_ERROR_SUCCESS || element.is_null() {
                return;
            }
            let mut pid: libc::pid_t = 0;
            let pid_err = AXUIElementGetPid(element, &mut pid);
            CFRelease(element as *const c_void);
            if pid_err != AX_ERROR_SUCCESS || pid <= 0 {
                return;
            }
            if let Some(bundle) = bundle_identifier_for_pid(pid) {
                if !bundle.is_empty() {
                    debug!(pid, bundle = %bundle, "ctrl→cmd click focus");
                    if let Ok(mut g) = LAST_FOCUS_BUNDLE.lock() {
                        *g = Some(bundle);
                    }
                }
            }
        });
    }

    /// Find the pid of the first running app matching `bundle`. Walks
    /// the kernel pid list and queries each via
    /// NSRunningApplication; both calls bypass the stale NSWorkspace
    /// cache. Returns None if the bundle isn't running.
    fn pid_for_bundle(bundle: &str) -> Option<libc::pid_t> {
        for pid in list_all_pids() {
            let app = unsafe { NSRunningApplication::runningApplicationWithProcessIdentifier(pid) };
            let Some(app) = app else { continue };
            let bid = unsafe { app.bundleIdentifier() };
            let Some(b) = bid else { continue };
            if b.to_string() == bundle {
                return Some(pid);
            }
        }
        None
    }

    fn ax_focus_mru_poller_loop() {
        let mut last_seen_pid: libc::pid_t = 0;
        loop {
            // Park while input-idle: no RDP input in the last minute (or ever)
            // means nobody is Cmd+Tab-ing or typing, so the system-wide AX
            // focus query 5×/s is pure wasted wakeups — including forever
            // after a disconnect. Check a cheap atomic at 1 Hz instead; the
            // first event after a pause restores full cadence within ~1 s.
            if !input_recently_active() {
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
            std::thread::sleep(Duration::from_millis(200));
            let Some(pid) = ax_focused_application_pid() else {
                continue;
            };
            if pid == last_seen_pid {
                continue;
            }
            last_seen_pid = pid;

            // Skip while a Cmd+Tab cycle is in flight — our own AX
            // activations are causing the focus changes the poller is
            // seeing, and we don't want them double-counted into MRU.
            // (Cmd-release commit already promotes the final target.)
            let cycle_active = CYCLE_SESSION
                .lock()
                .expect("cycle session mutex poisoned")
                .is_some();
            if cycle_active {
                continue;
            }

            let Some(bundle) = bundle_identifier_for_pid(pid) else {
                continue;
            };
            if bundle.is_empty() {
                continue;
            }
            promote_bundle_to_front(&bundle);
            debug!(
                pid,
                bundle = %bundle,
                "AX focus MRU poller promoted bundle"
            );
        }
    }

    /// Hard upper bound on cycle-session lifetime. Generation matching
    /// is the primary "is the session still alive" signal — this
    /// timeout is only a safety net for the rare case where the
    /// Cmd-release event was eaten before reaching us (RDP client
    /// focus loss taking the keyup with it). 5 min comfortably
    /// accommodates "user paused to read something" cases while still
    /// eventually recovering from a stuck session.
    const CYCLE_RESUME_GRACE: Duration = Duration::from_secs(300);

    /// Commit the active cycle session — promote cursor's bundle to
    /// MRU front and clear the session. Called when Cmd is fully
    /// released, and lazily from cycle_apps when the grace timeout
    /// has elapsed.
    fn commit_cycle_session() {
        // Take the session out under the CYCLE_SESSION lock, then DROP the lock
        // (it's confined to this block) before any HUD / AX / mru /
        // LAST_FOCUS_BUNDLE work below — those acquire OTHER mutexes, and holding
        // CYCLE_SESSION across them would nest locks (the same lock-order hazard
        // removed from h264 `ship_frames`: LAST_FOCUS_BUNDLE is written cross-
        // thread by the focus observer / click hit-test). Nothing below needs the
        // session map locked, only the taken session's data. The session is
        // already `take()`n, so a racing cycle_apps just starts a fresh one — the
        // correct outcome after a commit.
        let session = {
            let mut guard = CYCLE_SESSION.lock().expect("cycle session mutex poisoned");
            match guard.take() {
                Some(s) => s,
                None => return,
            }
        };
        // A session existed → tear down the on-screen HUD (best-effort).
        if super::APP_SWITCHER_HUD.load(std::sync::atomic::Ordering::Relaxed) {
            crate::switcher_hud::hide();
        }
        if let Some((bundle, _, _)) = session
            .snapshot
            .iter()
            .find(|(_, _, p)| *p == session.cursor_pid)
        {
            promote_bundle_to_front(bundle);
            // Record the landed app as the current focus for the Ctrl→Cmd
            // remap. Its `frontmost_is_excluded` reads LAST_FOCUS_BUNDLE
            // first, and our own Cmd+Tab/Option+Tab AX activation doesn't
            // reliably post an NSWorkspace activation — so without this,
            // cycling from an excluded app (terminal/VSCode) to a normal one
            // left the remap suppressed until the user clicked the new app.
            if let Ok(mut g) = LAST_FOCUS_BUNDLE.lock() {
                *g = Some(bundle.clone());
            }
        }
        let cursor_pid = session.cursor_pid;
        // On commit (Cmd release), re-assert the landed app and, if it has no
        // window (e.g. Notes/Calendar/Mail with their window closed), reopen it so
        // it surfaces — both gated to commit so apps merely cycled *through* aren't
        // un-minimized or popped open. Un-minimize only when --unminimize-on-switch.
        ax_make_frontmost(
            cursor_pid,
            super::UNMINIMIZE_ON_SWITCH.load(std::sync::atomic::Ordering::Relaxed),
            true,
        );
    }

    /// Cycle to the next (or previous) regular-policy running app and
    /// activate it. Replaces Cmd+Tab / Cmd+Shift+Tab, which WindowServer
    /// won't fire for CGEvent-posted keystrokes.
    fn cycle_apps(reverse: bool) {
        // SAFETY: NSWorkspace / NSRunningApplication are documented thread-
        // safe for these read-only queries.
        unsafe {
            let workspace = NSWorkspace::sharedWorkspace();
            // `NSWorkspace.frontmostApplication` is the live-queryable
            // replacement for `isActive` checks on cached snapshots: it
            // walks the active app fresh each call. We compare by PID
            // since NSRunningApplication instances don't share identity
            // across queries.
            let workspace_front_pid = workspace
                .frontmostApplication()
                .map(|a| a.processIdentifier())
                .unwrap_or(0);
            // Pass 1: gather every running app's metadata.
            //
            // We deliberately AVOID `NSWorkspace.runningApplications()`
            // here: its result is a cache refreshed via Cocoa
            // notifications that require a main-thread runloop, and
            // tokio owns our main thread. Apps launched after macrdp
            // starts never appear in that cache. Instead we read the
            // kernel proc table directly via `proc_listallpids` and
            // ask NSRunningApplication for each PID — that path does
            // a fresh per-call lookup and sees newly-launched apps
            // immediately.
            struct AppMeta {
                bundle: String,
                name: String,
                pid: libc::pid_t,
                policy: NSApplicationActivationPolicy,
                terminated: bool,
            }
            let pids = list_all_pids();
            let mut metas: Vec<AppMeta> = Vec::with_capacity(pids.len());
            for pid in pids {
                let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
                else {
                    continue;
                };
                // `isTerminated` lags the kernel by a few hundred ms
                // when an app quits — long enough for the first Cmd+Tab
                // after a close to still see the dead instance in the
                // candidate list (and, via MRU, potentially target it).
                // `kill(pid, 0)` is authoritative: returns 0 if the
                // process is alive, -1/ESRCH if it isn't. EPERM means
                // it exists but is not ours to signal — still alive.
                let kernel_dead =
                    pid <= 0 || (libc::kill(pid, 0) == -1 && *libc::__error() == libc::ESRCH);
                metas.push(AppMeta {
                    bundle: app
                        .bundleIdentifier()
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "<no-bundle-id>".to_string()),
                    name: app
                        .localizedName()
                        .map(|s| s.to_string())
                        .unwrap_or_default(),
                    pid,
                    policy: app.activationPolicy(),
                    terminated: app.isTerminated() || kernel_dead,
                });
            }
            let all_count = metas.len();

            // Reconcile workspace's notion of "front" against what we
            // last AX-activated. Workspace lies (returns the pre-AX
            // app) after we've activated something, so trusting it
            // blindly would re-promote the stale value to MRU front on
            // every session start and undo the previous commit. Using
            // the reconciled `front_pid` from here on means MRU
            // tracking actually survives a Cmd+Tab → release → Cmd+Tab
            // cycle.
            let front_pid = effective_front_pid(workspace_front_pid, |p| {
                metas.iter().any(|m| m.pid == p && !m.terminated)
            });

            // Refresh MRU. Whichever bundle is currently frontmost is the
            // one the user just left (or was working in if this is their
            // first Cmd+Tab) — that's the instance Cmd+Tab back should
            // land on later.
            {
                let mru = mru_map();
                let mut guard = mru.lock().expect("MRU mutex poisoned");
                if let Some(m) = metas.iter().find(|m| m.pid == front_pid) {
                    if m.bundle != "<no-bundle-id>" {
                        guard.insert(m.bundle.clone(), m.pid);
                    }
                }
                // Drop stale entries pointing at quit / terminated PIDs.
                guard.retain(|_, pid| metas.iter().any(|m| m.pid == *pid && !m.terminated));
            }
            let mru_snapshot: std::collections::HashMap<String, libc::pid_t> = {
                let mru = mru_map();
                mru.lock().expect("MRU mutex poisoned").clone()
            };

            // Pass 2: dedup by bundle, preferring the MRU instance. Apps
            // without a bundle ID pass through individually (rare for
            // regular-policy apps).
            let mut regular: Vec<(String, String, libc::pid_t)> = Vec::new();
            let mut by_bundle: std::collections::HashMap<String, usize> =
                std::collections::HashMap::new();
            let mut dup_pids: HashSet<libc::pid_t> = HashSet::new();
            for m in &metas {
                if m.policy != NSApplicationActivationPolicy::Regular || m.terminated {
                    continue;
                }
                if m.bundle == "<no-bundle-id>" {
                    regular.push((m.bundle.clone(), m.name.clone(), m.pid));
                    continue;
                }
                match by_bundle.get(&m.bundle).copied() {
                    None => {
                        by_bundle.insert(m.bundle.clone(), regular.len());
                        regular.push((m.bundle.clone(), m.name.clone(), m.pid));
                    }
                    Some(existing_idx) => {
                        // Two instances of the same bundle. Replace the
                        // kept entry only if the new candidate is the
                        // bundle's MRU pid; otherwise keep what we have.
                        if mru_snapshot.get(&m.bundle) == Some(&m.pid) {
                            dup_pids.insert(regular[existing_idx].2);
                            regular[existing_idx] = (m.bundle.clone(), m.name.clone(), m.pid);
                        } else {
                            dup_pids.insert(m.pid);
                        }
                    }
                }
            }

            // Build the human-readable dump after dedup so DUP tags reflect
            // actual decisions.
            let all_summary: Vec<String> = metas
                .iter()
                .map(|m| {
                    let mut tags = String::new();
                    if m.pid == front_pid {
                        tags.push_str(", FRONT");
                    }
                    if m.terminated {
                        tags.push_str(", TERMINATED");
                    }
                    if dup_pids.contains(&m.pid) {
                        tags.push_str(", DUP");
                    }
                    if mru_snapshot.get(&m.bundle) == Some(&m.pid) {
                        tags.push_str(", MRU");
                    }
                    format!(
                        "{bundle} (name={name:?}, policy={policy:?}, pid={pid}{tags})",
                        bundle = m.bundle,
                        name = m.name,
                        policy = m.policy.0,
                        pid = m.pid,
                    )
                })
                .collect();
            debug!(
                count = all_count,
                regular_count = regular.len(),
                front_pid,
                workspace_front_pid,
                "cycle_apps: full app list:\n  {}",
                all_summary.join("\n  ")
            );
            if regular.is_empty() {
                warn!("cycle_apps: no regular apps running");
                return;
            }

            // Decide: continue an existing held-Cmd session, or start
            // a fresh one. A fresh session rebuilds the snapshot in
            // MRU order (most recently used first) so press #1 lands
            // on the previous-MRU app, press #2 on the one before
            // that, etc. — matching native macOS Cmd+Tab. A continued
            // session reuses the frozen snapshot so successive
            // presses walk further back without the list reshuffling
            // under us (each AX activation would otherwise promote
            // the new target and ping-pong the cursor).
            let now = Instant::now();
            let current_release_gen =
                CMD_RELEASE_GENERATION.load(std::sync::atomic::Ordering::SeqCst);
            let mut session_guard = CYCLE_SESSION.lock().expect("cycle session mutex poisoned");
            // Continue if the session's stored release-generation
            // matches *and* the session is younger than the safety
            // bound. Generation matching is the real authority —
            // it's true iff Cmd has been held continuously since
            // session start. The grace check only kicks in to
            // recover from a missed Cmd-release event (RDP focus
            // loss eating the keyup).
            let continuing = session_guard.as_ref().is_some_and(|s| {
                s.cmd_release_gen == current_release_gen
                    && now.duration_since(s.last_press_at) < CYCLE_RESUME_GRACE
            });
            if !continuing && session_guard.is_some() {
                // Either Cmd was released between presses (generation
                // bumped) or the safety bound elapsed. Treat
                // any pending cursor as the user's last actual
                // selection and commit it to MRU before starting over.
                if let Some(stale) = session_guard.take() {
                    if let Some((bundle, _, _)) = stale
                        .snapshot
                        .iter()
                        .find(|(_, _, p)| *p == stale.cursor_pid)
                    {
                        promote_bundle_to_front(bundle);
                    }
                }
            }

            // Build (or reuse) the snapshot. The session origin (the
            // front-at-start app) IS included so the cycle has the
            // full set of regular apps. Native macOS Cmd+Tab cycles
            // through all apps including back to the starting one —
            // mirroring that gives users a familiar "I can reach all
            // N of my apps in one Cmd hold" experience. Each press
            // visibly switches to a different app (since we re-issue
            // AX activation every step), so even the wrap-back-to-
            // origin shows on screen.
            let (snapshot, cursor_pid_in): (Vec<(String, String, libc::pid_t)>, libc::pid_t) =
                if continuing {
                    let s = session_guard.as_mut().unwrap();
                    // Drop snapshot entries whose pid is no longer a
                    // running regular-policy app (quit mid-cycle).
                    let live: HashSet<libc::pid_t> = regular.iter().map(|(_, _, p)| *p).collect();
                    s.snapshot.retain(|(_, _, p)| live.contains(p));
                    let cursor = s.cursor_pid;
                    (s.snapshot.clone(), cursor)
                } else {
                    // Fresh session. Ingest any new bundles into
                    // mru_bundles (append at the end — least-recently
                    // used by default), then bump the live frontmost
                    // bundle to the front so it becomes the cycle
                    // origin even if NSWorkspace's frontmost tracker
                    // is stale relative to our last AX activation.
                    {
                        let mru = mru_bundles();
                        let mut guard = mru.lock().expect("MRU bundles mutex poisoned");
                        for (bundle, _, _) in &regular {
                            if bundle != "<no-bundle-id>" && !guard.iter().any(|b| b == bundle) {
                                guard.push(bundle.clone());
                            }
                        }
                    }
                    if let Some(m) = metas.iter().find(|m| m.pid == front_pid) {
                        promote_bundle_to_front(&m.bundle);
                    }
                    // Sort regular[] by MRU position. `<no-bundle-id>`
                    // entries (rare for regular-policy apps) sort to
                    // the end. Front_pid is at MRU[0] post-promote, so
                    // it lands at index 0 of the snapshot — the cursor
                    // starts there and the first Tab press advances
                    // to index 1 = previous-MRU app.
                    let mru_order: Vec<String> = mru_bundles()
                        .lock()
                        .expect("MRU bundles mutex poisoned")
                        .clone();
                    let mut ordered = regular.clone();
                    ordered.sort_by_key(|(bundle, _, _)| {
                        mru_order
                            .iter()
                            .position(|b| b == bundle)
                            .unwrap_or(usize::MAX)
                    });
                    (ordered, front_pid)
                };

            if snapshot.is_empty() {
                *session_guard = None;
                if super::APP_SWITCHER_HUD.load(std::sync::atomic::Ordering::Relaxed) {
                    crate::switcher_hud::hide();
                }
                debug!("cycle_apps: nothing to cycle to (no regular apps)");
                return;
            }

            let n = snapshot.len();
            // Locate cursor in the snapshot and advance ±1 with wrap.
            // If the cursor pid isn't in the snapshot (e.g., front_pid
            // was a non-regular app like Spotlight that doesn't appear
            // in `regular`), land on the most-recent MRU entry as the
            // best-effort starting point.
            let next_idx = match snapshot.iter().position(|(_, _, p)| *p == cursor_pid_in) {
                Some(i) => {
                    if reverse {
                        (i + n - 1) % n
                    } else {
                        (i + 1) % n
                    }
                }
                None => {
                    if reverse {
                        n - 1
                    } else {
                        0
                    }
                }
            };
            let (target_bundle, target_name, target_pid) = snapshot[next_idx].clone();

            // Save updated session so the next intra-hold press
            // advances from where we are now. Snapshot stays frozen —
            // do NOT promote target to MRU front here; that happens
            // at session commit (Cmd release / grace expiry).
            *session_guard = Some(CycleSession {
                snapshot: snapshot.clone(),
                cursor_pid: target_pid,
                last_press_at: now,
                cmd_release_gen: current_release_gen,
            });
            drop(session_guard);

            // Drive the optional on-screen HUD (best-effort; --app-switcher-hud).
            // SHOW on a fresh session (full app list), ADVANCE within a hold.
            if super::APP_SWITCHER_HUD.load(std::sync::atomic::Ordering::Relaxed) {
                if continuing {
                    crate::switcher_hud::advance(next_idx);
                } else {
                    let apps: Vec<(i32, String)> = snapshot
                        .iter()
                        .map(|(_, name, pid)| (*pid, name.clone()))
                        .collect();
                    crate::switcher_hud::show(apps, next_idx);
                }
            }

            // Pin the per-bundle MRU pid to the instance we're
            // activating, so if the user cycles away and back the
            // dedup keeps the same instance.
            if target_bundle != "<no-bundle-id>" {
                mru_map()
                    .lock()
                    .expect("MRU mutex poisoned")
                    .insert(target_bundle.clone(), target_pid);
            }
            // Dump the snapshot order (the actual cycle order, with
            // session-origin excluded) and the underlying global MRU
            // list, so a confusing cycle decision is debuggable from
            // the log alone without re-deriving state.
            let snapshot_order: Vec<String> = snapshot
                .iter()
                .map(|(b, _, p)| format!("{b}#{p}"))
                .collect();
            let mru_order: Vec<String> = mru_bundles()
                .lock()
                .expect("MRU bundles mutex poisoned")
                .clone();
            debug!(
                reverse,
                continuing,
                cursor_pid_in,
                next_idx,
                snapshot_len = n,
                effective_front_pid = front_pid,
                workspace_front_pid,
                target = %target_bundle,
                name = %target_name,
                pid = target_pid,
                snapshot_order = ?snapshot_order,
                mru_bundles = ?mru_order,
                "cycle_apps activating"
            );
            // Three activation paths exist on modern macOS:
            //   - NSRunningApplication.activateWithOptions: silently no-ops
            //     when the caller isn't already the front app (macOS 14+
            //     front-app-only enforcement).
            //   - osascript `tell application "X" to activate`: Apple
            //     Event, gated by TCC Automation. Unsigned macrdp running
            //     headless never gets the first-run grant prompt and the
            //     command silently fails.
            //   - `/usr/bin/open -b <bundle-id>`: LaunchServices route.
            //     The target's self-activate hits the same front-app-only
            //     gate, so it doesn't activate — BUT it does *launch* the
            //     bundle if no process is running, which gets in our way:
            //     when the user has just quit an app, falling through here
            //     re-launches it instead of skipping. Removed.
            //
            // Accessibility API is the one that works. macrdp already
            // holds the Accessibility TCC grant (required for CGEventPost
            // to take effect at all); AX permission lets us set
            // kAXFrontmostAttribute on a target app's AXUIElement, which
            // is the same gesture the Dock uses internally. No
            // front-app-only block, and — critically — doesn't relaunch
            // dead processes.
            // Capture workspace's current value the first time we're
            // about to make it go stale. After this AX activation,
            // `frontmostApplication()` will keep returning whatever
            // it was returning *right now*, and that's the value
            // `effective_front_pid` needs to recognize as lying.
            // Only set if currently None — within a held-Cmd cycle the
            // first activation already captured it; subsequent intra-
            // cycle activations don't change what workspace is lying
            // about.
            {
                let mut lie = WORKSPACE_LIE_FRONT
                    .lock()
                    .expect("workspace lie front mutex poisoned");
                if lie.is_none() {
                    *lie = Some(workspace_front_pid);
                }
            }
            // Force-show: un-minimize the target on each cycle step too (when
            // enabled), so a minimized app pops open as you Cmd+Tab to it rather
            // than only on release. A brief de-miniaturize flicker is expected.
            // Per cycle step: do NOT reopen windowless apps (that's commit-only,
            // so apps cycled *through* don't pop windows).
            let ax_err = ax_make_frontmost(
                target_pid,
                super::UNMINIMIZE_ON_SWITCH.load(std::sync::atomic::Ordering::Relaxed),
                false,
            );
            if ax_err == AX_ERROR_SUCCESS {
                *LAST_AX_ACTIVATED_PID
                    .lock()
                    .expect("last AX activated pid mutex poisoned") = Some(target_pid);
                // Promote on every successful activation, not just on
                // Cmd-release commit. Our cycle has no switcher UI, so
                // each Tab press visibly switches apps and the user
                // experiences every step as a real visit — MRU should
                // reflect that or the next session's cycle order will
                // disagree with what they just saw on screen. Snapshot
                // is frozen so this doesn't disrupt the active cycle;
                // it only affects what subsequent sessions consult.
                if target_bundle != "<no-bundle-id>" {
                    promote_bundle_to_front(&target_bundle);
                }
                debug!(target = %target_bundle, pid = target_pid, "cycle_apps: AX activation ok");
            } else {
                warn!(
                    target = %target_bundle,
                    pid = target_pid,
                    ax_err,
                    "cycle_apps: AX activation failed — skipping (no relaunch fallback)"
                );
            }
        }
    }

    const AX_ERROR_SUCCESS: i32 = 0;
    const AX_ERROR_ILLEGAL_ARGUMENT: i32 = -25201; // close enough — only used for null guard

    // Shared AX FFI surface used by the activation/window-cycling
    // helpers below. macrdp already holds the Accessibility TCC grant
    // (CGEventPost wouldn't work without it), so these calls don't
    // trip a permission gate. All return `AXError`:
    //   0       = kAXErrorSuccess
    //   -25201  = kAXErrorAPIDisabled (AX permission missing)
    //   -25204  = kAXErrorAttributeUnsupported (target has no AX
    //             attribute — e.g., a freshly-launched app whose main
    //             run loop hasn't installed AX yet)
    //   -25205  = kAXErrorNotImplemented
    //   -25211  = kAXErrorIllegalArgument (bad PID / null pointer)
    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXUIElementCreateApplication(pid: libc::pid_t) -> *mut std::ffi::c_void;
        fn AXUIElementCreateSystemWide() -> *mut std::ffi::c_void;
        fn AXUIElementGetPid(element: *mut std::ffi::c_void, pid: *mut libc::pid_t) -> i32;
        fn AXUIElementCopyElementAtPosition(
            application: *mut std::ffi::c_void,
            x: f32,
            y: f32,
            element: *mut *mut std::ffi::c_void,
        ) -> i32;
        fn AXUIElementSetAttributeValue(
            element: *mut std::ffi::c_void,
            attribute: core_foundation::base::CFTypeRef,
            value: core_foundation::base::CFTypeRef,
        ) -> i32;
        fn AXUIElementCopyAttributeValue(
            element: *mut std::ffi::c_void,
            attribute: core_foundation::base::CFTypeRef,
            value: *mut core_foundation::base::CFTypeRef,
        ) -> i32;
        fn AXUIElementPerformAction(
            element: *mut std::ffi::c_void,
            action: core_foundation::base::CFTypeRef,
        ) -> i32;
        fn AXValueGetValue(
            value: core_foundation::base::CFTypeRef,
            the_type: u32,
            value_ptr: *mut std::ffi::c_void,
        ) -> u8;
        fn AXValueCreate(
            the_type: u32,
            value_ptr: *const std::ffi::c_void,
        ) -> core_foundation::base::CFTypeRef;
        fn CFRelease(cf: *const std::ffi::c_void);
        fn CFEqual(a: *const std::ffi::c_void, b: *const std::ffi::c_void) -> u8;
        fn CFBooleanGetValue(boolean: core_foundation::base::CFTypeRef) -> u8;
        fn CFArrayGetCount(arr: *const std::ffi::c_void) -> isize;
        fn CFArrayGetValueAtIndex(
            arr: *const std::ffi::c_void,
            idx: isize,
        ) -> *const std::ffi::c_void;
    }

    /// Gather "stranded" windows — ones sitting entirely off the display the RDP
    /// client sees — back onto that display. Under `--capture-primary` (and the
    /// other headless modes) a window opened on the physical panel keeps its old
    /// global coordinates, which fall outside the virtual display's region, so
    /// it's invisible/unclickable over RDP. Triggered on demand by Ctrl+Option+G,
    /// this moves every such window's top-left just inside the target display.
    /// Windows that (even partly) overlap the target are left untouched,
    /// so a window the user positioned on the virtual display is never disturbed;
    /// only *regular* (Dock) GUI apps are considered, so menu-extras / system
    /// panels are never yanked around. Best-effort AX — macrdp already holds the
    /// Accessibility grant (for `CGEventPost`), so no extra permission/prompt.
    /// Returns how many windows were moved. Triggered on demand by the
    /// Ctrl+Option+G hotkey (see `try_symbolic_hotkey`).
    fn gather_windows_onto_display(display_id: u32) -> usize {
        // CFRelease + CGPoint are already in scope (the mod-level extern block /
        // `use` above); importing them here would shadow.
        use core_foundation::base::{CFTypeRef, TCFType};
        use core_foundation::string::CFString;
        use std::ffi::c_void;
        use std::ptr;

        // AXValue type tags (AXValue.h): CGPoint = 1, CGSize = 2.
        const K_AX_VALUE_CGPOINT_TYPE: u32 = 1;
        const K_AX_VALUE_CGSIZE_TYPE: u32 = 2;

        // Target rect in global top-left-origin coords (the display's live bounds).
        let b = CGDisplay::new(display_id).bounds();
        let (tx, ty, tw, th) = (b.origin.x, b.origin.y, b.size.width, b.size.height);
        if tw <= 0.0 || th <= 0.0 {
            return 0;
        }
        // Land stranded windows a little inside the top-left so the title bar is
        // grabbable.
        let (nx, ny) = (tx + 40.0, ty + 40.0);

        // Read an AXValue-typed attribute (AXPosition/AXSize) as a coordinate pair.
        let read_pair = |win: *const c_void, attr: &CFString, tag: u32| -> Option<(f64, f64)> {
            unsafe {
                let mut v: CFTypeRef = ptr::null();
                if AXUIElementCopyAttributeValue(
                    win as *mut c_void,
                    attr.as_concrete_TypeRef().cast(),
                    &mut v,
                ) != AX_ERROR_SUCCESS
                    || v.is_null()
                {
                    return None;
                }
                let mut pair = [0f64; 2];
                let ok = AXValueGetValue(v, tag, pair.as_mut_ptr().cast());
                CFRelease(v.cast());
                (ok != 0).then_some((pair[0], pair[1]))
            }
        };

        let pos_attr = CFString::from_static_string("AXPosition");
        let size_attr = CFString::from_static_string("AXSize");
        let windows_attr = CFString::from_static_string("AXWindows");

        let mut moved = 0usize;
        for pid in list_all_pids() {
            // Only regular (Dock) GUI apps — skip agents/daemons/accessory apps.
            let is_regular = unsafe {
                NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
                    .map(|a| a.activationPolicy() == NSApplicationActivationPolicy::Regular)
                    .unwrap_or(false)
            };
            if !is_regular {
                continue;
            }
            unsafe {
                let app = AXUIElementCreateApplication(pid);
                if app.is_null() {
                    continue;
                }
                let mut arr: CFTypeRef = ptr::null();
                if AXUIElementCopyAttributeValue(
                    app,
                    windows_attr.as_concrete_TypeRef().cast(),
                    &mut arr,
                ) == AX_ERROR_SUCCESS
                    && !arr.is_null()
                {
                    let n = CFArrayGetCount(arr.cast());
                    for i in 0..n {
                        // Array elements are borrowed (not retained).
                        let win = CFArrayGetValueAtIndex(arr.cast(), i);
                        if win.is_null() {
                            continue;
                        }
                        let Some((px, py)) = read_pair(win, &pos_attr, K_AX_VALUE_CGPOINT_TYPE)
                        else {
                            continue;
                        };
                        // Default a missing size to a point, so a window still
                        // counts as stranded by its origin alone.
                        let (sw, sh) = read_pair(win, &size_attr, K_AX_VALUE_CGSIZE_TYPE)
                            .unwrap_or((1.0, 1.0));
                        // Overlaps the target display at all? Leave it be.
                        let overlaps = px < tx + tw && px + sw > tx && py < ty + th && py + sh > ty;
                        if overlaps {
                            continue;
                        }
                        // Stranded — move its top-left onto the target display.
                        let np = CGPoint::new(nx, ny);
                        let val =
                            AXValueCreate(K_AX_VALUE_CGPOINT_TYPE, (&np as *const CGPoint).cast());
                        if !val.is_null() {
                            if AXUIElementSetAttributeValue(
                                win as *mut c_void,
                                pos_attr.as_concrete_TypeRef().cast(),
                                val,
                            ) == AX_ERROR_SUCCESS
                            {
                                moved += 1;
                            }
                            CFRelease(val.cast());
                        }
                    }
                    CFRelease(arr.cast());
                }
                CFRelease(app.cast());
            }
        }
        moved
    }

    /// Activate the target app via AX (kAXFrontmost) AND explicitly
    /// raise its focused window. The two-step gesture matters for apps
    /// with multiple windows in one process (the canonical case is two
    /// VSCode projects opened via File→New Window — same pid, two
    /// windows): setting kAXFrontmost activates the *process*, but
    /// macOS picks which window pops up, and it doesn't always pick
    /// the one the user was last working in. Following up with an
    /// AXRaise on `AXFocusedWindow` pins the result to the window AX
    /// tracks as focused — i.e. the one the user actually touched
    /// last. Without this the user perceives the cycle as "switching
    /// between two VSCodes" because the cycle keeps activating the
    /// same app but a different window pops up each time.
    /// Re-assert a just-un-minimized app to the front after its genie animation
    /// finishes (~0.4 s). Setting AXMain/AXRaise synchronously right after
    /// AXMinimized=false races the animation and can leave the window behind
    /// others; a delayed second raise lands reliably. Runs on a detached thread so
    /// it never blocks the input path; by then the window is no longer minimized,
    /// so the re-call takes the normal raise+AXMain path. Only scheduled when we
    /// actually un-minimized something, so it won't yank focus otherwise.
    fn schedule_refront(pid: libc::pid_t) {
        std::thread::Builder::new()
            .name("macrdp-refront".into())
            .spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(420));
                ax_make_frontmost(pid, false, false);
            })
            .ok();
    }

    fn ax_make_frontmost(pid: libc::pid_t, unminimize: bool, reopen_if_windowless: bool) -> i32 {
        use core_foundation::base::{CFTypeRef, TCFType};
        use core_foundation::boolean::CFBoolean;
        use core_foundation::string::CFString;
        use std::ffi::c_void;
        use std::ptr;

        let app_ref = unsafe { AXUIElementCreateApplication(pid) };
        if app_ref.is_null() {
            return AX_ERROR_ILLEGAL_ARGUMENT;
        }

        // Native Cmd+Tab unhides a hidden (Cmd+H) app when you switch to it.
        // AXFrontmost activates the process/menu bar but does NOT surface a
        // window of a hidden app, so it'd appear to "not come forward." Unhide
        // first; no-op when not hidden. Also grab the bundle id for the
        // windowless-reopen fallback below.
        let mut bundle_id: Option<String> = None;
        unsafe {
            if let Some(running) =
                NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
            {
                if running.isHidden() {
                    running.unhide();
                }
                bundle_id = running.bundleIdentifier().map(|s| s.to_string());
            }
        }

        let frontmost_attr = CFString::from_static_string("AXFrontmost");
        let true_val = CFBoolean::true_value();
        let frontmost_err = unsafe {
            AXUIElementSetAttributeValue(
                app_ref,
                frontmost_attr.as_concrete_TypeRef().cast(),
                true_val.as_CFTypeRef() as CFTypeRef,
            )
        };

        // Raise + main a window of the app. `kAXFrontmost` activates the
        // *process* (menu bar), but a window only surfaces if we raise one —
        // and `AXFocusedWindow` is **null for an app the user hasn't interacted
        // with on this Space** (the common case right after `--detach-primary`
        // evacuates everyone onto the virtual display). When that happens,
        // `kAXFrontmost` succeeds but nothing visibly comes forward, so the
        // cycle appears to "only switch between the 2 recently-touched apps"
        // until a real mouse click populates each app's focused window.
        //
        // Fix: fall back AXFocusedWindow -> AXMainWindow -> AXWindows[0], then
        // both AXRaise it AND set AXMain=true. AXMain is what actually moves
        // window stacking for apps where AXRaise alone is a no-op (Electron —
        // VSCode/Slack/etc.). All best-effort: the caller only cares that the
        // process was activated.
        // Restore a minimized window before raising — AXRaise does NOT
        // de-miniaturize, so without this, landing on an all-minimized app shows
        // nothing. We un-minimize the landed window when EITHER --unminimize-on-
        // switch is set (which also un-minimizes per cycle step) OR this is the
        // commit/landing call (`reopen_if_windowless`): the app you release on
        // should always surface — consistent with reopening a windowless app on
        // landing. Per intermediate cycle steps we still leave minimized windows
        // alone (no flag), so apps cycled *through* don't flicker open.
        // AXMinimized=false is idempotent on a non-minimized window.
        let effective_unminimize = unminimize || reopen_if_windowless;
        // Set when we actually de-miniaturize a window: un-minimize is an async
        // (genie) animation, so the AXRaise/AXMain we do immediately afterward
        // races it and the window can land BEHIND others. We re-assert front once
        // the animation has settled (see schedule_refront below).
        let did_unminimize = std::cell::Cell::new(false);
        let min_attr = CFString::from_static_string("AXMinimized");
        let raise_and_main = |win: *mut c_void| unsafe {
            // Is this window minimized?
            let mut mv: CFTypeRef = ptr::null();
            let is_min =
                AXUIElementCopyAttributeValue(win, min_attr.as_concrete_TypeRef().cast(), &mut mv)
                    == AX_ERROR_SUCCESS
                    && !mv.is_null()
                    && CFBooleanGetValue(mv) != 0;
            if !mv.is_null() {
                CFRelease(mv.cast());
            }
            if is_min {
                // On intermediate cycle steps, leave a minimized window alone
                // (no flicker for apps merely cycled through).
                if !effective_unminimize {
                    return;
                }
                // De-miniaturize, then fall through to the same raise + AXMain as a
                // normal window. Un-minimizing alone (or AXRaise alone) leaves the
                // window BEHIND others on some apps (e.g. Calendar) — AXMain=true is
                // what actually moves it to the front of the stack.
                let false_val = CFBoolean::false_value();
                AXUIElementSetAttributeValue(
                    win,
                    min_attr.as_concrete_TypeRef().cast(),
                    false_val.as_CFTypeRef() as CFTypeRef,
                );
                did_unminimize.set(true);
            }
            let raise = CFString::from_static_string("AXRaise");
            AXUIElementPerformAction(win, raise.as_concrete_TypeRef().cast());
            let main_attr = CFString::from_static_string("AXMain");
            AXUIElementSetAttributeValue(
                win,
                main_attr.as_concrete_TypeRef().cast(),
                true_val.as_CFTypeRef() as CFTypeRef,
            );
        };

        let mut raised_via: &str = "none";
        // Single-window attributes first (own a +1 retain → release after use).
        for attr in ["AXFocusedWindow", "AXMainWindow"] {
            let a = CFString::new(attr);
            let mut win: CFTypeRef = ptr::null();
            let err = unsafe {
                AXUIElementCopyAttributeValue(app_ref, a.as_concrete_TypeRef().cast(), &mut win)
            };
            if err == AX_ERROR_SUCCESS && !win.is_null() {
                raise_and_main(win as *mut c_void);
                unsafe { CFRelease(win.cast()) };
                raised_via = attr;
                break;
            }
        }
        // Fall back to the first window in AXWindows. Array elements are
        // borrowed (not retained), so raise while the array is still alive.
        if raised_via == "none" {
            let a = CFString::new("AXWindows");
            let mut arr: CFTypeRef = ptr::null();
            let err = unsafe {
                AXUIElementCopyAttributeValue(app_ref, a.as_concrete_TypeRef().cast(), &mut arr)
            };
            if err == AX_ERROR_SUCCESS && !arr.is_null() {
                if unsafe { CFArrayGetCount(arr.cast()) } > 0 {
                    let w = unsafe { CFArrayGetValueAtIndex(arr.cast(), 0) };
                    if !w.is_null() {
                        raise_and_main(w as *mut c_void);
                        raised_via = "AXWindows[0]";
                    }
                }
                unsafe { CFRelease(arr.cast()) };
            }
        }
        debug!(pid, raised_via, "ax_make_frontmost: window raise");

        // If we de-miniaturized the landed window, re-assert front after the
        // un-minimize animation settles — the immediate raise above races the
        // genie animation and can leave the window behind others (Calendar).
        if did_unminimize.get() && reopen_if_windowless {
            schedule_refront(pid);
        }

        // Running app with NO window (Notes/Calendar/Mail et al. with their
        // window closed): AXFrontmost activated the menu bar but there was
        // nothing to raise, so the app appears "not to come forward." Trigger the
        // app's reopen via LaunchServices (like clicking its Dock icon / what
        // native Cmd+Tab does), which creates a window. Gated to the landing app
        // (`reopen_if_windowless`, set only on commit) so apps merely *cycled
        // through* don't pop windows; only fires when no window was found, so
        // apps that already surfaced one are untouched. `open` re-launches a
        // since-died bundle, but the user is switching *to* it, so that's fine.
        if raised_via == "none" && reopen_if_windowless {
            if let Some(b) = &bundle_id {
                let _ = std::process::Command::new("/usr/bin/open")
                    .args(["-b", b])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn();
                debug!(pid, bundle = %b, "ax_make_frontmost: reopen windowless app via open -b");
            }
        }

        unsafe { CFRelease(app_ref as *const c_void) };
        frontmost_err
    }

    /// Cycle through windows of the currently-frontmost app, the way
    /// native Cmd+` does. Pulls the app's window list via
    /// `kAXWindowsAttribute`, locates `kAXFocusedWindow` in that list,
    /// and AXRaises the next (or previous) one. Returns `false` if the
    /// app has fewer than two windows so the caller can fall back to a
    /// no-op rather than re-raising the same window.
    fn ax_cycle_windows_of_front(reverse: bool) -> bool {
        use core_foundation::base::{CFTypeRef, TCFType};
        use core_foundation::boolean::CFBoolean;
        use core_foundation::string::CFString;
        use std::ffi::c_void;
        use std::ptr;

        // Source-of-truth for "what app is the user actually in":
        // delegate to `effective_front_pid`, which reconciles
        // `NSWorkspace.frontmostApplication` against the
        // LAST_AX_ACTIVATED_PID / WORKSPACE_LIE_FRONT tracking that
        // cycle_apps maintains. Without this reconciliation, Cmd+`
        // would key off the stale workspace value and try to cycle
        // windows of whatever app we *used to* be in, not the one we
        // just landed on via Cmd+Tab.
        let pid = unsafe {
            let ws = objc2_app_kit::NSWorkspace::sharedWorkspace();
            let workspace_pid = match ws.frontmostApplication() {
                Some(app) => app.processIdentifier(),
                None => return false,
            };
            effective_front_pid(workspace_pid, |p| libc::kill(p, 0) == 0)
        };
        let app_ref = unsafe { AXUIElementCreateApplication(pid) };
        if app_ref.is_null() {
            debug!(pid, "ax_cycle_windows_of_front: null app_ref (bad pid)");
            return false;
        }

        let windows_attr = CFString::from_static_string("AXWindows");
        let mut windows: CFTypeRef = ptr::null();
        let copy_err = unsafe {
            AXUIElementCopyAttributeValue(
                app_ref,
                windows_attr.as_concrete_TypeRef().cast(),
                &mut windows,
            )
        };
        if copy_err != AX_ERROR_SUCCESS || windows.is_null() {
            debug!(
                pid,
                copy_err,
                windows_null = windows.is_null(),
                "ax_cycle_windows_of_front: AXWindows copy failed"
            );
            unsafe { CFRelease(app_ref as *const c_void) };
            return false;
        }
        let count = unsafe { CFArrayGetCount(windows.cast()) };
        if count < 2 {
            debug!(
                pid,
                count, "ax_cycle_windows_of_front: app has <2 windows — nothing to cycle"
            );
            unsafe {
                CFRelease(windows.cast());
                CFRelease(app_ref as *const c_void);
            }
            return false;
        }

        // Find the focused window's index in the array. If we can't
        // resolve it, fall back to position 0 — pressing Cmd+` ought to
        // do *something* visible.
        let focused_attr = CFString::from_static_string("AXFocusedWindow");
        let mut focused: CFTypeRef = ptr::null();
        let _ = unsafe {
            AXUIElementCopyAttributeValue(
                app_ref,
                focused_attr.as_concrete_TypeRef().cast(),
                &mut focused,
            )
        };
        let mut focused_idx: isize = 0;
        if !focused.is_null() {
            for i in 0..count {
                let w = unsafe { CFArrayGetValueAtIndex(windows.cast(), i) };
                if unsafe { CFEqual(w, focused) } != 0 {
                    focused_idx = i;
                    break;
                }
            }
            unsafe { CFRelease(focused.cast()) };
        }
        let next_idx = if reverse {
            (focused_idx + count - 1) % count
        } else {
            (focused_idx + 1) % count
        };
        let next_window = unsafe { CFArrayGetValueAtIndex(windows.cast(), next_idx) };
        // Multi-strategy window raise. Native Cocoa apps (Terminal,
        // Finder) honor AXRaise on a window — that alone is enough.
        // Electron apps (VSCode, Slack, Discord) expose AXRaise but
        // the implementation is a no-op on most versions; for those
        // we need to:
        //   - Set AXMain=true on the target window (Electron's window
        //     controller activates the window when this transitions).
        //   - Set the app's AXMainWindow attribute to point at the
        //     target window (canonical "make this the main window"
        //     gesture that some AX bridges only honor at the app level).
        // We apply all three and log which succeeded so it's obvious
        // from the trace which path actually moved the window.
        let raise_action = CFString::from_static_string("AXRaise");
        let main_attr = CFString::from_static_string("AXMain");
        let main_window_attr = CFString::from_static_string("AXMainWindow");
        let true_val = CFBoolean::true_value();
        let raise_err = unsafe {
            AXUIElementPerformAction(
                next_window as *mut c_void,
                raise_action.as_concrete_TypeRef().cast(),
            )
        };
        let set_main_err = unsafe {
            AXUIElementSetAttributeValue(
                next_window as *mut c_void,
                main_attr.as_concrete_TypeRef().cast(),
                true_val.as_CFTypeRef() as CFTypeRef,
            )
        };
        let set_main_window_err = unsafe {
            AXUIElementSetAttributeValue(
                app_ref,
                main_window_attr.as_concrete_TypeRef().cast(),
                next_window.cast(),
            )
        };
        debug!(
            pid,
            count,
            focused_idx,
            next_idx,
            raise_err,
            set_main_err,
            set_main_window_err,
            reverse,
            "ax_cycle_windows_of_front"
        );

        unsafe {
            CFRelease(windows.cast());
            CFRelease(app_ref as *const c_void);
        }
        raise_err == AX_ERROR_SUCCESS
    }

    /// Cmd+Space → invoke Spotlight. WindowServer's symbolic-hotkey
    /// dispatcher doesn't fire for our CGEvent-posted Cmd+Space, and
    /// AppleScript's `key code … using command down` route goes through
    /// the same dispatcher so it doesn't help on macOS versions where
    /// the dispatcher rejects synthesized events (confirmed empirically:
    /// osascript exits cleanly with status 0 but the search UI never
    /// opens).
    ///
    /// Working path: AX-press the Spotlight menu-bar icon directly.
    /// We already hold the Accessibility TCC grant (required for
    /// CGEventPost), so no extra permission needed. Falls back to the
    /// osascript route if the menu-bar press fails — and if even that
    /// silently no-ops, the user's last resort is to rebind Spotlight
    /// in System Settings → Keyboard → Shortcuts (a custom binding
    /// goes through the normal app-shortcut path our CGEventPost
    /// handles fine).
    fn invoke_spotlight() {
        debug!("invoke_spotlight: trying AX menu-bar press");
        if ax_press_spotlight_menu_extra() {
            debug!("invoke_spotlight: AX menu-bar press ok");
            return;
        }
        debug!("invoke_spotlight: AX menu-bar press failed, falling back to osascript");
        invoke_spotlight_via_osascript();
    }

    /// Open Spotlight by synthesizing a real mouse click on its
    /// menu-bar magnifier icon. We tried AXPress on the icon first —
    /// it returned `kAXErrorSuccess` but didn't actually open the
    /// search UI on the macOS versions we tested, so the "press"
    /// action seems to be a no-op for menu extras on those releases.
    ///
    /// Mouse-click route: read the icon's screen-space AXPosition +
    /// AXSize, compute its center, and CGEventPost a left
    /// mouseDown/mouseUp pair there. Mouse events don't go through
    /// the symbolic-hotkey dispatcher that rejects our synthesized
    /// Cmd+Space, so this drives normal click-handling and actually
    /// opens Spotlight.
    fn ax_press_spotlight_menu_extra() -> bool {
        use core_foundation::array::{CFArrayGetCount, CFArrayGetValueAtIndex};
        use core_foundation::base::{CFRelease, CFTypeRef, TCFType};
        use core_foundation::string::CFString;
        use core_graphics::geometry::{CGPoint, CGSize};
        use std::ffi::c_void;
        use std::ptr;

        // AXValue type tags from <HIServices/AXValue.h>.
        const K_AX_VALUE_CG_POINT_TYPE: u32 = 1;
        const K_AX_VALUE_CG_SIZE_TYPE: u32 = 2;

        let Some(pid) = pid_for_bundle("com.apple.Spotlight") else {
            debug!("ax_press_spotlight: com.apple.Spotlight not running");
            return false;
        };
        unsafe {
            let app_ref = AXUIElementCreateApplication(pid);
            if app_ref.is_null() {
                debug!(pid, "ax_press_spotlight: null app_ref");
                return false;
            }

            // Accessory apps put their menu-bar icon under
            // `AXExtrasMenuBar`. Its children are the AXMenuBarItem
            // entries — for Spotlight there's just one (the magnifier).
            let extras_attr = CFString::from_static_string("AXExtrasMenuBar");
            let mut extras: CFTypeRef = ptr::null();
            let copy_err = AXUIElementCopyAttributeValue(
                app_ref,
                extras_attr.as_concrete_TypeRef().cast(),
                &mut extras,
            );
            if copy_err != AX_ERROR_SUCCESS || extras.is_null() {
                debug!(
                    pid,
                    copy_err, "ax_press_spotlight: AXExtrasMenuBar copy failed"
                );
                CFRelease(app_ref as *const c_void);
                return false;
            }

            let children_attr = CFString::from_static_string("AXChildren");
            let mut children: CFTypeRef = ptr::null();
            let children_err = AXUIElementCopyAttributeValue(
                extras as *mut c_void,
                children_attr.as_concrete_TypeRef().cast(),
                &mut children,
            );
            if children_err != AX_ERROR_SUCCESS || children.is_null() {
                debug!(
                    pid,
                    children_err, "ax_press_spotlight: AXChildren copy failed"
                );
                CFRelease(extras);
                CFRelease(app_ref as *const c_void);
                return false;
            }

            let count = CFArrayGetCount(children.cast());
            debug!(pid, count, "ax_press_spotlight: extras menu-bar children");
            if count < 1 {
                CFRelease(children);
                CFRelease(extras);
                CFRelease(app_ref as *const c_void);
                return false;
            }

            let item = CFArrayGetValueAtIndex(children.cast(), 0) as *mut c_void;

            // Pull AXPosition (top-left of the icon, screen-space).
            let pos_attr = CFString::from_static_string("AXPosition");
            let mut pos_value: CFTypeRef = ptr::null();
            let pos_err = AXUIElementCopyAttributeValue(
                item,
                pos_attr.as_concrete_TypeRef().cast(),
                &mut pos_value,
            );
            if pos_err != AX_ERROR_SUCCESS || pos_value.is_null() {
                debug!(pid, pos_err, "ax_press_spotlight: AXPosition copy failed");
                CFRelease(children);
                CFRelease(extras);
                CFRelease(app_ref as *const c_void);
                return false;
            }
            let mut pos = CGPoint { x: 0.0, y: 0.0 };
            let pos_ok = AXValueGetValue(
                pos_value,
                K_AX_VALUE_CG_POINT_TYPE,
                &mut pos as *mut _ as *mut c_void,
            );
            CFRelease(pos_value);
            if pos_ok == 0 {
                debug!(pid, "ax_press_spotlight: AXValueGetValue(point) failed");
                CFRelease(children);
                CFRelease(extras);
                CFRelease(app_ref as *const c_void);
                return false;
            }

            // Pull AXSize so we can click the icon center.
            let size_attr = CFString::from_static_string("AXSize");
            let mut size_value: CFTypeRef = ptr::null();
            let size_err = AXUIElementCopyAttributeValue(
                item,
                size_attr.as_concrete_TypeRef().cast(),
                &mut size_value,
            );
            let size = if size_err == AX_ERROR_SUCCESS && !size_value.is_null() {
                let mut s = CGSize {
                    width: 0.0,
                    height: 0.0,
                };
                let ok = AXValueGetValue(
                    size_value,
                    K_AX_VALUE_CG_SIZE_TYPE,
                    &mut s as *mut _ as *mut c_void,
                );
                CFRelease(size_value);
                if ok != 0 {
                    s
                } else {
                    // Best-effort fallback — typical menu-bar icon is
                    // ~24x24 points.
                    CGSize {
                        width: 24.0,
                        height: 24.0,
                    }
                }
            } else {
                CGSize {
                    width: 24.0,
                    height: 24.0,
                }
            };

            let center = CGPoint {
                x: pos.x + size.width / 2.0,
                y: pos.y + size.height / 2.0,
            };
            debug!(
                pid,
                center_x = center.x,
                center_y = center.y,
                "ax_press_spotlight: clicking icon center"
            );

            CFRelease(children);
            CFRelease(extras);
            CFRelease(app_ref as *const c_void);

            // Build a CGEvent source — fresh for this click so we
            // don't tangle with the keyboard-input source state.
            let Ok(source) = CGEventSource::new(CGEventSourceStateID::CombinedSessionState) else {
                debug!("ax_press_spotlight: CGEventSource::new failed");
                return false;
            };
            // Move the cursor to the icon BEFORE the click. Without
            // this, menu-bar items often reject the synthesized click
            // (they hit-test against actual cursor position, not the
            // event's CG point). The mouseMoved + mouseDown + mouseUp
            // sequence mirrors what a real cursor-driven click sends.
            // Also clear modifier flags on every event — Cmd is still
            // held from the user's Cmd+Space chord, and a Cmd-modified
            // menu-bar click has different semantics than a plain one.
            let moved = CGEvent::new_mouse_event(
                source.clone(),
                CGEventType::MouseMoved,
                center,
                CGMouseButton::Left,
            );
            let down = CGEvent::new_mouse_event(
                source.clone(),
                CGEventType::LeftMouseDown,
                center,
                CGMouseButton::Left,
            );
            let up = CGEvent::new_mouse_event(
                source,
                CGEventType::LeftMouseUp,
                center,
                CGMouseButton::Left,
            );
            match (moved, down, up) {
                (Ok(m), Ok(d), Ok(u)) => {
                    m.set_flags(CGEventFlags::empty());
                    d.set_flags(CGEventFlags::empty());
                    u.set_flags(CGEventFlags::empty());
                    m.post(CGEventTapLocation::HID);
                    d.post(CGEventTapLocation::HID);
                    u.post(CGEventTapLocation::HID);
                    debug!("ax_press_spotlight: mouse move+click posted");
                    true
                }
                _ => {
                    debug!("ax_press_spotlight: CGEvent::new_mouse_event failed");
                    false
                }
            }
        }
    }

    fn invoke_spotlight_via_osascript() {
        debug!("invoke_spotlight: spawning osascript");
        let script = r#"tell application "System Events" to key code 49 using {command down}"#;
        let res = Command::new("/usr/bin/osascript")
            .arg("-e")
            .arg(script)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();
        match res {
            Ok(child) => {
                let child_pid = child.id();
                debug!(child_pid, "invoke_spotlight: osascript spawned");
                // Reap on a background thread and log the exit status +
                // any stderr so we can distinguish TCC denials, missing
                // automation grants, etc. from a real launch.
                std::thread::Builder::new()
                    .name("invoke-spotlight-wait".into())
                    .spawn(move || match child.wait_with_output() {
                        Ok(out) => {
                            let stdout = String::from_utf8_lossy(&out.stdout);
                            let stderr = String::from_utf8_lossy(&out.stderr);
                            debug!(
                                child_pid,
                                status = ?out.status,
                                stdout = %stdout.trim(),
                                stderr = %stderr.trim(),
                                "invoke_spotlight: osascript exited"
                            );
                        }
                        Err(e) => {
                            warn!(
                                child_pid,
                                error = %e,
                                "invoke_spotlight: wait on osascript failed"
                            );
                        }
                    })
                    .ok();
            }
            Err(e) => {
                warn!(error = %e, "invoke_spotlight: osascript spawn failed");
            }
        }
    }

    /// Run /usr/sbin/screencapture with the given args. `-x` = silent
    /// (no shutter sound), `-i` = interactive region. Output path is
    /// the default (~/Desktop) when not provided.
    fn screencapture(args: &[&str]) {
        // Default to the same path Cmd+Shift+3 produces natively.
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let Some(home) = home else {
            warn!("screencapture: $HOME unset");
            return;
        };
        let now = chrono_like_filename();
        let path = home.join("Desktop").join(format!("Screenshot {now}.png"));
        let res = Command::new("/usr/sbin/screencapture")
            .args(args)
            .arg(&path)
            .spawn();
        if let Err(e) = res {
            warn!(error = %e, "screencapture spawn failed");
        }
    }

    fn open_screenshot_app() {
        let res = Command::new("/usr/bin/open")
            .arg("-a")
            .arg("/System/Applications/Utilities/Screenshot.app")
            .spawn();
        if let Err(e) = res {
            warn!(error = %e, "open Screenshot.app failed");
        }
    }

    /// "2026-05-18 at 14.32.05" — matches macOS's native screenshot
    /// filename suffix closely enough that the result feels indigenous
    /// in Finder's sort order.
    fn chrono_like_filename() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // Local-time conversion without pulling in chrono: use libc.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        let t = secs as libc::time_t;
        unsafe {
            libc::localtime_r(&t, &mut tm);
        }
        format!(
            "{:04}-{:02}-{:02} at {:02}.{:02}.{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    }
}

/// Probe whether this process has Accessibility (AX) permission, prompting if
/// not. Without it, posted CGEvents are silently dropped by the WindowServer.
#[cfg(target_os = "macos")]
pub fn ensure_accessibility_access() -> bool {
    use core_foundation::base::TCFType;
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::string::CFString;
    use std::os::raw::c_void;

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXIsProcessTrustedWithOptions(options: *const c_void) -> bool;
    }

    // kAXTrustedCheckOptionPrompt — passing true makes macOS surface the
    // "Allow X to control this computer" prompt the first time we ask.
    let key = CFString::from_static_string("AXTrustedCheckOptionPrompt");
    let value = CFBoolean::true_value();
    let opts = CFDictionary::from_CFType_pairs(&[(key, value)]);
    unsafe { AXIsProcessTrustedWithOptions(opts.as_concrete_TypeRef().cast()) }
}

#[cfg(not(target_os = "macos"))]
pub fn ensure_accessibility_access() -> bool {
    true
}
