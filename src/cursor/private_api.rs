//! Read the actual system-rendered cursor bitmap via undocumented SkyLight
//! symbols. ALL touches to private CoreGraphics/SkyLight symbols live in this
//! file — when Apple changes the API in a future macOS release (renames a
//! function, drops a parameter, returns a different type), update only this
//! file.
//!
//! Why this exists: the WindowServer's connection exposes the cursor it is
//! actually compositing on screen, as raw pixels with the hotspot, via the
//! private functions below. `NSCursor.currentSystemCursor` (the fallback in
//! `cursor/mod.rs`) also returns cursors set by other processes, but only as
//! an `NSImage` that must be drawn into a bitmap first.
//!
//! Symbols live in `/System/Library/PrivateFrameworks/SkyLight.framework`,
//! aren't exported in the framework's `.tbd` stub, and weren't kept under
//! the legacy `CGS*` names — they're `SLS*` on modern macOS (12+). We
//! resolve them at runtime via `dlsym(RTLD_DEFAULT, ...)` instead of
//! link-time: if Apple removes or renames any of them in a future macOS,
//! the lookup returns `None`, this module returns `None`, and `cursor/mod.rs`
//! falls back to the public NSCursor path without the binary failing to
//! launch.
//!
//! Reverse-engineered signatures cross-checked against:
//!   - setomits/PrivateHeadersForMacOS — `SLSCursor.h`
//!   - macmade/CursorView — `SkyLight.h`
//!   - sapuapu/MouseCape (CGS-era naming, same shape)
#![cfg(target_os = "macos")]

use core_graphics::geometry::{CGPoint, CGRect};
use libc::{dlsym, RTLD_DEFAULT};
use std::ffi::{c_int, c_void, CString};
use std::sync::OnceLock;

#[allow(non_camel_case_types)]
type CGSConnectionID = c_int;

type SLSMainConnectionIDFn = unsafe extern "C" fn() -> CGSConnectionID;
type SLSGetGlobalCursorDataSizeFn = unsafe extern "C" fn(CGSConnectionID, *mut c_int) -> c_int;
// Signature (9 out params after the connection):
//   data:             buffer to fill (caller-allocated, size from -DataSize)
//   data_size:        IN size of buffer / OUT bytes actually written
//   row_bytes:        OUT row stride
//   rect:             OUT cursor bounding rect (size = bitmap dimensions)
//   hot_spot:         OUT hotspot in pixels relative to the cursor rect
//   depth:            OUT bits per pixel (typically 32)
//   components:       OUT components per pixel (typically 4)
//   bits_per_component: OUT bits per component (typically 8)
// Pixel format in memory is little-endian BGRA premultiplied — i.e., the
// CG-native bitmap layout (kCGBitmapByteOrder32Little |
// kCGImageAlphaPremultipliedFirst). Byte 0 of each pixel is blue. Rows are
// top-down: row 0 holds the arrow's tip.
type SLSGetGlobalCursorDataFn = unsafe extern "C" fn(
    CGSConnectionID,
    *mut c_void,
    *mut c_int,
    *mut c_int,
    *mut CGRect,
    *mut CGPoint,
    *mut c_int,
    *mut c_int,
    *mut c_int,
) -> c_int;

/// Resolve a private SkyLight/CoreGraphics symbol by name. Returns `None`
/// if the symbol isn't present (the OS version removed it, sandbox is
/// hiding it, etc.).
fn resolve<T: Copy>(name: &str) -> Option<T> {
    let cname = CString::new(name).ok()?;
    let sym = unsafe { dlsym(RTLD_DEFAULT, cname.as_ptr()) };
    if sym.is_null() {
        return None;
    }
    // SAFETY: caller picks the right function-pointer type for the
    // symbol they asked for. We're internal-only here.
    Some(unsafe { *(&sym as *const *mut c_void as *const T) })
}

fn main_connection_fn() -> Option<SLSMainConnectionIDFn> {
    static F: OnceLock<Option<SLSMainConnectionIDFn>> = OnceLock::new();
    *F.get_or_init(|| resolve("SLSMainConnectionID"))
}

fn cursor_data_size_fn() -> Option<SLSGetGlobalCursorDataSizeFn> {
    static F: OnceLock<Option<SLSGetGlobalCursorDataSizeFn>> = OnceLock::new();
    *F.get_or_init(|| resolve("SLSGetGlobalCursorDataSize"))
}

