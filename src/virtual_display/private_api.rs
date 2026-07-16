//! All touches to undocumented `CGVirtualDisplay*` private API live here.
//!
//! When Apple renames or restructures the surface in a future macOS,
//! this is the only file that should need to change. The public side
//! (in `mod.rs`) talks to this layer through stable Rust types only —
//! no `*mut AnyObject`, no private class names leak out.
//!
//! ## API surface
//!
//! On macOS 26+ the surface is a fully Obj-C class `CGVirtualDisplay`
//! with `-initWithDescriptor:` / `-applySettings:` / `-displayID`. The
//! older C functions `CGVirtualDisplayCreate` /
//! `CGVirtualDisplayApplySettings` / `CGVirtualDisplayGetDisplayID`
//! that BetterDisplay's wrappers cover were removed/renamed at some
//! point during the macOS 16/26 cycle. We don't currently support the
//! older path — if you need it, add a fallback in `create()` that
//! detects class absence via `AnyClass::get` and reaches for `dlsym`
//! on the C symbols instead.
//!
//! ## Maintenance contract
//!
//! - All Obj-C classes resolved via `AnyClass::get(...)`. A missing
//!   class surfaces as an `Err` naming what's gone.
//! - Selectors used: `alloc`, `init`, `release`, `setName:`,
//!   `setMaxPixelsWide:`, `setMaxPixelsHigh:`, `setSizeInMillimeters:`,
//!   `setProductID:`, `setVendorID:`, `setSerialNum:`,
//!   `initWithDescriptor:`, `initWithWidth:height:refreshRate:`,
//!   `arrayWithObject:`, `setHiDPI:`, `setModes:`, `applySettings:`,
//!   `displayID`. Confirmed against macOS 26.4 (`dyld_info -exports`
//!   on `/System/Library/Frameworks/CoreGraphics.framework/...`).

#![cfg(target_os = "macos")]

use std::ffi::{c_void, CString};

use anyhow::{anyhow, Result};
use objc2::msg_send;
use objc2::runtime::{AnyClass, AnyObject};
use objc2_foundation::{CGSize, NSString};

type CGDirectDisplayID = u32;

/// `CGSConfigureDisplayEnabled(config, display, enabled)` — undocumented
/// SkyLight symbol re-exported from CoreGraphics on macOS 26. Called
/// between `CGBeginDisplayConfiguration` / `CGCompleteDisplayConfiguration`
/// to enable or disable a display from the WindowServer's perspective
/// (backlight off, no menu bar, no windows can be placed there).
pub(super) struct DisplayEnabler(unsafe extern "C" fn(*mut c_void, CGDirectDisplayID, bool) -> i32);

impl DisplayEnabler {
    pub(super) fn load() -> Result<Self> {
        unsafe {
            let rtld_default = -2isize as *mut c_void;
            let name = CString::new("CGSConfigureDisplayEnabled").unwrap();
            let p = libc::dlsym(rtld_default, name.as_ptr());
            if p.is_null() {
                return Err(anyhow!(
                    "private CoreGraphics symbol `CGSConfigureDisplayEnabled` \
                     not found — display detach is unavailable on this macOS"
                ));
            }
            Ok(Self(std::mem::transmute::<
                *mut c_void,
                unsafe extern "C" fn(*mut c_void, CGDirectDisplayID, bool) -> i32,
            >(p)))
        }
    }

    /// Apply the enabled/disabled state inside an active CG display config.
    /// `config` is the `CGDisplayConfigRef` returned by
    /// `CGDisplay::begin_configuration` — its layout is `*mut c_void` in
    /// the core-graphics crate, which is what the FFI expects.
    pub(super) fn set(&self, config: *mut c_void, display: u32, enabled: bool) -> Result<()> {
        let err = unsafe { (self.0)(config, display, enabled) };
        if err == 0 {
            Ok(())
        } else {
            Err(anyhow!(
                "CGSConfigureDisplayEnabled(display={display}, enabled={enabled}) \
                 returned CGError {err}"
            ))
        }
    }
}

fn class_required(name: &str) -> Result<&'static AnyClass> {
    AnyClass::get(name).ok_or_else(|| {
        anyhow!(
            "private Obj-C class `{name}` not registered in the runtime — \
             virtual-display support is unavailable on this macOS"
        )
    })
}

/// Owned `CGVirtualDisplay` instance. Released via Obj-C `release` on
/// Drop, which is what un-registers the display from the system.
pub(super) struct Handle {
    raw: *mut AnyObject,
    display_id: u32,
}

// SAFETY: CGVirtualDisplay is a CF-bridged Obj-C class; CF types are
// thread-safe. Our usage is single-threaded but the wrapper crosses
// thread boundaries in main's task setup.
unsafe impl Send for Handle {}

