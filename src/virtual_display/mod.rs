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
pub use macos::{CapturedPrimary, DetachedPrimary, PrimaryOverride, VirtualDisplay};

#[cfg(not(target_os = "macos"))]
pub use stub::{CapturedPrimary, DetachedPrimary, PrimaryOverride, VirtualDisplay};

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
    }

    impl VirtualDisplay {
        /// Allocate a headless display at `width × height` pixels and
        /// `refresh_hz` Hz. Refresh rate is mostly cosmetic — the RDP
        /// server's frame cadence is independent — but a real value
        /// keeps `Displays.app` from looking weird.
        ///
        /// Returns an Err with a clear "private API unavailable" message
        /// if any of the underlying CG symbols / Obj-C classes can't be
        /// resolved on this macOS version. Caller should treat that as
        /// "this feature isn't usable here," not a fatal bug.
        pub fn new(width: u32, height: u32, refresh_hz: u32) -> Result<Self> {
            let handle = private_api::create(width, height, refresh_hz, "macrdp")
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

            Ok(Self {
                _handle: handle,
                display_id: id,
                origin_pts: (bounds.origin.x, bounds.origin.y),
                size_pts: (bounds.size.width, bounds.size.height),
            })
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

        /// Live-resize the display to a new mode (client-driven session
        /// resize). Re-applies settings with a single mode at the new size
        /// — the WindowServer treats it like a physical monitor changing
        /// resolution (windows re-layout, bounds update) — then polls
        /// `CGDisplayBounds` until the new size is visible (the apply is
        /// async on the WindowServer side; creation has the same
        /// zero-size-rect race, handled there by erroring). The display id
        /// is stable across the resize; the global-space origin can shift,
        /// so both cached values are refreshed. Virtual displays are
        /// always 1:1 points-to-pixels (no HiDPI backing — see the
        /// known-quirks note), so the expected bounds equal the mode size.
        pub fn resize(&mut self, width: u32, height: u32) -> Result<()> {
            self._handle
                .apply_mode(width, height, 60)
                .context("re-applying virtual display mode")?;

            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            loop {
                let bounds = CGDisplay::new(self.display_id).bounds();
                if bounds.size.width as u32 == width && bounds.size.height as u32 == height {
                    self.origin_pts = (bounds.origin.x, bounds.origin.y);
                    self.size_pts = (bounds.size.width, bounds.size.height);
                    return Ok(());
                }
                if std::time::Instant::now() >= deadline {
                    return Err(anyhow!(
                        "virtual display mode applied but CGDisplayBounds still reports \
                         {}x{} (expected {width}x{height}) after 3s — WindowServer hasn't \
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
                tracing::warn!(
                    "exhausted enable-tx retries; detached displays will be \
                     re-enabled when this process exits (ForAppOnly scope)"
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
}

#[cfg(not(target_os = "macos"))]
mod stub {
    use anyhow::{anyhow, Result};

    pub struct VirtualDisplay;

    impl VirtualDisplay {
        pub fn new(_width: u32, _height: u32, _refresh_hz: u32) -> Result<Self> {
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
        pub fn resize(&mut self, _width: u32, _height: u32) -> Result<()> {
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
}