fn cursor_data_fn() -> Option<SLSGetGlobalCursorDataFn> {
    static F: OnceLock<Option<SLSGetGlobalCursorDataFn>> = OnceLock::new();
    *F.get_or_init(|| resolve("SLSGetGlobalCursorData"))
}

/// Snapshot the real WindowServer cursor.
///
/// Returns `(rgba_unpremult, width, height, hot_x, hot_y)`. The pixel
/// buffer is tightly packed, top-down, RGBA with non-premultiplied alpha.
/// Returns `None` if any private symbol is missing or any SLS call fails.
/// What SkyLight gave for the current cursor.
pub enum SkyLightCursor {
    /// RGBA bytes, width, height, hotspot x, hotspot y.
    Cursor((Vec<u8>, u16, u16, u16, u16)),
    /// A cursor larger than the RDP pointer allows (an animated cursor's frames
    /// stacked in one image): worth the `NSCursor` fallback.
    TooLarge,
    /// No usable cursor (a failed call, an empty or unexpected image). The
    /// caller keeps the last shape: `NSCursor` doesn't know when an app has
    /// hidden the cursor, so falling back here showed the arrow over full-screen
    /// video.
    Unavailable,
}

pub fn copy_current_system_cursor() -> SkyLightCursor {
    match read_system_cursor() {
        Ok(cursor) => SkyLightCursor::Cursor(cursor),
        Err(Rejected::TooLarge) => SkyLightCursor::TooLarge,
        Err(Rejected::Unavailable) => SkyLightCursor::Unavailable,
    }
}

enum Rejected {
    TooLarge,
    Unavailable,
}

/// Whether a width or height SkyLight reports is too large for the RDP pointer
/// (more than 256) rather than missing.
fn too_large(w: i64, h: i64) -> bool {
    (w > 256 && h >= 1) || (h > 256 && w >= 1)
}

fn read_system_cursor() -> Result<(Vec<u8>, u16, u16, u16, u16), Rejected> {
    use Rejected::Unavailable;
    let main_conn = main_connection_fn().ok_or(Unavailable)?;
    let size_fn = cursor_data_size_fn().ok_or(Unavailable)?;
    let data_fn = cursor_data_fn().ok_or(Unavailable)?;

    unsafe {
        let cid = main_conn();

        let mut data_size: c_int = 0;
        if size_fn(cid, &mut data_size) != 0 || data_size <= 0 {
            return Err(Unavailable);
        }
        // Refuse absurd sizes — RDP pointers cap at 256x256x4 = 256 KiB
        // plus a generous margin. Anything wildly bigger is a wire-format
        // mismatch we'd rather skip than honour.
        if data_size as usize > 1024 * 1024 {
            return Err(Unavailable);
        }

        let mut buf = vec![0u8; data_size as usize];
        let mut size_inout: c_int = data_size;
        let mut row_bytes: c_int = 0;
        let mut rect = CGRect {
            origin: CGPoint { x: 0.0, y: 0.0 },
            size: core_graphics::geometry::CGSize {
                width: 0.0,
                height: 0.0,
            },
        };
        let mut hot = CGPoint { x: 0.0, y: 0.0 };
        let mut depth: c_int = 0;
        let mut components: c_int = 0;
        let mut bits_per_component: c_int = 0;

        let err = data_fn(
            cid,
            buf.as_mut_ptr() as *mut c_void,
            &mut size_inout,
            &mut row_bytes,
            &mut rect,
            &mut hot,
            &mut depth,
            &mut components,
            &mut bits_per_component,
        );
        if err != 0 {
            return Err(Unavailable);
        }

        let w = rect.size.width.round() as i64;
        let h = rect.size.height.round() as i64;
        if too_large(w, h) {
            return Err(Rejected::TooLarge);
        }
        if !(1..=256).contains(&w) || !(1..=256).contains(&h) {
            return Err(Unavailable);
        }
        let w = w as usize;
        let h = h as usize;
        let row_bytes = row_bytes as usize;
        if row_bytes < w * 4 {
            return Err(Unavailable);
        }
        // Sanity: BGRA premultiplied is what we expect (depth=32, comps=4,
        // bpc=8). If macOS ever returns something different — say a 16bpc
        // HDR cursor — skip rather than emit garbled colors.
        if depth != 32 || components != 4 || bits_per_component != 8 {
            return Err(Unavailable);
        }
        if size_inout as usize > buf.len() {
            return Err(Unavailable);
        }

        let out = bgra_premultiplied_to_rgba(&buf, row_bytes, w, h).ok_or(Unavailable)?;

        let hot_x = hot.x.round().max(0.0).min(f64::from(u16::MAX)) as u16;
        let hot_y = hot.y.round().max(0.0).min(f64::from(u16::MAX)) as u16;
        Ok((out, w as u16, h as u16, hot_x, hot_y))
    }
}

