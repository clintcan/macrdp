//! Headless virtual display backed by a private `CGVirtualDisplay`
//! handle. The Mac treats it like an attached external monitor — same
//! displayID, same SCK enumeration, same bounds in the global coord
//! space — without changing the user's primary display.
//!
//! Public API is intentionally tiny and stable: `VirtualDisplay::new`
//! gives you a handle, and `display_id`/`origin_pts`/`size_pts` are the
//! three things capture / input / cursor need to address it. Everything
//! private API-related is in `private_api.rs`; nothing outside this
//! module should touch that file.

#[cfg(target_os = "macos")]
mod private_api;

#[cfg(target_os = "macos")]
pub use macos::{
    is_hidpi_virtual_display, screen_is_locked, shield_keeps_physical_main,
    take_detach_reenable_failed, CapturedPrimary, DetachedPrimary, PrimaryOverride,
    ShieldedPrimary, VirtualDisplay,
};

#[cfg(not(target_os = "macos"))]
pub use stub::{
    is_hidpi_virtual_display, screen_is_locked, take_detach_reenable_failed, CapturedPrimary,
    DetachedPrimary, PrimaryOverride, ShieldedPrimary, VirtualDisplay,
};

/// When the virtual display runs HiDPI (`--virtual-display-hidpi`).
#[derive(Clone, Debug)]
pub enum HidpiPolicy {
    /// Always 1:1 (the default).
    Off,
    /// Always HiDPI.
    On,
    /// Per connection, from the client's announced desktop scale factor
    /// (percent; the cell is filled by the server when a client connects).
    Auto(std::sync::Arc<std::sync::atomic::AtomicU32>),
}

impl HidpiPolicy {
    /// May the display ever run HiDPI? Decides its panel size and product id
    /// at creation, which can't change later.
    pub fn capable(&self) -> bool {
        !matches!(self, Self::Off)
    }

    /// Session pixels per UI point for the current (or next) connection:
    /// 2 lays the UI out at half the session size (a Retina client), 1 at
    /// the session size.
    pub fn ui_scale(&self) -> u32 {
        match self {
            Self::Off => 1,
            Self::On => 2,
            Self::Auto(scale) => {
                if scale_wants_hidpi(scale.load(std::sync::atomic::Ordering::Relaxed)) {
                    2
                } else {
                    1
                }
            }
        }
    }

    /// Record a scale factor the client announced mid-session, in a monitor
    /// layout (MS-RDPEDISP), and say whether it changes the UI scale. Windows
    /// App sends one when its window moves to a display with a different
    /// scale, e.g. from a Retina panel to a 1x external monitor. Only `Auto`
    /// follows it; `None` (the field absent or out of range) changes nothing.
    pub fn observe_client_scale(&self, desktop_scale_factor: Option<u32>) -> bool {
        let (Self::Auto(cell), Some(factor)) = (self, desktop_scale_factor) else {
            return false;
        };
        let before = self.ui_scale();
        cell.store(factor, std::sync::atomic::Ordering::Relaxed);
        self.ui_scale() != before
    }
}

/// Whether a client's desktop scale factor (percent) asks for a 2× display.
/// Windows App sends 200 with "Optimize for Retina displays" on and 100 with
/// it off; macOS only offers 2× scaling, so anything short of 200 stays 1:1.
pub fn scale_wants_hidpi(desktop_scale_factor: u32) -> bool {
    desktop_scale_factor >= 200
}

/// Physical panel size, in millimetres, that a virtual display reports for a
/// `width_px × height_px` framebuffer.
///
/// Without HiDPI this is purely cosmetic (a 27-inch 16:9 label in
/// Displays.app). With HiDPI it decides whether the display is actually
/// scaled: macOS offers both the 2× mode and a 1:1 duplicate at twice the
/// point size, and picks between them by pixel density, which it derives from
/// this size. It only picks the scaled mode inside a band — roughly 160 to
/// 450 PPI measured on macOS 26.6, with too dense falling back to 1:1 just
/// like too sparse. The size is fixed when the display is created while a
/// live resize changes the pixel count, so the width is a constant chosen to
/// keep common Retina session widths inside the band, not derived from the
/// initial size.
pub fn panel_size_mm(width_px: u32, height_px: u32, hidpi: bool) -> (f64, f64) {
    if !hidpi {
        return (600.0, 338.0);
    }
    let aspect = f64::from(height_px.max(1)) / f64::from(width_px.max(1));
    (HIDPI_PANEL_WIDTH_MM, HIDPI_PANEL_WIDTH_MM * aspect)
}

/// HiDPI panel width. On macOS 26.6 a panel this wide selected the scaled
/// mode for framebuffers from 1440 to 4096 px wide (about 160 to 450 PPI);
/// 1280 px fell back to 1:1. Outside that range the display runs 1:1 and
/// `VirtualDisplay` reports it.
const HIDPI_PANEL_WIDTH_MM: f64 = 233.0;

#[cfg(target_os = "macos")]
mod macos {
    use std::time::Duration;

    use anyhow::{anyhow, Context, Result};
    use core_graphics::display::{
        CGConfigureOption, CGDirectDisplayID, CGDisplay, CGDisplayCapture, CGDisplayRelease,
        CGError,
    };

    use super::private_api;

    // Brief pause between CGConfigure transactions so SkyLight has a moment
    // to propagate the previous commit before we open the next one. The
    // complete call returns as soon as the transaction is queued, not when
    // it's fully applied — back-to-back configures on the same display can
    // race and reject the second with CGError 1001.
    const TX_SETTLE: Duration = Duration::from_millis(200);