impl Handle {
    pub(super) fn display_id(&self) -> u32 {
        self.display_id
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe {
                let _: () = msg_send![self.raw, release];
            }
            self.raw = std::ptr::null_mut();
        }
    }
}

/// Allocate a virtual display at the requested pixel size, apply a
/// single-mode settings object, and return the owning handle.
///
/// Temporary Obj-C objects (descriptor, mode, mode-array, settings)
/// are intentionally leaked — they're tiny, only allocated once per
/// process, and avoiding manual `release` calls keeps the code
/// obviously free of double-free hazards.
pub(super) fn create(width: u32, height: u32, refresh_hz: u32, name: &str) -> Result<Handle> {
    let desc_class = class_required("CGVirtualDisplayDescriptor")?;
    let display_class = class_required("CGVirtualDisplay")?;
    let settings_class = class_required("CGVirtualDisplaySettings")?;
    let mode_class = class_required("CGVirtualDisplayMode")?;
    let nsarray_class = class_required("NSArray")?;

    let ns_name = NSString::from_str(name);

    unsafe {
        // 1. Descriptor: alloc/init + property setters. Every property
        //    is cosmetic except the pixel dimensions; the rest just
        //    populates the metadata Displays.app shows for the monitor.
        let desc: *mut AnyObject = msg_send![desc_class, alloc];
        let desc: *mut AnyObject = msg_send![desc, init];
        if desc.is_null() {
            return Err(anyhow!("CGVirtualDisplayDescriptor init returned nil"));
        }
        let _: () = msg_send![desc, setName: &*ns_name];
        let _: () = msg_send![desc, setMaxPixelsWide: width];
        let _: () = msg_send![desc, setMaxPixelsHigh: height];
        // 600x338 mm ≈ a 27-inch 16:9 panel. Just a label.
        let _: () = msg_send![desc, setSizeInMillimeters: CGSize { width: 600.0, height: 338.0 }];
        let _: () = msg_send![desc, setProductID: 0x6D616372u32]; // "macr"
        let _: () = msg_send![desc, setVendorID: 0x6D616372u32];
        // Was hardcoded to 1: a second macrdp process (e.g. two concurrent
        // installs, or dev testing alongside a running instance) registering a
        // display with an identical vendor/product/serial triple gets its
        // descriptor rejected by CGVirtualDisplay initWithDescriptor: outright
        // (returns nil). Per-process id keeps concurrent instances from colliding.
        let _: () = msg_send![desc, setSerialNum: std::process::id()];

        // 2. CGVirtualDisplay instance from the descriptor.
        let display: *mut AnyObject = msg_send![display_class, alloc];
        let display: *mut AnyObject = msg_send![display, initWithDescriptor: desc];
        if display.is_null() {
            return Err(anyhow!(
                "[CGVirtualDisplay initWithDescriptor:] returned nil — the \
                 descriptor was rejected (try different dimensions)"
            ));
        }

        // 3. One mode at the requested resolution, then a settings object
        //    holding that single-mode array.
        let mode: *mut AnyObject = msg_send![mode_class, alloc];
        let mode: *mut AnyObject = msg_send![
            mode,
            initWithWidth: width,
            height: height,
            refreshRate: f64::from(refresh_hz),
        ];
        if mode.is_null() {
            let _: () = msg_send![display, release];
            return Err(anyhow!("[CGVirtualDisplayMode init...] returned nil"));
        }
        let modes: *mut AnyObject = msg_send![nsarray_class, arrayWithObject: mode];

        let settings: *mut AnyObject = msg_send![settings_class, alloc];
        let settings: *mut AnyObject = msg_send![settings, init];
        if settings.is_null() {
            let _: () = msg_send![display, release];
            return Err(anyhow!("[CGVirtualDisplaySettings init] returned nil"));
        }
        let _: () = msg_send![settings, setHiDPI: 0u32];
        let _: () = msg_send![settings, setModes: modes];

        // 4. Apply settings — this is the call that actually registers
        //    the display with the WindowServer so it appears in Displays.
        let ok: bool = msg_send![display, applySettings: settings];
        if !ok {
            let _: () = msg_send![display, release];
            return Err(anyhow!(
                "[CGVirtualDisplay applySettings:] returned false — mode is \
                 likely unsupported (try a different resolution / refresh rate)"
            ));
        }

        // 5. Read back the assigned displayID.
        let display_id: u32 = msg_send![display, displayID];
        if display_id == 0 {
            let _: () = msg_send![display, release];
            return Err(anyhow!(
                "[CGVirtualDisplay displayID] returned 0 — display registered \
                 but the system hasn't assigned a directDisplayID"
            ));
        }

        Ok(Handle {
            raw: display,
            display_id,
        })
    }
}