/// Premultiplied BGRA rows `row_bytes` apart → tightly packed straight RGBA,
/// keeping the row order.
fn bgra_premultiplied_to_rgba(buf: &[u8], row_bytes: usize, w: usize, h: usize) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(w * h * 4);
    for row in 0..h {
        let start = row * row_bytes;
        let src = buf.get(start..start + w * 4)?;
        for &[b, g, r, a] in src.as_chunks::<4>().0 {
            let (r, g, b) = un_premultiply(r, g, b, a);
            out.extend_from_slice(&[r, g, b, a]);
        }
    }
    Some(out)
}

/// Tightly-packed RGBA from premultiplied RGBA rows with stride `row_bytes`
/// (the NSCursor fallback in `mod.rs` draws into a premultiplied bitmap).
pub(super) fn rgba_premultiplied_to_rgba(
    buf: &[u8],
    row_bytes: usize,
    w: usize,
    h: usize,
) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(w * h * 4);
    for row in 0..h {
        let start = row * row_bytes;
        let src = buf.get(start..start + w * 4)?;
        for &[r, g, b, a] in src.as_chunks::<4>().0 {
            let (r, g, b) = un_premultiply(r, g, b, a);
            out.extend_from_slice(&[r, g, b, a]);
        }
    }
    Some(out)
}

fn un_premultiply(r: u8, g: u8, b: u8, a: u8) -> (u8, u8, u8) {
    match a {
        0 => (0, 0, 0),
        255 => (r, g, b),
        _ => {
            let a_u = a as u32;
            let r = ((r as u32 * 255 + a_u / 2) / a_u).min(255) as u8;
            let g = ((g as u32 * 255 + a_u / 2) / a_u).min(255) as u8;
            let b = ((b as u32 * 255 + a_u / 2) / a_u).min(255) as u8;
            (r, g, b)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{bgra_premultiplied_to_rgba, rgba_premultiplied_to_rgba};

    #[test]
    fn conversion_swaps_to_rgb_unpremultiplies_and_keeps_row_order() {
        // 1x2 with an 8-byte stride: opaque red on top, then blue premultiplied at A=128.
        let buf = [0, 0, 255, 255, 9, 9, 9, 9, 128, 0, 0, 128, 9, 9, 9, 9];
        let out = bgra_premultiplied_to_rgba(&buf, 8, 1, 2).unwrap();
        assert_eq!(out, vec![255, 0, 0, 255, 0, 0, 255, 128]);
    }

    /// Only a cursor larger than the RDP pointer earns the NSCursor fallback; an
    /// empty or missing one doesn't (it keeps the last shape, which matters
    /// while an app hides the cursor).
    #[test]
    fn only_an_oversized_cursor_is_too_large() {
        assert!(
            super::too_large(28, 600),
            "an animated cursor's frame strip"
        );
        assert!(super::too_large(300, 300));
        assert!(!super::too_large(28, 40));
        assert!(!super::too_large(256, 256));
        assert!(!super::too_large(0, 0), "an empty cursor isn't too large");
        assert!(!super::too_large(0, 600));
        assert!(!super::too_large(600, 0));
    }

    #[test]
    fn short_buffer_is_rejected() {
        assert_eq!(bgra_premultiplied_to_rgba(&[0; 11], 8, 1, 2), None);
        assert_eq!(rgba_premultiplied_to_rgba(&[0; 11], 8, 1, 2), None);
    }

    #[test]
    fn rgba_conversion_unpremultiplies_and_keeps_row_order() {
        // 1x2 with an 8-byte stride: opaque red on top, then blue premultiplied at A=128.
        let buf = [255, 0, 0, 255, 9, 9, 9, 9, 0, 0, 128, 128, 9, 9, 9, 9];
        let out = rgba_premultiplied_to_rgba(&buf, 8, 1, 2).unwrap();
        assert_eq!(out, vec![255, 0, 0, 255, 0, 0, 255, 128]);
    }
}