    /// Set true by `DetachedPrimary::drop` when its re-enable transaction is
    /// exhausted (the panel is left disabled). On macOS 26.x the CGS app-scoped
    /// display *disable* is not reversible in-process — only the process exiting
    /// (which closes our CGS connection) reverts it — so a disconnect that hits
    /// this leaves the built-in panel dark until macrdp restarts (#168). The
    /// overlay watcher reads-and-clears this after `drop`, and under launchd
    /// deliberately exits so KeepAlive restarts a fresh process (panel back in
    /// ~2-3 s) instead of leaving it dark indefinitely. Process-global because
    /// detach is single-instance and `Drop` can't return a value to the
    /// (guard-type-generic) watcher.
    static DETACH_REENABLE_FAILED: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);

    /// Display id of the HiDPI virtual display (0 = none). The capture path
    /// needs the display's backing pixel size, and `CGDisplayCopyDisplayMode`
    /// returns nothing for a virtual display, so the scale is recorded here
    /// when the display is created. Process-global because there is at most
    /// one macrdp virtual display per process.
    static HIDPI_DISPLAY_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    /// Whether `display_id` is macrdp's virtual display running HiDPI (2×
    /// pixels per point) via `--virtual-display-hidpi`.
    pub fn is_hidpi_virtual_display(display_id: u32) -> bool {
        display_id != 0 && HIDPI_DISPLAY_ID.load(std::sync::atomic::Ordering::Relaxed) == display_id
    }

    /// Read-and-clear the "detach left the physical panel stuck-disabled" flag
    /// (see [`DETACH_REENABLE_FAILED`]). Returns true at most once per failure.
    pub fn take_detach_reenable_failed() -> bool {
        DETACH_REENABLE_FAILED.swap(false, std::sync::atomic::Ordering::SeqCst)
    }

    /// Whether the local macOS session is currently screen-locked —
    /// `None` when the underlying private-API lookup itself failed (this
    /// is NOT the same as "not locked"; see the call sites in main.rs for
    /// why the distinction matters). Thin re-export of the quarantined
    /// private-API touch — see `private_api::screen_is_locked` for the
    /// mechanism and its maintenance/fallback notes.
    pub fn screen_is_locked() -> Option<bool> {
        private_api::screen_is_locked()
    }

    // CGGetOnlineDisplayList is a public CoreGraphics symbol but isn't in the
    // core-graphics crate. "Online" includes both active displays and any
    // online-but-currently-disabled displays — which is exactly the set we
    // need to look at when recovering from a prior install/drop that left a
    // physical display stuck disabled.
    fn online_displays() -> Result<Vec<CGDirectDisplayID>, CGError> {
        extern "C" {
            fn CGGetOnlineDisplayList(
                max_displays: u32,
                online_displays: *mut CGDirectDisplayID,
                display_count: *mut u32,
            ) -> CGError;
        }
        unsafe {
            let mut count: u32 = 0;
            let r = CGGetOnlineDisplayList(0, std::ptr::null_mut(), &mut count);
            if r != 0 {
                return Err(r);
            }
            let mut buf: Vec<CGDirectDisplayID> = vec![0; count as usize];
            let mut got: u32 = 0;
            let r = CGGetOnlineDisplayList(count, buf.as_mut_ptr(), &mut got);
            if r != 0 {
                return Err(r);
            }
            buf.truncate(got as usize);
            Ok(buf)
        }
    }

    pub struct VirtualDisplay {
        // RAII: dropped last so the CG handle is released after we've
        // logged anything we want to log about it.
        _handle: private_api::Handle,
        display_id: u32,
        origin_pts: (f64, f64),
        size_pts: (f64, f64),
        /// Created HiDPI (`--virtual-display-hidpi`): every mode is applied
        /// with `hiDPI=1` and only the point size changes. Whether macOS
        /// actually chose the scaled mode is recorded in `HIDPI_DISPLAY_ID`,
        /// which the capture path reads.
        hidpi: bool,
        /// Session pixels per UI point: 2 for a Retina client (the UI is laid
        /// out at half the session size), 1 for a 1x client (UI at the
        /// session size, captured down from the 2x backing). Always 1
        /// without `hidpi`.
        ui_scale: u32,
        /// Framebuffer size in pixels the display was last asked for. Kept
        /// separately from `size_pts × ui_scale` because an odd pixel count
        /// can't be halved exactly.
        mode_px: (u32, u32),
    }

    /// How long a reading that matches the pre-resize bounds is treated as
    /// "not applied yet" rather than as the outcome of the new mode.
    const SETTLE: Duration = Duration::from_millis(1000);

    /// Point size of the mode for a `px`-pixel session at `ui_scale`.
    fn mode_points(px: u32, ui_scale: u32) -> u32 {
        (px / ui_scale.max(1)).max(1)
    }

    /// What a mode request actually produced.
    enum Landed {
        /// The requested point size.
        Exact,
        /// HiDPI requested, but macOS chose the 1:1 duplicate at twice the
        /// point size (pixel density outside the band it accepts).
        OneToOne,
        /// HiDPI requested, but macOS chose the half-size HiDPI variant
        /// (W/2 points at W pixels) — see `resize`.
        HalfVariant,
    }

    impl VirtualDisplay {
        /// Allocate a headless display for a `width × height`-pixel session
        /// at `refresh_hz` Hz. Refresh rate is mostly cosmetic — the RDP
        /// server's frame cadence is independent — but a real value
        /// keeps `Displays.app` from looking weird.
        ///
        /// Returns an Err with a clear "private API unavailable" message
        /// if any of the underlying CG symbols / Obj-C classes can't be
        /// resolved on this macOS version. Caller should treat that as
        /// "this feature isn't usable here," not a fatal bug.
        ///
        /// With `hidpi` the display is a Retina-style 2× display for its
        /// whole life, and `ui_scale` picks its point size (`width /
        /// ui_scale`). It is never switched to 1:1: on the dense panel HiDPI
        /// needs, a display created 1:1 comes up at a mode macOS picks itself,
        /// and switching a live one HiDPI → 1:1 left the desktop composited
        /// into a corner of the framebuffer.
        pub fn new(
            width: u32,
            height: u32,
            refresh_hz: u32,
            hidpi: bool,
            ui_scale: u32,
        ) -> Result<Self> {
            let ui_scale = if hidpi { ui_scale.max(1) } else { 1 };
            let (mode_w, mode_h) = (mode_points(width, ui_scale), mode_points(height, ui_scale));
            let handle = private_api::create(
                mode_w,
                mode_h,
                refresh_hz,
                "macrdp",
                hidpi,
                hidpi,
                super::panel_size_mm(width, height, hidpi),
            )
            .context("creating virtual display")?;

            // CGDisplayBounds gives both the origin (in global point space)
            // and the point-space size. macOS auto-arranges new displays
            // off the right edge of the main panel by default; the origin
            // is what we add to mouse coords so CGEventPost lands events
            // on the vdisplay, not on the user's main screen.
            let id = handle.display_id();
            let bounds = CGDisplay::new(id).bounds();
            if bounds.size.width <= 0.0 || bounds.size.height <= 0.0 {
                return Err(anyhow!(
                    "virtual display registered (id={id}) but CGDisplayBounds \
                     returned a zero-size rect — the system hasn't finished \
                     activating it yet"
                ));
            }

            let mut vd = Self {
                _handle: handle,
                display_id: id,
                origin_pts: (bounds.origin.x, bounds.origin.y),
                size_pts: (bounds.size.width, bounds.size.height),
                hidpi,
                ui_scale,
                mode_px: (width, height),
            };
            vd.record_scale(mode_w);
            Ok(vd)
        }

        /// Work out which scale macOS actually gave the display after a mode
        /// was applied, from its point width. With HiDPI requested, macOS can
        /// still choose the 1:1 duplicate (twice the point size) when the
        /// panel density is out of range; the capture path has to know, so
        /// the shared registry follows what was chosen, not what was asked.
        fn record_scale(&mut self, mode_w: u32) {
            let scaled = self.hidpi && self.size_pts.0 as u32 == mode_w;
            if scaled {
                HIDPI_DISPLAY_ID.store(self.display_id, std::sync::atomic::Ordering::Relaxed);
                return;
            }
            let _ = HIDPI_DISPLAY_ID.compare_exchange(
                self.display_id,
                0,
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
            );
            if self.hidpi {
                tracing::warn!(
                    requested_points = ?(mode_w, self.size_pts.1),
                    bounds = ?self.size_pts,
                    session_px = ?self.mode_px,
                    "--virtual-display-hidpi: macOS did not select the scaled mode for this \
                     size (pixel density out of range); the display is running 1:1"
                );
            }
        }

        /// Framebuffer size in pixels the display was last set to.
        pub fn mode_px(&self) -> (u32, u32) {
            self.mode_px
        }

        /// Session pixels per UI point the display was last set to.
        pub fn ui_scale(&self) -> u32 {
            self.ui_scale
        }

        pub fn display_id(&self) -> u32 {
            self.display_id
        }

        pub fn origin_pts(&self) -> (f64, f64) {
            self.origin_pts
        }

        pub fn size_pts(&self) -> (f64, f64) {
            self.size_pts
        }

        /// Live-resize the display for a `width × height`-pixel session
        /// (client-driven session resize, or a new client). Re-applies
        /// settings with a single mode — the WindowServer treats it like a
        /// physical monitor changing resolution (windows re-layout, bounds
        /// update) — then polls `CGDisplayBounds` until the new size is
        /// visible. The display id is stable; the global-space origin can
        /// shift, so both cached values are refreshed. On a HiDPI display the
        /// mode is `width / ui_scale` points and stays HiDPI.
        pub fn resize(&mut self, width: u32, height: u32, ui_scale: u32) -> Result<()> {
            let ui_scale = if self.hidpi { ui_scale.max(1) } else { 1 };
            let (mode_w, mode_h) = (mode_points(width, ui_scale), mode_points(height, ui_scale));
            let mut landed = self.apply_and_wait(mode_w, mode_h)?;
            // For a HiDPI mode of W points macOS has two variants — W points at
            // 2W pixels, and W/2 points at W pixels — and can keep whichever
            // matches the current framebuffer's pixel count (a 1728-point
            // request after a 1728-pixel framebuffer came out as 864 points on
            // an idle test display). Going through a slightly different HiDPI
            // size first leaves neither variant matching, after which the
            // request lands correctly. Only done when the bad variant actually
            // shows up — the live agent display didn't do this when tested —
            // and always staying in HiDPI, never through 1:1.
            if matches!(landed, Landed::HalfVariant) {
                tracing::info!(
                    expected = ?(mode_w, mode_h),
                    "virtual display landed on the half-size HiDPI variant — stepping through \
                     another HiDPI size"
                );
                self.apply_and_wait(mode_w + 16, mode_h + 16)
                    .context("stepping through another HiDPI size")?;
                landed = self.apply_and_wait(mode_w, mode_h)?;
                if matches!(landed, Landed::HalfVariant) {
                    return Err(anyhow!(
                        "virtual display still on the half-size HiDPI variant for \
                         {mode_w}x{mode_h} points after stepping through another size"
                    ));
                }
            }
            self.mode_px = (width, height);
            self.ui_scale = ui_scale;
            self.record_scale(mode_w);
            Ok(())
        }

        /// Apply one mode and wait (up to 3 s) for the WindowServer to show
        /// it, reporting what it landed on. Refreshes the cached bounds.
        fn apply_and_wait(&mut self, mode_w: u32, mode_h: u32) -> Result<Landed> {
            let before = CGDisplay::new(self.display_id).bounds().size;
            let before = (before.width as u32, before.height as u32);
            self._handle
                .apply_mode(mode_w, mode_h, 60, self.hidpi)
                .context("re-applying virtual display mode")?;

            let start = std::time::Instant::now();
            let deadline = start + Duration::from_secs(3);
            loop {
                let bounds = CGDisplay::new(self.display_id).bounds();
                let (bw, bh) = (bounds.size.width as u32, bounds.size.height as u32);
                // A reading equal to the pre-apply bounds may just be the old
                // mode still showing, so the two "wrong mode" outcomes only
                // count once it differs from before, or after a settle.
                let settled = (bw, bh) != before || start.elapsed() >= SETTLE;
                let landed = if (bw, bh) == (mode_w, mode_h) {
                    Some(Landed::Exact)
                } else if self.hidpi && settled && (bw, bh) == (mode_w * 2, mode_h * 2) {
                    Some(Landed::OneToOne)
                } else if self.hidpi && settled && (bw, bh) == (mode_w / 2, mode_h / 2) {
                    Some(Landed::HalfVariant)
                } else {
                    None
                };
                if let Some(landed) = landed {
                    self.origin_pts = (bounds.origin.x, bounds.origin.y);
                    self.size_pts = (bounds.size.width, bounds.size.height);
                    return Ok(landed);
                }
                if std::time::Instant::now() >= deadline {
                    return Err(anyhow!(
                        "virtual display mode applied but CGDisplayBounds still reports \
                         {}x{} (expected {mode_w}x{mode_h}) after 3s — WindowServer hasn't \
                         picked up the new mode",
                        bounds.size.width,
                        bounds.size.height,
                    ));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }

        /// Re-assert the virtual display at the global origin `(0, 0)` so it
        /// stays the system MAIN display (the one holding the menu bar + Dock)
        /// after a live re-mode. A mode change (`applySettings`) can reset the
        /// display arrangement, knocking the vd off `(0, 0)` — and on the
        /// headless capture/detach path (where `CapturedPrimary`/`DetachedPrimary`
        /// placed the vd at `(0, 0)` precisely to make it main) that moves the
        /// menu bar + Dock back onto the now-blanked physical panel, so they
        /// vanish from the client's view. Headless-only helper: `capture.rs`
        /// calls it right after `resize` when a session tracker is active. No-op
        /// if the vd is already main at `(0, 0)`. Returns whether it had to move.
        pub fn reanchor_as_main(&mut self) -> Result<bool> {
            let vd = CGDisplay::new(self.display_id);
            let b = vd.bounds();
            let origin = (b.origin.x.round() as i32, b.origin.y.round() as i32);
            let main_id = CGDisplay::main().id;
            if origin == (0, 0) && main_id == self.display_id {
                tracing::debug!(
                    display_id = self.display_id,
                    "virtual display still main at (0,0) after re-mode — no re-anchor needed"
                );
                return Ok(false);
            }
            tracing::info!(
                display_id = self.display_id,
                ?origin,
                main_id,
                "virtual display drifted off (0,0)/main after re-mode — re-anchoring so the \
                 menu bar + Dock stay on it"
            );
            let config = vd
                .begin_configuration()
                .map_err(|e| anyhow!("CGBeginDisplayConfiguration (re-anchor): CGError {e}"))?;
            vd.configure_display_origin(&config, 0, 0).map_err(|e| {
                anyhow!("CGConfigureDisplayOrigin(vd, 0, 0) (re-anchor): CGError {e}")
            })?;
            vd.complete_configuration(&config, CGConfigureOption::ConfigureForAppOnly)
                .map_err(|e| anyhow!("CGCompleteDisplayConfiguration (re-anchor): CGError {e}"))?;
            self.origin_pts = (0.0, 0.0);
            Ok(true)
        }
    }

    /// Promotes a virtual (or any other secondary) display to be the
    /// system's primary — the one at global-coord origin `(0, 0)` that
    /// holds the menu bar and is where new app windows open.
    ///
    /// Implemented as a session-scoped `CGConfigureDisplayOrigin` swap:
    /// the target display is moved to `(0, 0)` and the old primary is
    /// shifted aside. Drop restores the original arrangement; if Drop
    /// doesn't run (signal-driven `std::process::exit`, crash), the
    /// session scope still reverts the layout when the user logs out.
    pub struct PrimaryOverride {
        primary_id: u32,
        primary_old_origin: (i32, i32),
        virtual_id: u32,
        virtual_old_origin: (i32, i32),
    }

    impl PrimaryOverride {
        /// Returns `Ok(Some(_))` after performing the swap, `Ok(None)`
        /// if the virtual display was already the system primary (the
        /// caller wanted that end state, so there's nothing to do and
        /// nothing to restore on shutdown), or `Err` on a real failure.
        pub fn install(virtual_display_id: u32) -> Result<Option<Self>> {
            let main = CGDisplay::main();
            let primary_id = main.id;
            if primary_id == virtual_display_id {
                // macOS auto-placed the vdisplay at (0,0) and made it the
                // menu-bar display — fairly common on first-time vdisplay
                // attach, since macOS persists external-monitor arrangement.
                return Ok(None);
            }

            let main_bounds = main.bounds();
            let primary_old_origin = (
                main_bounds.origin.x.round() as i32,
                main_bounds.origin.y.round() as i32,
            );

            let vd = CGDisplay::new(virtual_display_id);
            let vd_bounds = vd.bounds();
            let virtual_old_origin = (
                vd_bounds.origin.x.round() as i32,
                vd_bounds.origin.y.round() as i32,
            );
            // Width we'll shift the old primary by so it sits flush to the
            // right of the (now-primary) virtual display.
            let vd_width = vd_bounds.size.width.round() as i32;

            let config = main
                .begin_configuration()
                .map_err(|e| anyhow!("CGBeginDisplayConfiguration failed: CGError {e}"))?;
            vd.configure_display_origin(&config, 0, 0)
                .map_err(|e| anyhow!("CGConfigureDisplayOrigin(vd, 0, 0): CGError {e}"))?;
            main.configure_display_origin(&config, vd_width, 0)
                .map_err(|e| {
                    anyhow!("CGConfigureDisplayOrigin(primary, {vd_width}, 0): CGError {e}")
                })?;
            // ConfigureForSession means the swap reverts on user logout —
            // a free safety net if our explicit restore in Drop doesn't run.
            main.complete_configuration(&config, CGConfigureOption::ConfigureForSession)
                .map_err(|e| anyhow!("CGCompleteDisplayConfiguration: CGError {e}"))?;

            Ok(Some(Self {
                primary_id,
                primary_old_origin,
                virtual_id: virtual_display_id,
                virtual_old_origin,
            }))
        }
    }

    impl Drop for PrimaryOverride {
        fn drop(&mut self) {
            // Best-effort restore. Any error here is a "user's layout is now
            // wrong until logout"-level annoyance, not a panic. Don't fail.
            let main = CGDisplay::new(self.primary_id);
            let config = match main.begin_configuration() {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(
                        "could not begin display reconfig during restore (CGError {e}); \
                         layout will revert on logout"
                    );
                    return;
                }
            };
            let _ = main.configure_display_origin(
                &config,
                self.primary_old_origin.0,
                self.primary_old_origin.1,
            );
            let vd = CGDisplay::new(self.virtual_id);
            let _ = vd.configure_display_origin(
                &config,
                self.virtual_old_origin.0,
                self.virtual_old_origin.1,
            );
            if let Err(e) =
                main.complete_configuration(&config, CGConfigureOption::ConfigureForSession)
            {
                tracing::warn!(
                    "could not complete display reconfig during restore (CGError {e}); \
                     layout will revert on logout"
                );
            } else {
                tracing::info!("restored original display arrangement");
            }
        }
    }

    /// Makes the Mac headless except via the virtual display: every
    /// active physical display (built-in panel, any attached external
    /// monitors) is disabled at the WindowServer level — backlight
    /// off, no menu bar, no windows can be placed there, cursor can't
    /// cross onto them.
    ///
    /// Drop re-enables every detached display and restores its
    /// original origin. All CGConfigure transactions here use
    /// `ConfigureForAppOnly`: macOS automatically reverts every
    /// change when this process exits, so even a hard crash (signal,
    /// SIGKILL, panic past unwinding) leaves the panel re-enabled —
    /// no logout dance required. ForAppOnly also avoids the
    /// session-state accumulation that previously made the re-enable
    /// transaction synchronously reject with CGError 1001 on the
    /// second connect/disconnect cycle (and on first cycle of any
    /// run that inherited dirty session state from a prior failed
    /// run).
    pub struct DetachedPrimary {
        virtual_id: u32,
        virtual_old_origin: (i32, i32),
        /// Each physical display we disabled, with its origin at
        /// install time (so Drop can put it back where it was).
        detached: Vec<(u32, (i32, i32))>,
        enabler: private_api::DisplayEnabler,
    }

    impl DetachedPrimary {
        pub fn install(virtual_display_id: u32) -> Result<Self> {
            let enabler = private_api::DisplayEnabler::load()
                .context("loading CGSConfigureDisplayEnabled private symbol")?;

            // Capture vd's pre-install bounds.
            let vd = CGDisplay::new(virtual_display_id);
            let vd_bounds = vd.bounds();
            let virtual_old_origin = (
                vd_bounds.origin.x.round() as i32,
                vd_bounds.origin.y.round() as i32,
            );
            let vd_width = vd_bounds.size.width.round() as i32;

            // Discover the displays that need detaching. The normal case is
            // "everything active that isn't the virtual display." Recovery
            // case: a previous DetachedPrimary::drop on this session may have
            // failed to re-enable a physical display, leaving it online but
            // inactive — those won't show up in active_displays. Fall back to
            // online_displays so the reconnect path can pick up that stale
            // disabled set and retry, rather than wedging until logout.
            let actives = CGDisplay::active_displays()
                .map_err(|e| anyhow!("CGGetActiveDisplayList: CGError {e}"))?;
            let mut detached: Vec<(u32, (i32, i32))> = actives
                .iter()
                .copied()
                .filter(|id| *id != virtual_display_id)
                .map(|id| {
                    let b = CGDisplay::new(id).bounds();
                    (id, (b.origin.x.round() as i32, b.origin.y.round() as i32))
                })
                .collect();
            if detached.is_empty() {
                let online = online_displays()
                    .map_err(|e| anyhow!("CGGetOnlineDisplayList: CGError {e}"))?;
                let stuck: Vec<u32> = online
                    .into_iter()
                    .filter(|id| *id != virtual_display_id && !actives.contains(id))
                    .collect();
                if !stuck.is_empty() {
                    tracing::warn!(
                        stuck_count = stuck.len(),
                        "found online-but-inactive physical displays — a previous \
                         restore left them disabled; treating as already-detached"
                    );
                    // No reliable way to recover their original origin (the
                    // disabled state hides their pre-detach bounds). (0, 0)
                    // is the conventional "back to primary" target so Drop
                    // at least won't park them somewhere weirder.
                    detached = stuck.into_iter().map(|id| (id, (0, 0))).collect();
                }
            }
            if detached.is_empty() {
                return Err(anyhow!(
                    "no physical display to detach — the virtual display is the \
                     only active one already"
                ));
            }

            // Any CGDisplay handle works for begin/complete; the config is global.
            let any = CGDisplay::new(virtual_display_id);

            // Tx 1: only origin moves. Mixing the disable into this tx tripped
            // SkyLight badly — the move never reached the disabled display,
            // which then refused to re-enable on Drop (CGError 1001 at
            // complete-time). Keep each transaction single-purpose so the
            // display's state machine stays consistent.
            let config = any
                .begin_configuration()
                .map_err(|e| anyhow!("CGBeginDisplayConfiguration (move-tx): CGError {e}"))?;
            vd.configure_display_origin(&config, 0, 0)
                .map_err(|e| anyhow!("CGConfigureDisplayOrigin(vd, 0, 0): CGError {e}"))?;
            let mut x_off = vd_width;
            for (id, _) in &detached {
                let d = CGDisplay::new(*id);
                if d.is_active() {
                    d.configure_display_origin(&config, x_off, 0).map_err(|e| {
                        anyhow!("CGConfigureDisplayOrigin(physical {id}, {x_off}, 0): CGError {e}")
                    })?;
                    x_off += d.bounds().size.width.round() as i32;
                }
            }
            any.complete_configuration(&config, CGConfigureOption::ConfigureForAppOnly)
                .map_err(|e| anyhow!("CGCompleteDisplayConfiguration (move-tx): CGError {e}"))?;

            std::thread::sleep(TX_SETTLE);

            // Tx 2: only disables. By now the move has propagated, so disable
            // applies cleanly and the display's last-known position is the
            // shifted one — important because that's what SkyLight will
            // validate against on the eventual re-enable.
            let config = any
                .begin_configuration()
                .map_err(|e| anyhow!("CGBeginDisplayConfiguration (disable-tx): CGError {e}"))?;
            for (id, _) in &detached {
                if CGDisplay::new(*id).is_active() {
                    enabler.set(config, *id, false)?;
                }
            }
            any.complete_configuration(&config, CGConfigureOption::ConfigureForAppOnly)
                .map_err(|e| anyhow!("CGCompleteDisplayConfiguration (disable-tx): CGError {e}"))?;

            tracing::info!(
                detached_count = detached.len(),
                "physical displays detached"
            );

            Ok(Self {
                virtual_id: virtual_display_id,
                virtual_old_origin,
                detached,
                enabler,
            })
        }
    }

    impl Drop for DetachedPrimary {
        fn drop(&mut self) {
            tracing::info!(
                detached_count = self.detached.len(),
                "DetachedPrimary::drop running — re-enabling physical displays"
            );
            let any = CGDisplay::new(self.virtual_id);

            // Two transactions: re-enable in tx 1, reposition in tx 2.
            // Mixing both in a single transaction trips CGError 1001
            // (kCGErrorIllegalArgument) at complete-time — configure_display_origin
            // on a display that's transitioning disabled→enabled inside the
            // same transaction is rejected, which rolls back the enable too,
            // leaving the displays dark until process exit (ForAppOnly).
            //
            // Enable-tx is retried on failure as a defensive measure; with
            // ForAppOnly scope the per-cycle state stacking that historically
            // forced retries should no longer occur, but CG occasionally
            // returns transient 1001s during display state transitions so the
            // retry stays in as cheap insurance. If retries do exhaust, the
            // ForAppOnly scope guarantees the displays come back the moment
            // this process exits — no logout required.
            const MAX_ENABLE_ATTEMPTS: u32 = 3;
            const ENABLE_RETRY_BACKOFF: Duration = Duration::from_secs(2);
            let mut enable_ok = false;
            for attempt in 1..=MAX_ENABLE_ATTEMPTS {
                let config = match any.begin_configuration() {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(
                            attempt,
                            "could not begin enable-tx during detach restore (CGError {e})"
                        );
                        if attempt < MAX_ENABLE_ATTEMPTS {
                            std::thread::sleep(ENABLE_RETRY_BACKOFF);
                        }
                        continue;
                    }
                };
                for (id, _) in &self.detached {
                    if let Err(e) = self.enabler.set(config, *id, true) {
                        tracing::warn!(attempt, "could not queue enable for display {id} ({e})");
                    }
                }
                match any.complete_configuration(&config, CGConfigureOption::ConfigureForAppOnly) {
                    Ok(()) => {
                        if attempt > 1 {
                            tracing::info!(attempt, "enable-tx succeeded on retry");
                        }
                        enable_ok = true;
                        break;
                    }
                    Err(e) => {
                        tracing::warn!(attempt, "enable-tx complete failed (CGError {e})");
                        if attempt < MAX_ENABLE_ATTEMPTS {
                            std::thread::sleep(ENABLE_RETRY_BACKOFF);
                        }
                    }
                }
            }
            if !enable_ok {
                // The panel is left disabled. On macOS 26.x this is not
                // recoverable in-process (the CGS app-scoped disable only
                // reverts when our CGS connection closes, i.e. on process
                // exit) — so flag it for the overlay watcher, which under
                // launchd will exit to trigger a clean restart (#168).
                DETACH_REENABLE_FAILED.store(true, std::sync::atomic::Ordering::SeqCst);
                tracing::warn!(
                    "exhausted enable-tx retries; the built-in display is left \
                     disabled — it re-enables only when this process exits \
                     (ForAppOnly scope). Under launchd macrdp will restart to \
                     restore it (#168)."
                );
                return;
            }

            std::thread::sleep(TX_SETTLE);

            // Now the displays are live again; queue origin restores in a
            // second transaction. Best-effort — a wrong origin is far less
            // bad than a dark display.
            let config = match any.begin_configuration() {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(
                        "displays re-enabled but could not begin reposition-tx \
                         (CGError {e}); layout will be wrong until logout"
                    );
                    return;
                }
            };
            for (id, origin) in &self.detached {
                let _ = CGDisplay::new(*id).configure_display_origin(&config, origin.0, origin.1);
            }
            let _ = CGDisplay::new(self.virtual_id).configure_display_origin(
                &config,
                self.virtual_old_origin.0,
                self.virtual_old_origin.1,
            );
            if let Err(e) =
                any.complete_configuration(&config, CGConfigureOption::ConfigureForAppOnly)
            {
                tracing::warn!(
                    "displays re-enabled but reposition-tx failed (CGError {e}); \
                     layout will revert to original on process exit"
                );
            } else {
                tracing::info!(
                    detached_count = self.detached.len(),
                    "re-enabled detached displays and restored layout"
                );
            }
        }
    }

    // CGSetDisplayTransferByFormula / CGDisplayRestoreColorSyncSettings —
    // public CoreGraphics gamma-LUT API, not surfaced by the core_graphics
    // crate. Used to force each captured display to render pure black at
    // the output stage regardless of what's still being composited to it
    // (`CGDisplayCapture` alone hasn't actually blanked the panel since
    // ~macOS 10.10 — it only marks the display as exclusively owned).
    //
    // The transfer formula is `output = (max-min) * I^gamma + min`. With
    // min=max=0 the output is zero for every input, so the panel goes
    // black with the backlight still on. Gamma=1 keeps the math well-
    // defined; nothing about the gamma exponent matters when max=min.
    //
    // Per Apple's docs, gamma changes are scoped to the calling process —
    // the WindowServer reverts the LUT when our connection drops, so a
    // hard exit (SIGKILL, panic) auto-restores without a logout dance.
    extern "C" {
        fn CGSetDisplayTransferByFormula(
            display: CGDirectDisplayID,
            red_min: f32,
            red_max: f32,
            red_gamma: f32,
            green_min: f32,
            green_max: f32,
            green_gamma: f32,
            blue_min: f32,
            blue_max: f32,
            blue_gamma: f32,
        ) -> CGError;
        fn CGDisplayRestoreColorSyncSettings();
    }

    // NOTE: a CGDisplayRegisterReconfigurationCallback-based re-blank was tried
    // and REMOVED (2026-07-19). The vd live re-mode is a private CGVirtualDisplay
    // `applySettings`, which resets the physical panels' gamma but does NOT emit a
    // public CGDisplay reconfiguration notification — verified live: the callback
    // registered fine but never fired across many resizes. So the gamma re-assert
    // is driven from the capture path instead (an immediate one after the re-mode
    // + one after each post-resize window-gather sweep, which is the relayout that
    // actually re-resets the gamma ~700 ms in). See capture.rs `sync_virtual_display`.

    /// Alternative "headless while connected" mechanism that takes
    /// **exclusive capture** of every physical display via the public
    /// `CGDisplayCapture` API and forces each captured panel's gamma
    /// LUT to map every input intensity to black via
    /// `CGSetDisplayTransferByFormula(_, 0,0,1, 0,0,1, 0,0,1)`.
    ///
    /// Capture alone doesn't visually blank modern macOS displays —
    /// the "fill with black on capture" behaviour was a 10.x-era
    /// behaviour and is unreliable on contemporary releases. The
    /// gamma trick is the public-API way to force a panel to render
    /// pure black without dragging in AppKit / a run loop. Backlight
    /// stays on (the panel is still powered) but every pixel is
    /// black, and the capture token sinks any local input on the
    /// physical displays.
    ///
    /// Why not `DetachedPrimary`'s approach:
    /// - No `CGSConfigureDisplayEnabled` private symbol involved.
    /// - No `CGError 1001` window because there is no second
    ///   reconfigure transaction interleaved with the state change.
    /// - If the process dies hard (signal, panic, SIGKILL), macOS
    ///   automatically releases every display we captured AND
    ///   reverts the gamma LUT — both are process-scoped, no
    ///   logout required.
    pub struct CapturedPrimary {
        virtual_id: u32,
        virtual_old_origin: (i32, i32),
        /// Physical displays we hold a capture token for, with the
        /// pre-install origin so Drop can put them back.
        captured: Vec<(u32, (i32, i32))>,
    }

    impl CapturedPrimary {
        pub fn install(virtual_display_id: u32) -> Result<Self> {
            let vd = CGDisplay::new(virtual_display_id);
            let vd_bounds = vd.bounds();
            let virtual_old_origin = (
                vd_bounds.origin.x.round() as i32,
                vd_bounds.origin.y.round() as i32,
            );
            let vd_width = vd_bounds.size.width.round() as i32;

            let actives = CGDisplay::active_displays()
                .map_err(|e| anyhow!("CGGetActiveDisplayList: CGError {e}"))?;
            let targets: Vec<(u32, (i32, i32))> = actives
                .iter()
                .copied()
                .filter(|id| *id != virtual_display_id)
                .map(|id| {
                    let b = CGDisplay::new(id).bounds();
                    (id, (b.origin.x.round() as i32, b.origin.y.round() as i32))
                })
                .collect();
            if targets.is_empty() {
                return Err(anyhow!(
                    "no physical display to capture — the virtual display is \
                     the only active one already"
                ));
            }

            // Origin tx: vd → (0, 0), physicals shifted aside. Done
            // *before* capture so cursor coords resolved against the
            // vd's bounds line up with the RDP frame's (0, 0). After
            // capture the layout is frozen from our perspective; only
            // the vd is being composited to.
            //
            // ConfigureForSession (NOT ForAppOnly) is load-bearing for live
            // resize: ForAppOnly is process-scoped and never enters the
            // WindowServer's persisted arrangement, which therefore still says
            // "vd at its creation origin, physical is main" — so EVERY live
            // re-mode (`applySettings` on a client resize) re-derived the
            // arrangement from that store and snapped the vd off (0,0)/main
            // (confirmed in the log: `drifted off (0,0)/main after re-mode` on
            // each resize), yanking the menu bar + Dock back to the blanked
            // physical panel and re-stranding windows mid-gather. ForSession
            // persists vd@(0,0) into the session store, so a re-mode has
            // nothing to snap back to. Crash-safety is unchanged: the capture
            // tokens + gamma below are process-scoped regardless (SIGKILL
            // un-blanks the panels), and when the vd vanishes with a dead
            // process the remaining physical automatically becomes main at
            // (0,0) again (a lone display always anchors the arrangement);
            // logout clears the session store as a backstop.
            let any = CGDisplay::new(virtual_display_id);
            let config = any
                .begin_configuration()
                .map_err(|e| anyhow!("CGBeginDisplayConfiguration (move-tx): CGError {e}"))?;
            vd.configure_display_origin(&config, 0, 0)
                .map_err(|e| anyhow!("CGConfigureDisplayOrigin(vd, 0, 0): CGError {e}"))?;
            let mut x_off = vd_width;
            for (id, _) in &targets {
                let d = CGDisplay::new(*id);
                if d.is_active() {
                    d.configure_display_origin(&config, x_off, 0).map_err(|e| {
                        anyhow!("CGConfigureDisplayOrigin(physical {id}, {x_off}, 0): CGError {e}")
                    })?;
                    x_off += d.bounds().size.width.round() as i32;
                }
            }
            any.complete_configuration(&config, CGConfigureOption::ConfigureForSession)
                .map_err(|e| anyhow!("CGCompleteDisplayConfiguration (move-tx): CGError {e}"))?;

            std::thread::sleep(TX_SETTLE);

            // Capture each physical display. Track successes so a
            // partial failure rolls back cleanly instead of leaving
            // some panels captured and others live.
            let mut held: Vec<(u32, (i32, i32))> = Vec::with_capacity(targets.len());
            for (id, origin) in &targets {
                let err = unsafe { CGDisplayCapture(*id) };
                if err == 0 {
                    held.push((*id, *origin));
                } else {
                    for (rid, _) in &held {
                        let _ = unsafe { CGDisplayRelease(*rid) };
                    }
                    return Err(anyhow!(
                        "CGDisplayCapture(display={id}) returned CGError {err} \
                         — another process may already hold this display"
                    ));
                }
            }

            // Force each captured panel's gamma LUT to all-black so the
            // desktop the WindowServer is still compositing to it doesn't
            // visibly show. Best-effort per-display — a gamma-set
            // rejection means that one panel keeps showing the desktop,
            // but the capture is still in effect.
            for (id, _) in &held {
                let err = unsafe {
                    CGSetDisplayTransferByFormula(*id, 0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 1.0)
                };
                if err != 0 {
                    tracing::warn!(
                        "CGSetDisplayTransferByFormula(display={id}) returned \
                         CGError {err} — that panel will keep showing the \
                         desktop, but capture is in effect"
                    );
                }
            }

            tracing::info!(
                captured_count = held.len(),
                "physical displays captured + gamma-blanked"
            );

            // SECURITY: the blanked panel is NOT a lock, and while the capture
            // is engaged the Mac CANNOT be locked at all. Apple menu → Lock
            // Screen (and ⌃⌘Q) appear to work — no error, no feedback — but
            // `loginwindow` cannot draw onto a display this process holds via
            // CGDisplayCapture, so the lock never engages and the session stays
            // live behind the black panel. Experimentally isolated 2026-07-20
            // across ~340 samples / 3 independent signals; `caffeinate` was the
            // obvious suspect and is exonerated (killing it changes nothing).
            // A plain --virtual-display session is unaffected and locks fine.
            //
            // The failure is silent to the user standing at the machine, so we
            // say it loudly here at least once per engage.
            tracing::warn!(
                "SECURITY: while --capture-primary is engaged this Mac CANNOT be \
                 locked — Lock Screen silently does nothing, and the black panel \
                 is gamma trickery (a crash or kill restores a live, unlocked \
                 desktop). Treat the machine as physically unsecured until the \
                 last client disconnects. See docs/known-quirks.md."
            );

            Ok(Self {
                virtual_id: virtual_display_id,
                virtual_old_origin,
                captured: held,
            })
        }

        /// Re-apply the all-black gamma LUT to every captured panel. Needed
        /// after a live re-mode (`applySettings` when the client maximizes /
        /// moves to a different-resolution monitor): a display reconfiguration
        /// **resets the gamma tables**, so the blanking installed at `install`
        /// time is lost and the physical panel shows the desktop again. The
        /// capture tokens survive the re-mode (they're process-held), so only
        /// the gamma needs re-asserting. Called SYNCHRONOUSLY from the capture
        /// path right after the re-mode (same timing rule as `reanchor_as_main`
        /// — a few hundred ms late and the desktop has already flashed).
        /// Returns the number of panels that could not be re-blanked (0 = all ok).
        pub fn reassert_blanking(&self) -> usize {
            let mut failed = 0usize;
            for (id, _) in &self.captured {
                let err = unsafe {
                    CGSetDisplayTransferByFormula(*id, 0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 1.0)
                };
                if err != 0 {
                    failed += 1;
                    tracing::warn!(
                        "reassert_blanking: CGSetDisplayTransferByFormula(display={id}) \
                         returned CGError {err}"
                    );
                }
            }
            failed
        }
    }

    impl Drop for CapturedPrimary {
        fn drop(&mut self) {
            tracing::info!(
                captured_count = self.captured.len(),
                "CapturedPrimary::drop running — restoring gamma + releasing displays"
            );

            // Restore the gamma LUTs first so the panels become visible
            // again before we hand control back. CGDisplayRestoreColorSyncSettings
            // reverts every display whose LUT THIS process altered to
            // the user's ColorSync profile — it does not clobber gamma
            // changes made by other apps.
            unsafe { CGDisplayRestoreColorSyncSettings() };

            // Release the capture tokens. CGDisplayRelease is best-effort
            // from our perspective — any non-zero return just means we
            // never held it (process-scoped capture may have already
            // been auto-released).
            for (id, _) in &self.captured {
                let err = unsafe { CGDisplayRelease(*id) };
                if err != 0 {
                    tracing::warn!(
                        "CGDisplayRelease(display={id}) returned CGError {err} \
                         — display may already be released"
                    );
                }
            }

            std::thread::sleep(TX_SETTLE);

            // Best-effort restore of the pre-install origins.
            let any = CGDisplay::new(self.virtual_id);
            let config = match any.begin_configuration() {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(
                        "displays released but could not begin reposition-tx \
                         (CGError {e}); layout will revert on process exit"
                    );
                    return;
                }
            };
            for (id, origin) in &self.captured {
                let _ = CGDisplay::new(*id).configure_display_origin(&config, origin.0, origin.1);
            }
            let _ = CGDisplay::new(self.virtual_id).configure_display_origin(
                &config,
                self.virtual_old_origin.0,
                self.virtual_old_origin.1,
            );
            // ConfigureForSession (NOT ForAppOnly) — MUST match `install`'s
            // ConfigureForSession. `install` persists "vd@(0,0) is main" into the
            // WindowServer's session store; if this restore reverts the origins
            // only process-scoped (ForAppOnly), that store STILL says the vd is
            // main while the live arrangement puts the physical back at (0,0) —
            // and the Dock/menu bar (which follow the main display) then
            // *sometimes* follow the persisted vd off-screen on disconnect
            // ("the Dock disappears"). Persisting the restore updates the store to
            // "physical@(0,0) is main again", so it agrees with reality and the
            // Dock stays put. Crash-safety is unchanged: on SIGKILL/panic drop
            // never runs, the vd vanishes with the process, and the lone physical
            // auto-becomes main at (0,0); logout clears the session store.
            if let Err(e) =
                any.complete_configuration(&config, CGConfigureOption::ConfigureForSession)
            {
                tracing::warn!(
                    "displays released but reposition-tx failed (CGError {e}); \
                     layout will revert on process exit"
                );
            } else {
                tracing::info!(
                    captured_count = self.captured.len(),
                    "released captured displays and restored layout"
                );
            }
        }
    }

    /// Headless blanking via an opaque black **shield window** over each
    /// physical panel, drawn by the `macrdpshield` helper process.
    ///
    /// Third headless mode alongside [`DetachedPrimary`] and
    /// [`CapturedPrimary`]. The arrangement half is identical to
    /// `CapturedPrimary` — vd to `(0, 0)` as system main, physicals shifted
    /// aside, completed `ConfigureForSession` (see that type for why ForSession
    /// is load-bearing across live re-modes and on disconnect). What differs is
    /// how the panel is hidden:
    ///
    /// | | `CapturedPrimary` | `ShieldedPrimary` |
    /// |---|---|---|
    /// | hides the desktop with | all-black gamma LUT | an opaque black window |
    /// | survives a live re-mode | **no** — macOS resets gamma on every display reconfiguration, so it must be re-asserted, and a write *during* the reconfiguration does not stick (irreducible ~250 ms desktop flash) | **yes** — a window is not gamma; nothing to re-assert, no flash |
    /// | Mac can be locked | **no** — `loginwindow` cannot draw onto a display held by `CGDisplayCapture` | **yes** — no capture is taken |
    /// | local cursor | confined off the panel by the capture | **not confined** — see below |
    /// | crash safety | process-scoped; SIGKILL auto-reverts | helper self-exits when its parent dies |
    ///
    /// **Security trade-off, stated plainly.** Dropping `CGDisplayCapture` is
    /// what makes the machine lockable again, but capture was also what kept the
    /// local pointer off the panel. Under a shield, someone physically at the Mac
    /// can move the pointer — which is shared global state, so it yanks the
    /// remote user's cursor out of the virtual display. Their *clicks* are
    /// swallowed (the shield window takes mouse events), and their keystrokes go
    /// wherever focus already was, exactly as under capture (capture never
    /// blocked the keyboard). Net: this mode trades "a local person can disturb
    /// the pointer" for "the Mac can actually be locked", which is the better
    /// posture for an unattended machine — but it is a real trade, not a
    /// free win.
    ///
    /// **Fail-open by design.** If the helper cannot be reached, `install`
    /// fails and the mode refuses to engage rather than reporting success over a
    /// visible desktop. If the helper dies mid-session the panel becomes
    /// visible — the same end state as a SIGKILLed `CapturedPrimary`, whose
    /// gamma is likewise process-scoped.
    pub struct ShieldedPrimary {
        virtual_id: u32,
        virtual_old_origin: (i32, i32),
        /// Physical displays we shielded, with the pre-install origin so Drop
        /// can put them back.
        shielded: Vec<(u32, (i32, i32))>,
        /// Whether `install` moved the vd to `(0,0)` as main. `false` in the
        /// default keep-physical-main mode, in which case `Drop` has no
        /// arrangement to restore (it only lowers the shields).
        moved_arrangement: bool,
        /// Physical displays whose vd-mirror `install` broke (single-panel
        /// auto-mirror hardware). `Drop` re-mirrors them so the built-in panel
        /// shows the desktop again between sessions. Empty on hardware where
        /// the vd extends rather than mirrors.
        broke_mirror: Vec<u32>,
    }

    /// Whether `--shield-primary` should keep the PHYSICAL panel as the system
    /// main display instead of moving the vd to `(0, 0)` as main.
    ///
    /// This exists because making the vd main (as `CapturedPrimary` does, and as
    /// shield originally copied) puts the lock screen where nobody can see it:
    /// `loginwindow` draws on the main display, so on a shielded session ⌃⌘Q
    /// locked the Mac but rendered the password field on the *headless* vd, with
    /// the physical panel showing the live desktop (live-observed 2026-07-20).
    /// Keeping the physical panel main puts the lock screen back on the physical
    /// panel where it is visible (corroborated by the capture-off positive
    /// control, which also drew the lock on the physical main display).
    ///
    /// The trade-off: with the vd not main, the menu bar + Dock stay on the
    /// (shielded/black) physical panel rather than on the display the client
    /// sees — so the remote desktop loses them. You cannot have the lock visible
    /// AND the Dock on the vd; they are the same `(0,0)`/main bit. Default on
    /// (lock visibility wins); `MACRDP_SHIELD_KEEP_PHYSICAL_MAIN=0` restores the
    /// old vd-as-main behaviour for A/B comparison.
    pub fn shield_keeps_physical_main() -> bool {
        match std::env::var("MACRDP_SHIELD_KEEP_PHYSICAL_MAIN") {
            Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false") && !v.is_empty(),
            Err(_) => true,
        }
    }

    /// Re-mirror `panels` onto the vd — the undo of the `install` mirror-break
    /// on single-panel auto-mirror hardware. One function because it MUST run
    /// from three places with identical semantics: `Drop` (clean disconnect),
    /// and both of `install`'s post-mirror-break failure paths (SHOW-exhaust
    /// and a failed arrangement tx). The failure paths matter as much as Drop:
    /// they `return Err` before `Self` exists, so Drop never runs — without an
    /// explicit re-mirror there, the `ConfigureForAppOnly` break would persist
    /// for the PROCESS lifetime (macrdp keeps serving after a failed install),
    /// leaving the local user an un-mirrored, displaced, empty desktop panel.
    /// Re-mirroring also subsumes the displace-tx's origin change — a mirrored
    /// panel has no independent origin — so no separate restore is needed.
    /// Best-effort by contract (callers are teardown paths): failures are
    /// logged, and the ForAppOnly scope still self-reverts on process exit.
    fn remirror_onto_vd(vd_id: u32, panels: &[u32]) {
        if panels.is_empty() {
            return;
        }
        let vd = CGDisplay::new(vd_id);
        match vd.begin_configuration() {
            Ok(config) => {
                for id in panels {
                    let _ = CGDisplay::new(*id).configure_display_mirror_of_display(&config, &vd);
                }
                if let Err(e) =
                    vd.complete_configuration(&config, CGConfigureOption::ConfigureForAppOnly)
                {
                    tracing::warn!(
                        "re-mirror tx failed (CGError {e}); the built-in panel stays a \
                         separate display until the next connect or process exit"
                    );
                } else {
                    tracing::info!(
                        remirrored_count = panels.len(),
                        "restored the vd→physical mirror"
                    );
                }
            }
            Err(e) => tracing::warn!("could not begin re-mirror tx (CGError {e})"),
        }
    }

    impl ShieldedPrimary {
        pub fn install(virtual_display_id: u32) -> Result<Self> {
            let vd = CGDisplay::new(virtual_display_id);
            let vd_bounds = vd.bounds();
            let virtual_old_origin = (
                vd_bounds.origin.x.round() as i32,
                vd_bounds.origin.y.round() as i32,
            );
            let vd_width = vd_bounds.size.width.round() as i32;

            let actives = CGDisplay::active_displays()
                .map_err(|e| anyhow!("CGGetActiveDisplayList: CGError {e}"))?;
            let mut targets: Vec<(u32, (i32, i32))> = actives
                .iter()
                .copied()
                .filter(|id| *id != virtual_display_id)
                .map(|id| {
                    let b = CGDisplay::new(id).bounds();
                    (id, (b.origin.x.round() as i32, b.origin.y.round() as i32))
                })
                .collect();
            if targets.is_empty() {
                // No *active* physical display — but on single-panel hardware
                // creating the vd makes macOS mirror the built-in INTO the vd,
                // which drops the physical out of the active list (it reads
                // online-but-inactive) even though it is very much still lit and
                // showing the desktop. Without this fallback `install` fails with
                // "no physical display to shield" and the overlay watcher's error
                // arm only warns — so the session proceeds with the desktop fully
                // VISIBLE, silently defeating the whole point of the mode. Fall
                // back to CGGetOnlineDisplayList (which includes mirrored /
                // disabled panels) and shield those, exactly as
                // DetachedPrimary/CapturedPrimary already do. The origin is
                // unknowable while mirrored/disabled, so use (0, 0) — Drop only
                // repositions when it actually moved the arrangement, and the
                // default keep-physical-main path moves nothing.
                let online = online_displays()
                    .map_err(|e| anyhow!("CGGetOnlineDisplayList: CGError {e}"))?;
                let mirrored: Vec<u32> = online
                    .into_iter()
                    .filter(|id| *id != virtual_display_id && !actives.contains(id))
                    .collect();
                if !mirrored.is_empty() {
                    tracing::warn!(
                        mirrored_count = mirrored.len(),
                        "no ACTIVE physical display to shield, but found \
                         online-but-inactive panel(s) (mirrored into the virtual \
                         display, or left disabled by a prior session) — shielding \
                         those so the desktop is covered"
                    );
                    targets = mirrored.into_iter().map(|id| (id, (0, 0))).collect();
                }
            }
            // If STILL empty after the online-display fallback above, there is
            // no physical panel in ANY state (active, mirrored, or disabled) —
            // not merely inactive. This is the lid-closed case: a closed lid
            // removes the built-in panel from the display subsystem entirely
            // (unlike sleep/idle, which leaves it "online but inactive" and
            // caught by the fallback above). With no panel present at all,
            // there is nothing rendering a desktop for anyone to see, so
            // there's genuinely nothing to shield — proceed with `targets`
            // empty rather than failing. `install` naturally degrades to a
            // no-op shield (SHOW's `need` is 0, trivially satisfied; the
            // mirror-break/displace/arrangement blocks below are all gated on
            // a non-empty `targets`/`broke_mirror` so they no-op too), which
            // matters because failing here means the whole watcher's
            // `Ok(ovr) =>` branch — restore-windows and the connect-time
            // window gather — never runs, silently.
            if targets.is_empty() {
                tracing::info!(
                    "no physical display found in any state (not even online-but- \
                     inactive) — likely the lid is closed. Nothing is showing the \
                     desktop, so there's nothing to shield; proceeding with an \
                     empty shield."
                );
            }

            // Break any mirror a physical shares with the vd BEFORE shielding.
            // On single-panel hardware macOS mirrors the vd onto the built-in
            // panel, which is fatal to a window-based blank two ways: (1) the
            // mirror set collapses to ONE screen in `NSScreen.screens` (the vd),
            // so the helper — which shields everything in NSScreen minus the vd —
            // has nothing separate to cover; (2) even if it did, a mirrored
            // panel is the SAME framebuffer as the vd, so a black window can't
            // hide one without hiding what the client sees. Un-mirroring makes
            // the physical its own independent display the helper can cover while
            // the vd stays visible to the client. `ConfigureForAppOnly` keeps it
            // process-scoped (SIGKILL/crash auto-reverts, like capture's gamma
            // and detach's disable); `Drop` restores the mirror on a clean
            // disconnect. A no-op on hardware where the vd extends rather than
            // mirrors (no target reports `mirrors_display() == vd`).
            let broke_mirror: Vec<u32> = targets
                .iter()
                .map(|(id, _)| *id)
                .filter(|id| CGDisplay::new(*id).mirrors_display() == virtual_display_id)
                .collect();
            // Set when the displace-tx below succeeds; the window-gather that
            // depends on it is deferred until install is KNOWN to succeed, so a
            // failed install can't first sweep the user's windows onto a vd
            // they cannot see locally.
            let mut displaced = false;
            if !broke_mirror.is_empty() {
                let any = CGDisplay::new(virtual_display_id);
                // kCGNullDirectDisplay (id 0) as the mirror master = "mirror
                // nothing" = break the mirror.
                let null_master = CGDisplay::new(0);
                let unmirror = || -> Result<()> {
                    let config = any.begin_configuration().map_err(|e| {
                        anyhow!("CGBeginDisplayConfiguration (unmirror-tx): CGError {e}")
                    })?;
                    for id in &broke_mirror {
                        CGDisplay::new(*id)
                            .configure_display_mirror_of_display(&config, &null_master)
                            .map_err(|e| {
                                anyhow!(
                                    "CGConfigureDisplayMirrorOfDisplay(break {id}): CGError {e}"
                                )
                            })?;
                    }
                    any.complete_configuration(&config, CGConfigureOption::ConfigureForAppOnly)
                        .map_err(|e| {
                            anyhow!("CGCompleteDisplayConfiguration (unmirror-tx): CGError {e}")
                        })
                };
                unmirror().context(
                    "could not break the vd↔physical mirror — refusing to engage \
                     --shield-primary rather than shield a display that shares the \
                     client's framebuffer",
                )?;
                tracing::info!(
                    unmirrored_count = broke_mirror.len(),
                    "broke the vd→physical mirror so the built-in panel is a \
                     separate, shieldable display (reverts on disconnect / exit)"
                );

                // Displace the newly-separate physical(s) OFF the vd's origin —
                // load-bearing on single-panel auto-mirror hardware, and the fix
                // for "reconnect shows my windows, then a few seconds later the
                // desktop goes blank" (2026-07-22). Breaking the mirror leaves
                // the built-in panel a SEPARATE framebuffer that still sits at
                // the SAME bounds as the vd (both at (0,0), 1728×1084 here). The
                // user's app windows stay on the built-in; the client watches the
                // vd; so once the mirror breaks the client sees an empty vd
                // (wallpaper + its own menu bar) — the "reset to blank desktop".
                // The window-gather (capture.rs, on the connect re-mode) is meant
                // to sweep those windows onto the vd, but with both displays at
                // IDENTICAL bounds "move to the vd's top-left" is also inside the
                // physical, so macOS keeps the window there and the sweep is a
                // no-op. Moving the physical aside (x = vd_width) gives the vd
                // sole ownership of (0,0): the windows travel with the physical,
                // land off the vd, and the gather then relocates them onto the vd
                // unambiguously — exactly how the keep_physical_main=false
                // `arrange()` path already works. ForAppOnly so a SIGKILL/crash
                // auto-reverts (like the mirror-break itself); Drop's re-mirror
                // resets the origin regardless, so no extra restore is needed.
                // Only fires when a mirror was actually broken, so multi-panel
                // hardware (physicals already at their own origins) is untouched.
                //
                // CAVEAT (known, accepted): pinning the vd at (0,0) makes the vd
                // the system MAIN display, which is in tension with the
                // keep_physical_main default (whose point is that loginwindow
                // draws the lock screen on the physical main). On auto-mirror
                // hardware this changes nothing in practice — the vd is already
                // main even while idle (it is the mirror MASTER), so the
                // physical was never main here and the lock-screen-visibility
                // property never held on this hardware class to begin with.
                // Lock-while-shielded on the single-panel path is UNVERIFIED;
                // if it matters for a deployment, test ⌃⌘Q on a live shielded
                // session before relying on it (see the shield notes in
                // docs/known-quirks.md).
                let displace = || -> Result<()> {
                    let any = CGDisplay::new(virtual_display_id);
                    let config = any.begin_configuration().map_err(|e| {
                        anyhow!("CGBeginDisplayConfiguration (displace-tx): CGError {e}")
                    })?;
                    // Pin the vd at (0,0) so it owns the origin the client sees…
                    vd.configure_display_origin(&config, 0, 0)
                        .map_err(|e| anyhow!("CGConfigureDisplayOrigin(vd, 0, 0): CGError {e}"))?;
                    // …and push each unmirrored physical to the right of it.
                    let mut x_off = vd_width;
                    for id in &broke_mirror {
                        let d = CGDisplay::new(*id);
                        d.configure_display_origin(&config, x_off, 0).map_err(|e| {
                            anyhow!(
                                "CGConfigureDisplayOrigin(physical {id}, {x_off}, 0): CGError {e}"
                            )
                        })?;
                        x_off += d.bounds().size.width.round().max(1.0) as i32;
                    }
                    // ConfigureForSession, NOT ForAppOnly — the #161/#162 lesson
                    // replayed on this path, from a live re-strand (2026-07-22):
                    // with ForAppOnly the WindowServer's session store never
                    // learns the displaced arrangement, and a TRAILING relayout
                    // re-derives window placement from the stale store — a
                    // window the connect-time gather had already parked on the
                    // vd was observed re-stranded onto the physical's region
                    // minutes later ("chrome window is not showing"), exactly
                    // the whack-a-mole `CapturedPrimary` hit before its
                    // ForSession fix. Crash-safety holds by the same #162
                    // argument: if macrdp dies, the vd vanishes and the lone
                    // physical auto-anchors at (0,0) regardless of the store (a
                    // single display always anchors the arrangement), and Drop's
                    // re-mirror makes the stored origin moot on a clean
                    // disconnect (a mirrored panel has no independent origin).
                    any.complete_configuration(&config, CGConfigureOption::ConfigureForSession)
                        .map_err(|e| {
                            anyhow!("CGCompleteDisplayConfiguration (displace-tx): CGError {e}")
                        })
                };
                // Non-fatal: if the displace fails the shield still covers the
                // panel (privacy intact); the only loss is the client seeing an
                // empty vd until a manual Ctrl+Alt+G. Don't strand the session.
                if let Err(e) = displace() {
                    tracing::warn!(
                        error = %e,
                        "could not displace the shielded physical off the vd origin — the \
                         remote may see an empty desktop until Ctrl+Alt+G gathers windows"
                    );
                } else {
                    tracing::info!(
                        displaced_count = broke_mirror.len(),
                        "moved the shielded physical panel(s) off the vd origin so app \
                         windows gather onto the display the client sees"
                    );
                    displaced = true;
                }

                // Let the un-mirror + displace settle so the newly-separate
                // physical appears in NSScreen.screens AND the helper finishes
                // the shield relayout its own screen-parameters observer kicks
                // off, before we send the explicit SHOW below — otherwise SHOW
                // races that relayout and the helper is too busy to answer within
                // the read timeout (observed: the 1 s timeout firing while the
                // helper's window was already up).
                std::thread::sleep(Duration::from_millis(600));
            }

            // Raise the shields BEFORE the arrangement change. The origin tx
            // moves the vd to (0,0) and pushes the physicals aside, which is
            // itself a visible relayout on the physical panel; shielding first
            // means the user never sees it. (CapturedPrimary has the opposite
            // order because its gamma write would be reset by the tx anyway.)
            // Exclude the vd; the helper shields everything else it can see.
            // Phrased as an exclude list so a monitor attached mid-session is
            // covered too (the helper re-derives on screen-layout changes) —
            // an include list is a snapshot and would silently miss it.
            //
            // Retry until the helper confirms it covered every target: right
            // after a mirror-break the helper is relaying out its own shields
            // from the screen-change notification and can be transiently slow to
            // answer (read timeout) or answer before NSScreen has updated to
            // include the newly-separate panel (a short count). Neither is a real
            // failure; a retry a beat later succeeds. The ack (`count >= targets`)
            // is what proves the desktop is actually covered — a display absent
            // from NSScreen.screens (asleep/mid-wake) is skipped silently on the
            // helper side, so without it we could report a shielded-but-visible
            // desktop.
            // Backoff, not a flat retry: the helper can stay busy well past a
            // second. Observed live 2026-07-22 — a flat 4×300 ms (~5 s worst
            // case) EXHAUSTED and the mode refused to engage, leaving the panel
            // visible for the whole session. The helper was not wedged (a
            // manual SHOW moments later answered instantly); it was busy
            // relaying out across the mirror-break screen change *while*
            // macrdp's own post-resize gather sweep was moving 10 windows
            // through the WindowServer. So the tail is much longer than the
            // typical case, which is what backoff is for: the common path still
            // settles in ~300 ms, and a slow one gets ~13 s before we give up.
            //
            // Bounded deliberately rather than "retry until it works": this runs
            // on a `tokio::spawn`ed watcher task, so the whole loop blocks a
            // runtime worker (see the `crate::shield` module docs). ~13 s is the
            // most that seems defensible against that; if it is ever exhausted
            // again the fix is to move `install` onto a blocking thread rather
            // than to keep widening this.
            const SHOW_BACKOFF_MS: [u64; 6] = [300, 500, 800, 1200, 1600, 2000];
            let need = targets.len();
            let mut shielded_count = 0u16;
            let mut last_err: Option<anyhow::Error> = None;
            for (attempt, backoff) in SHOW_BACKOFF_MS
                .iter()
                .map(Some)
                .chain(std::iter::once(None)) // final attempt: no trailing sleep
                .enumerate()
            {
                match crate::shield::show(&[virtual_display_id]) {
                    Ok(n) => {
                        shielded_count = n;
                        if usize::from(n) >= need {
                            last_err = None;
                            break;
                        }
                        last_err = Some(anyhow!(
                            "shield helper covered only {n} of {need} display(s)"
                        ));
                    }
                    Err(e) => last_err = Some(e),
                }
                // Visible in the log, so a future engage failure shows how many
                // attempts it burned rather than only the final error.
                if let Some(e) = last_err.as_ref() {
                    tracing::debug!(
                        attempt = attempt + 1,
                        error = %e,
                        "shield SHOW not acknowledged yet — retrying"
                    );
                }
                match backoff {
                    Some(ms) => std::thread::sleep(Duration::from_millis(*ms)),
                    None => break,
                }
            }
            if usize::from(shielded_count) < need {
                let _ = crate::shield::hide();
                // Undo the mirror-break (which also undoes the displace) — we
                // return before `Self` exists, so Drop will never do it, and
                // the ForAppOnly break would otherwise outlive this failed
                // engage for the whole process lifetime: an un-mirrored,
                // displaced, empty panel in front of the local user.
                remirror_onto_vd(virtual_display_id, &broke_mirror);
                return Err(last_err
                    .unwrap_or_else(|| anyhow!("shield show failed"))
                    .context(
                        "could not raise the shield windows over every physical display \
                     — refusing to engage --shield-primary rather than leave one visible",
                    ));
            }

            // Arrangement: whether to move the vd to (0,0) as system main.
            //
            // keep_physical_main = true (default): SKIP the move entirely. The
            // physical panel stays main, so the lock screen (which loginwindow
            // draws on main) renders on the physical panel where it is visible.
            // The cost is that the vd is not main, so the remote desktop's menu
            // bar + Dock stay on the physical (shielded) panel. See
            // `shield_keeps_physical_main`.
            //
            // keep_physical_main = false: the old behaviour — move vd → (0,0),
            // physicals aside. Remote gets the menu bar + Dock, but the lock
            // screen renders on the invisible headless vd (refuted 2026-07-20).
            let keep_physical_main = shield_keeps_physical_main();
            if keep_physical_main {
                tracing::info!(
                    "--shield-primary: keeping the PHYSICAL panel as system main so a \
                     lock screen stays visible locally (the vd is NOT moved to (0,0)). \
                     Trade-off: the remote desktop's menu bar + Dock stay on the \
                     shielded physical panel. Set MACRDP_SHIELD_KEEP_PHYSICAL_MAIN=0 \
                     for the old vd-as-main behaviour."
                );
            } else {
                // Origin tx: vd → (0, 0), physicals shifted aside. See
                // `CapturedPrimary::install` for the full ConfigureForSession
                // rationale — it is identical here and equally load-bearing.
                //
                // CRITICAL: from here on the shields are UP, but no `Self` exists
                // yet — so `Drop` cannot run and there is nothing to lower them.
                // A bare `?` on any step below would return `Err` and strand a
                // black screen over the user's desktop for the life of the process
                // (macrdp keeps running; the watcher just logs the failed install).
                // So the fallible remainder runs in a closure whose `Err` we
                // intercept to lower the shields before propagating.
                let arrange = || -> Result<()> {
                    let any = CGDisplay::new(virtual_display_id);
                    let config = any.begin_configuration().map_err(|e| {
                        anyhow!("CGBeginDisplayConfiguration (move-tx): CGError {e}")
                    })?;
                    vd.configure_display_origin(&config, 0, 0)
                        .map_err(|e| anyhow!("CGConfigureDisplayOrigin(vd, 0, 0): CGError {e}"))?;
                    let mut x_off = vd_width;
                    for (id, _) in &targets {
                        let d = CGDisplay::new(*id);
                        if d.is_active() {
                            d.configure_display_origin(&config, x_off, 0).map_err(|e| {
                                anyhow!(
                                    "CGConfigureDisplayOrigin(physical {id}, {x_off}, 0): CGError {e}"
                                )
                            })?;
                            x_off += d.bounds().size.width.round() as i32;
                        }
                    }
                    any.complete_configuration(&config, CGConfigureOption::ConfigureForSession)
                        .map_err(|e| {
                            anyhow!("CGCompleteDisplayConfiguration (move-tx): CGError {e}")
                        })?;
                    Ok(())
                };
                if let Err(e) = arrange() {
                    tracing::warn!(
                        "arrangement transaction failed after the shields were raised — \
                         lowering them so the desktop is not left blacked out"
                    );
                    if let Err(he) = crate::shield::hide() {
                        // Both failed. Say so loudly: the screen is black and we
                        // could not clear it. It will clear when macrdp exits (the
                        // helper self-exits with its parent).
                        tracing::error!(
                            "could not lower the shields after a failed install: {he:#} — \
                             the physical display will stay black until macrdp exits"
                        );
                    }
                    // Same rationale as the SHOW-exhaust path above: no `Self`
                    // yet ⇒ no Drop ⇒ the mirror-break would outlive this
                    // failed engage. Undo it before propagating.
                    remirror_onto_vd(virtual_display_id, &broke_mirror);
                    return Err(e);
                }

                std::thread::sleep(TX_SETTLE);

                // The tx moved the panels, so re-fit the shields to their new
                // frames. The helper also re-fits itself on the screen-parameters
                // notification; this is the belt to that suspenders, and is cheap
                // because SHOW reconciles rather than stacking.
                if let Err(e) = crate::shield::show(&[virtual_display_id]) {
                    tracing::warn!("could not re-fit shields after the arrangement tx: {e:#}");
                }
            }

            tracing::info!(
                shielded_count = targets.len(),
                "physical displays shielded with black windows"
            );
            tracing::info!(
                "NOTE: --shield-primary does NOT capture the displays, so this Mac \
                 CAN still be locked normally (unlike --capture-primary). The \
                 trade-off is that a person at the machine can move the pointer, \
                 which disturbs the remote cursor; their clicks are swallowed."
            );

            if displaced {
                // The displace pushed the user's windows OFF the vd (they
                // travelled with the physical). Now pull them back onto the vd —
                // this is the step that actually makes the client see them.
                // Deliberately HERE, after every fallible step: install has
                // succeeded, so the windows can't be swept onto a vd the user
                // ends up unable to see (a failed install re-mirrors instead).
                // Detached + delayed so it runs after the displace has
                // registered in the WindowServer and doesn't block this
                // watcher-task's install. Two sweeps for the same reason
                // capture.rs uses two: the WindowServer relayout from the
                // displace has variable timing and can trail the first sweep.
                // A no-op second sweep is cheap.
                let vd_for_gather = virtual_display_id;
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(900));
                    let first = crate::input::gather_windows_onto_display(vd_for_gather);
                    std::thread::sleep(Duration::from_millis(1000));
                    let second = crate::input::gather_windows_onto_display(vd_for_gather);
                    if first > 0 || second > 0 {
                        tracing::info!(
                            first,
                            second,
                            display_id = vd_for_gather,
                            "gathered windows onto the vd after displacing the shielded panel"
                        );
                    }
                });
            }

            Ok(Self {
                virtual_id: virtual_display_id,
                virtual_old_origin,
                shielded: targets,
                moved_arrangement: !keep_physical_main,
                broke_mirror,
            })
        }

        /// Re-fit the shields, **fire-and-forget on a detached thread**.
        ///
        /// Unlike [`CapturedPrimary::reassert_blanking`] — a pair of
        /// synchronous CG calls costing microseconds — this is a loopback round
        /// trip to another process. The call sites in `capture.rs` run on the
        /// latency-sensitive resize path (one of them while holding the
        /// `VirtualDisplay` mutex, and three more from the post-gather sweep),
        /// so doing it inline would put a multi-second worst case — a dead or
        /// wedged helper — onto a hot path and hold that mutex across it.
        ///
        /// Detaching is safe precisely because this call is **not required for
        /// correctness**: a shield window survives a display reconfiguration
        /// (that is the entire premise of the mode), and the helper re-derives
        /// its shields on every screen-layout change anyway. This is belt to
        /// that suspenders, sent because the private `CGVirtualDisplay
        /// applySettings` is known not to emit a public reconfiguration event.
        ///
        /// Always returns 0 (nothing to report synchronously); failures are
        /// logged from the thread.
        pub fn reassert_blanking(&self) -> usize {
            let vd = self.virtual_id;
            std::thread::spawn(move || {
                if let Err(e) = crate::shield::show(&[vd]) {
                    tracing::warn!("could not re-fit shields after a display change: {e:#}");
                }
            });
            0
        }
    }

    impl Drop for ShieldedPrimary {
        fn drop(&mut self) {
            tracing::info!(
                shielded_count = self.shielded.len(),
                "ShieldedPrimary::drop running — lowering shields + restoring layout"
            );

            // Lower the shields first so the panels are usable again even if
            // the arrangement restore below fails.
            if let Err(e) = crate::shield::hide() {
                tracing::warn!(
                    "could not lower the shield windows: {e:#} — they will vanish \
                     when the helper exits with this process"
                );
            }

            // Restore any vd→physical mirror `install` broke, so the built-in
            // panel shows the desktop again between sessions. The un-mirror was
            // `ConfigureForAppOnly` (process-scoped), so it only self-reverts on
            // process EXIT — a clean disconnect keeps the process alive, so we
            // must re-mirror explicitly here or the built-in is left a separate,
            // empty extended desktop.
            remirror_onto_vd(self.virtual_id, &self.broke_mirror);

            // In keep-physical-main mode `install` never moved the arrangement,
            // so there is nothing to restore — lowering the shields (and any
            // re-mirror above) is the whole teardown.
            if !self.moved_arrangement {
                tracing::info!("lowered shields (no arrangement was moved)");
                return;
            }

            let any = CGDisplay::new(self.virtual_id);
            let config = match any.begin_configuration() {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(
                        "shields lowered but could not begin reposition-tx \
                         (CGError {e}); layout will revert on logout"
                    );
                    return;
                }
            };
            for (id, origin) in &self.shielded {
                let _ = CGDisplay::new(*id).configure_display_origin(&config, origin.0, origin.1);
            }
            let _ = CGDisplay::new(self.virtual_id).configure_display_origin(
                &config,
                self.virtual_old_origin.0,
                self.virtual_old_origin.1,
            );
            // ConfigureForSession — MUST match `install`, for exactly the reason
            // spelled out in `CapturedPrimary`'s Drop (a ForAppOnly restore
            // leaves the persisted store saying "vd is main" and the Dock
            // sometimes follows it off-screen on disconnect).
            if let Err(e) =
                any.complete_configuration(&config, CGConfigureOption::ConfigureForSession)
            {
                tracing::warn!(
                    "shields lowered but reposition-tx failed (CGError {e}); \
                     layout will revert on logout"
                );
            } else {
                tracing::info!(
                    shielded_count = self.shielded.len(),
                    "lowered shields and restored layout"
                );
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod stub {
    use anyhow::{anyhow, Result};

    /// No detach path off macOS, so nothing ever leaves a panel stuck.
    pub fn take_detach_reenable_failed() -> bool {
        false
    }

    /// No screen-lock concept off macOS — definitively not locked, not
    /// "unknown" (there's nothing to fail to look up).
    pub fn screen_is_locked() -> Option<bool> {
        Some(false)
    }

    /// No virtual display off macOS.
    pub fn is_hidpi_virtual_display(_display_id: u32) -> bool {
        false
    }

    pub struct VirtualDisplay;

    impl VirtualDisplay {
        pub fn new(
            _width: u32,
            _height: u32,
            _refresh_hz: u32,
            _hidpi: bool,
            _ui_scale: u32,
        ) -> Result<Self> {
            Err(anyhow!(
                "virtual display is macOS-only — this binary was built for a \
                 different target"
            ))
        }
        pub fn display_id(&self) -> u32 {
            0
        }
        pub fn origin_pts(&self) -> (f64, f64) {
            (0.0, 0.0)
        }
        pub fn size_pts(&self) -> (f64, f64) {
            (0.0, 0.0)
        }
        pub fn mode_px(&self) -> (u32, u32) {
            (0, 0)
        }
        pub fn ui_scale(&self) -> u32 {
            1
        }
        pub fn resize(&mut self, _width: u32, _height: u32, _ui_scale: u32) -> Result<()> {
            Err(anyhow!("virtual display is macOS-only"))
        }
        pub fn reanchor_as_main(&mut self) -> Result<bool> {
            Err(anyhow!("virtual display is macOS-only"))
        }
    }

    pub struct PrimaryOverride;

    impl PrimaryOverride {
        pub fn install(_virtual_display_id: u32) -> Result<Option<Self>> {
            Err(anyhow!("primary-display override is macOS-only"))
        }
    }

    pub struct DetachedPrimary;

    impl DetachedPrimary {
        pub fn install(_virtual_display_id: u32) -> Result<Self> {
            Err(anyhow!("primary-display detach is macOS-only"))
        }
    }

    pub struct CapturedPrimary;

    impl CapturedPrimary {
        pub fn install(_virtual_display_id: u32) -> Result<Self> {
            Err(anyhow!("primary-display capture is macOS-only"))
        }
        pub fn reassert_blanking(&self) -> usize {
            0
        }
    }

    pub struct ShieldedPrimary;

    impl ShieldedPrimary {
        pub fn install(_virtual_display_id: u32) -> Result<Self> {
            Err(anyhow!("primary-display shielding is macOS-only"))
        }
        pub fn reassert_blanking(&self) -> usize {
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_2x_client_gets_hidpi() {
        assert!(!scale_wants_hidpi(0)); // not announced
        assert!(!scale_wants_hidpi(100)); // Windows App, Retina off
        assert!(!scale_wants_hidpi(175)); // macOS has no fractional scaling
        assert!(scale_wants_hidpi(200)); // Windows App, Retina on
        assert!(scale_wants_hidpi(300));
    }

    #[test]
    fn auto_policy_follows_the_announced_scale() {
        use std::sync::{atomic::AtomicU32, Arc};
        let scale = Arc::new(AtomicU32::new(100));
        let policy = HidpiPolicy::Auto(scale.clone());
        assert!(policy.capable());
        assert_eq!(policy.ui_scale(), 1);
        scale.store(200, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(policy.ui_scale(), 2);
        assert!(!HidpiPolicy::Off.capable());
        assert_eq!(HidpiPolicy::Off.ui_scale(), 1);
        assert!(HidpiPolicy::On.capable());
        assert_eq!(HidpiPolicy::On.ui_scale(), 2);
    }

    #[test]
    fn a_mid_session_scale_change_switches_the_ui_scale() {
        use std::sync::{atomic::AtomicU32, Arc};
        let policy = HidpiPolicy::Auto(Arc::new(AtomicU32::new(200)));
        // Window moved from the Retina panel to a 1x monitor.
        assert!(policy.observe_client_scale(Some(100)));
        assert_eq!(policy.ui_scale(), 1);
        // Same scale again, or no scale in the layout: nothing to re-mode.
        assert!(!policy.observe_client_scale(Some(100)));
        assert!(!policy.observe_client_scale(None));
        assert_eq!(policy.ui_scale(), 1);
        // And back to the Retina panel.
        assert!(policy.observe_client_scale(Some(200)));
        assert_eq!(policy.ui_scale(), 2);
        // A fixed policy ignores the client.
        assert!(!HidpiPolicy::On.observe_client_scale(Some(100)));
        assert_eq!(HidpiPolicy::On.ui_scale(), 2);
    }

    /// Live check of the HiDPI re-mode rules against the real WindowServer.
    /// Creates (and destroys) a virtual display under a test-only identity,
    /// so it can't disturb a running macrdp:
    ///   cargo test --release hidpi_switching_live -- --ignored --nocapture
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "creates a real virtual display"]
    fn hidpi_switching_live() {
        let mut vd = VirtualDisplay::new(3456, 2170, 60, true, 2).unwrap();
        let id = vd.display_id();
        let state = |vd: &VirtualDisplay| (vd.size_pts(), is_hidpi_virtual_display(id));
        assert_eq!(state(&vd), ((1728.0, 1085.0), true), "created HiDPI");
        // Retina client at 3456x2168: 1728x1084 points.
        vd.resize(3456, 2168, 2).unwrap();
        assert_eq!(state(&vd), ((1728.0, 1084.0), true), "2x client");
        // 1x client at 1728x1084: the same 1728x1084-point HiDPI mode,
        // captured at the session size. No scale switch.
        vd.resize(1728, 1084, 1).unwrap();
        assert_eq!(state(&vd), ((1728.0, 1084.0), true), "1x client");
        assert_eq!(vd.ui_scale(), 1);
        // A live resize down and back up while staying HiDPI: the second
        // step is where the half-size variant can appear.
        vd.resize(1728, 1084, 2).unwrap();
        assert_eq!(state(&vd), ((864.0, 542.0), true), "small HiDPI");
        vd.resize(3456, 2168, 2).unwrap();
        assert_eq!(state(&vd), ((1728.0, 1084.0), true), "large HiDPI again");
        // A 1x client at another size.
        vd.resize(1440, 900, 1).unwrap();
        assert_eq!(state(&vd), ((1440.0, 900.0), true), "1x client at 1440x900");
    }

    #[test]
    fn panel_size_is_the_cosmetic_label_without_hidpi() {
        assert_eq!(panel_size_mm(3456, 2168, false), (600.0, 338.0));
    }

    #[test]
    fn hidpi_panel_keeps_common_retina_widths_in_the_scaled_band() {
        let (w_mm, h_mm) = panel_size_mm(3456, 2168, true);
        let ppi = |px: u32| f64::from(px) / (w_mm / 25.4);
        // The band macOS selected the scaled mode in, measured on 26.6.
        for px in [1440, 2560, 2880, 3024, 3456, 3840] {
            assert!(
                (155.0..=460.0).contains(&ppi(px)),
                "{px} px -> {} PPI",
                ppi(px)
            );
        }
        // Fixed width (it can't change after creation), aspect follows.
        assert_eq!(panel_size_mm(1920, 1080, true).0, w_mm);
        assert!((h_mm / w_mm - 2168.0 / 3456.0).abs() < 1e-9);
    }
}
