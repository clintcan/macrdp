//! Forward the macOS system cursor to RDP clients.
//!
//! Primary path: `private_api::copy_current_system_cursor` (SkyLight
//! `SLSGetGlobalCursorData`) — reads the WindowServer's actually-
//! composited cursor, so I-beams in other apps, crosshairs during
//! `screencapture -i`, hand pointers over links, etc. all forward
//! correctly. Returns raw RGBA bytes + hotspot directly, no CGImage
//! round-trip.
//!
//! Fallback path: `NSCursor.currentSystemCursor` rendered via
//! `NSBitmapImageRep` — process-local cursor stack, only sees cursors
//! set in macrdp's own process. Kept as a last resort in case a future
//! macOS removes/renames the SLS symbols.
//!
//! Position is read via CGEvent.location() on a fresh no-op event but
//! intentionally not forwarded — see `poll` for the reasoning.

#[cfg(target_os = "macos")]
mod private_api;

use ironrdp_server::DisplayUpdate;

pub struct CursorState {
    #[cfg(target_os = "macos")]
    inner: macos::Inner,
    #[cfg(not(target_os = "macos"))]
    _phantom: (),
}

impl CursorState {
    /// `screen_size_pts` is the target display's logical size in
    /// points; only consumed by the (currently-disabled) position
    /// polling path, but parameterized for symmetry with input.rs.
    ///
    /// `cursor_scale` is the user-facing comfort multiplier (`--cursor-scale`,
    /// default `1.0`). At `1.0` the SkyLight cursor passes through at its
    /// native size (matching the Mac's own screen, hotspot exact); higher
    /// values enlarge the pointer for clients that upscale the desktop image
    /// but draw the pointer at native pixels. The hotspot scales with the
    /// bitmap, so it stays accurate at any value.
    pub fn new(
        desktop_w: u16,
        desktop_h: u16,
        screen_size_pts: (f64, f64),
        cursor_scale: f64,
    ) -> anyhow::Result<Self> {
        #[cfg(target_os = "macos")]
        let inner = macos::Inner::new(desktop_w, desktop_h, screen_size_pts, cursor_scale)?;
        #[cfg(not(target_os = "macos"))]
        let _ = (desktop_w, desktop_h, screen_size_pts, cursor_scale);
        Ok(Self {
            #[cfg(target_os = "macos")]
            inner,
            #[cfg(not(target_os = "macos"))]
            _phantom: (),
        })
    }

    /// Return any cursor-related DisplayUpdates that the client should see.
    /// Cheap to call: the cost is one CGEvent::new + one NSCursor read.
    pub fn poll(&mut self) -> Vec<DisplayUpdate> {
        #[cfg(target_os = "macos")]
        return self.inner.poll();
        #[cfg(not(target_os = "macos"))]
        return Vec::new();
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    use anyhow::{anyhow, Result};
    use core_graphics::event::CGEvent;
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
    use ironrdp_pdu::pointer::PointerPositionAttribute;
    use ironrdp_server::{DisplayUpdate, RGBAPointer};
    use objc2::rc::Retained;
    use objc2::ClassType;
    use objc2_app_kit::{
        NSBitmapFormat, NSBitmapImageRep, NSCompositingOperation, NSCursor, NSDeviceRGBColorSpace,
        NSGraphicsContext,
    };
    use objc2_foundation::{NSPoint, NSRect, NSSize};
    use tracing::trace;

    use super::private_api;

    #[allow(dead_code)] // last_pos / desktop+screen fields are kept warm for
                        // re-enabling position polling in non-RDP-driven cases
    pub struct Inner {
        source: CGEventSource,
        last_hash: u64,
        last_pos: Option<(u16, u16)>,
        desktop_w: u16,
        desktop_h: u16,
        screen_w_pts: f64,
        screen_h_pts: f64,
        /// session-framebuffer-px ÷ display-backing-px; see `CursorState::new`.
        cursor_scale: f64,
        /// one-shot guard for the cursor-size diagnostic log.
        diag_logged: bool,
    }

    // CGEventSource is a CF type — thread-safe by Apple convention, single-
    // threaded use in practice. See input.rs for the same justification.
    unsafe impl Send for Inner {}

    impl Inner {
        pub fn new(
            desktop_w: u16,
            desktop_h: u16,
            screen_size_pts: (f64, f64),
            cursor_scale: f64,
        ) -> Result<Self> {
            let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
                .map_err(|_| anyhow!("CGEventSource::new failed"))?;
            // Guard against a zero/negative/NaN ratio (missing backing size,
            // virtual display, etc.) — fall back to 1.0 (pass-through).
            let cursor_scale = if cursor_scale.is_finite() && cursor_scale > 0.0 {
                cursor_scale
            } else {
                1.0
            };
            Ok(Self {
                source,
                last_hash: 0,
                last_pos: None,
                desktop_w,
                desktop_h,
                screen_w_pts: screen_size_pts.0,
                screen_h_pts: screen_size_pts.1,
                cursor_scale,
                diag_logged: false,
            })
        }

        pub fn poll(&mut self) -> Vec<DisplayUpdate> {
            let mut updates = Vec::new();
            self.poll_shape(&mut updates);
            // Intentionally NOT polling cursor position: mstsc (and other RDP
            // clients) predict the cursor locally from the user's mouse input
            // and snap to any PointerPositionPDU we send. Our updates lag the
            // local prediction by the encode/network round-trip, so sending
            // them causes the cursor to jump back to a stale position. The
            // downside: if a Mac app programmatically moves the cursor (rare),
            // the client won't see it until something else triggers a frame.
            updates
        }

        #[allow(dead_code)]
        fn poll_position(&mut self, out: &mut Vec<DisplayUpdate>) {
            let Ok(ev) = CGEvent::new(self.source.clone()) else {
                return;
            };
            let pt = ev.location();
            // CGEvent::location is in pixels with top-left origin (display
            // coordinates), same orientation as our captured framebuffer.
            let x = (pt.x * f64::from(self.desktop_w) / self.screen_w_pts.max(1.0))
                .round()
                .clamp(0.0, f64::from(u16::MAX));
            let y = (pt.y * f64::from(self.desktop_h) / self.screen_h_pts.max(1.0))
                .round()
                .clamp(0.0, f64::from(u16::MAX));
            let x = x as u16;
            let y = y as u16;
            if self.last_pos != Some((x, y)) {
                self.last_pos = Some((x, y));
                out.push(DisplayUpdate::PointerPosition(PointerPositionAttribute {
                    x,
                    y,
                }));
            }
        }

        fn poll_shape(&mut self, out: &mut Vec<DisplayUpdate>) {
            let bytes_and_hot = unsafe { read_cursor_bitmap() };
            let Some((data, w, h, hot_x, hot_y)) = bytes_and_hot else {
                return;
            };

            let mut hasher = DefaultHasher::new();
            data.hash(&mut hasher);
            // Mix size/hot into hash so identical pixel patterns at different
            // dimensions still register as a shape change.
            w.hash(&mut hasher);
            h.hash(&mut hasher);
            hot_x.hash(&mut hasher);
            hot_y.hash(&mut hasher);
            let hash = hasher.finish();
            if hash == self.last_hash {
                return;
            }
            self.last_hash = hash;
            let (raw_w, raw_h) = (w, h);
            // Default (`cursor_scale == 1.0`): pass the SkyLight cursor through
            // at its native size — correct on 1× panels, Retina, and `--hidpi`
            // alike, with an exact hotspot. A non-1.0 `--cursor-scale` resizes
            // the bitmap (and hotspot, which tracks it) purely for comfort.
            let (data, w, h, hot_x, hot_y) =
                scale_cursor(data, w, h, hot_x, hot_y, self.cursor_scale);
            // Oversized sprites (shake-to-locate at Retina backing pixels)
            // would overflow the pointer PDU's u16 mask length and kill the
            // client loop — shrink them to fit instead.
            let (data, w, h, hot_x, hot_y) = {
                let (pre_w, pre_h) = (w, h);
                let clamped = clamp_pointer_size(data, w, h, hot_x, hot_y);
                if (clamped.1, clamped.2) != (pre_w, pre_h) {
                    tracing::debug!(
                        pre_w,
                        pre_h,
                        w = clamped.1,
                        h = clamped.2,
                        "cursor sprite too large for the pointer PDU — downscaled to fit"
                    );
                }
                clamped
            };
            // One-shot diagnostic: the raw SkyLight size vs. the scaled output,
            // handy when tuning --cursor-scale against the local cursor.
            if !self.diag_logged {
                self.diag_logged = true;
                tracing::debug!(
                    raw_w,
                    raw_h,
                    scaled_w = w,
                    scaled_h = h,
                    hot_x,
                    hot_y,
                    scale = self.cursor_scale,
                    "cursor first shape: SkyLight raw size -> scaled size"
                );
            }
            trace!(
                raw_w,
                raw_h,
                w,
                h,
                hot_x,
                hot_y,
                scale = self.cursor_scale,
                "cursor shape changed"
            );
            out.push(DisplayUpdate::RGBAPointer(RGBAPointer {
                // macrdp doesn't maintain a client-side pointer cache; every
                // shape change re-sends the full bitmap to slot 0 (the encoder
                // always emits NewPointer for RGBAPointer, never CachedPointer),
                // so a single fixed index is correct and won't freeze animated
                // cursors (beachball/watch).
                cache_index: 0,
                width: w,
                height: h,
                hot_x,
                hot_y,
                data,
            }));
        }
    }

    /// Snapshot the current system cursor into a tightly-packed RGBA buffer.
    /// Returns `(data, width, height, hot_x, hot_y)` or `None` if anything
    /// went wrong (no cursor available, weird image size, draw failed).
    ///
    /// Two-tier lookup:
    ///   1. SkyLight `SLSGetGlobalCursorData` → the actually-rendered
    ///      system cursor (any process's I-beam, crosshair, hand, etc.).
    ///      Returns bitmap + hotspot directly.
    ///   2. Fallback to `NSCursor.currentSystemCursor` for the (rare)
    ///      case where SkyLight refuses the call or the symbols vanish
    ///      in a future macOS.
    unsafe fn read_cursor_bitmap() -> Option<(Vec<u8>, u16, u16, u16, u16)> {
        // Try SkyLight first — this sees cursors set by other processes
        // (Safari's I-beam, `screencapture -i`'s crosshair, web link hand
        // pointers, etc.) and gives us the real hotspot in one call.
        if let Some(t) = private_api::copy_current_system_cursor() {
            return Some(t);
        }

        // Fallback: NSCursor only sees cursors set in macrdp's process,
        // but works as a last resort if the private SkyLight symbols are
        // ever removed.
        let cursor: Retained<NSCursor> = match NSCursor::currentSystemCursor() {
            Some(c) => c,
            None => NSCursor::currentCursor(),
        };
        let image = cursor.image();
        let hot = cursor.hotSpot();
        let size = image.size();

        let w = size.width.round() as isize;
        let h = size.height.round() as isize;
        if !(1..=256).contains(&w) || !(1..=256).contains(&h) {
            return None;
        }

        // Allocate a 32-bit RGBA, non-premultiplied, top-down bitmap rep.
        // bytesPerRow=0 makes AppKit pick an aligned stride; we read it back.
        let rep: Retained<NSBitmapImageRep> = NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bitmapFormat_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            w,
            h,
            8,
            4,
            true,
            false,
            NSDeviceRGBColorSpace,
            NSBitmapFormat::AlphaNonpremultiplied,
            0,
            32,
        )?;

        let ctx = NSGraphicsContext::graphicsContextWithBitmapImageRep(&rep)?;
        NSGraphicsContext::saveGraphicsState_class();
        NSGraphicsContext::setCurrentContext(Some(&ctx));

        let dst_rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w as f64, h as f64));
        let zero_rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0));
        image.drawInRect_fromRect_operation_fraction(
            dst_rect,
            zero_rect,
            NSCompositingOperation::Copy,
            1.0,
        );

        NSGraphicsContext::restoreGraphicsState_class();

        let ptr = rep.bitmapData();
        if ptr.is_null() {
            return None;
        }
        let stride = rep.bytesPerRow() as usize;
        let row_bytes = (w as usize) * 4;
        let mut data = Vec::with_capacity((w as usize) * (h as usize) * 4);
        for row in 0..(h as usize) {
            let src = std::slice::from_raw_parts(ptr.add(row * stride), row_bytes);
            data.extend_from_slice(src);
        }

        Some((
            data,
            w as u16,
            h as u16,
            hot.x.round().max(0.0).min(f64::from(u16::MAX)) as u16,
            hot.y.round().max(0.0).min(f64::from(u16::MAX)) as u16,
        ))
    }

    /// Resample a tightly-packed top-down RGBA cursor bitmap (and its hotspot)
    /// by `scale`. Returns the input untouched when no resize is needed
    /// (`scale` ≈ 1, or rounding lands on the same dimensions), so the 1× and
    /// `--hidpi` cases stay byte-identical. Output dims are clamped to the RDP
    /// pointer limit (1..=256).
    fn scale_cursor(
        data: Vec<u8>,
        w: u16,
        h: u16,
        hot_x: u16,
        hot_y: u16,
        scale: f64,
    ) -> (Vec<u8>, u16, u16, u16, u16) {
        let new_w = ((f64::from(w) * scale).round() as i64).clamp(1, 256) as u16;
        let new_h = ((f64::from(h) * scale).round() as i64).clamp(1, 256) as u16;
        resize_cursor(data, w, h, hot_x, hot_y, new_w, new_h)
    }

    /// Resample a tightly-packed top-down RGBA cursor bitmap (and its hotspot)
    /// to exact target dimensions. Returns the input untouched when the
    /// dimensions already match.
    fn resize_cursor(
        data: Vec<u8>,
        w: u16,
        h: u16,
        hot_x: u16,
        hot_y: u16,
        new_w: u16,
        new_h: u16,
    ) -> (Vec<u8>, u16, u16, u16, u16) {
        if new_w == w && new_h == h {
            return (data, w, h, hot_x, hot_y);
        }
        let resized = resample_rgba(
            &data,
            w as usize,
            h as usize,
            new_w as usize,
            new_h as usize,
        );
        // Scale the hotspot by the actual per-axis resize ratio so it tracks
        // the resampled bitmap exactly (w/h are guaranteed >= 1 upstream).
        let hot_x = ((f64::from(hot_x) * f64::from(new_w) / f64::from(w)).round() as i64)
            .clamp(0, i64::from(new_w - 1)) as u16;
        let hot_y = ((f64::from(hot_y) * f64::from(new_h) / f64::from(h)).round() as i64)
            .clamp(0, i64::from(new_h - 1)) as u16;
        (resized, new_w, new_h, hot_x, hot_y)
    }

    /// Shrink an oversized cursor sprite so its 32-bpp bitmap fits the wire
    /// format. `TS_COLORPOINTERATTRIBUTE` carries the bitmap as its XOR mask
    /// with a u16 byte length, so `w*h*4` must stay ≤ 65535 (area ≤ 16383 px)
    /// or the encode fails — and that error tears down the whole client loop.
    /// Reachable in practice: macOS "shake mouse pointer to locate" enlarges
    /// the cursor several-fold, and at Retina backing pixels even 128×128
    /// (= 65536 bytes) is over the limit. Aspect ratio and hotspot are
    /// preserved; normal-sized cursors pass through untouched.
    fn clamp_pointer_size(
        data: Vec<u8>,
        w: u16,
        h: u16,
        hot_x: u16,
        hot_y: u16,
    ) -> (Vec<u8>, u16, u16, u16, u16) {
        const MAX_AREA: u32 = (u16::MAX / 4) as u32; // 16383 px at 4 bytes/px
        let area = u32::from(w) * u32::from(h);
        if area <= MAX_AREA || w == 0 || h == 0 {
            return (data, w, h, hot_x, hot_y);
        }
        // Floor guarantees new_w*new_h <= shrink²*w*h = MAX_AREA; the extra
        // per-axis min-clamp covers degenerate aspect ratios.
        let shrink = (f64::from(MAX_AREA) / f64::from(area)).sqrt();
        let new_h = (((f64::from(h) * shrink).floor() as u32).clamp(1, MAX_AREA)) as u16;
        let new_w =
            (((f64::from(w) * shrink).floor() as u32).clamp(1, MAX_AREA / u32::from(new_h))) as u16;
        resize_cursor(data, w, h, hot_x, hot_y, new_w, new_h)
    }

    /// Area-average resample of a tightly-packed top-down RGBA buffer. RGB is
    /// averaged weighted by alpha (premultiplied) so the cursor's transparent
    /// edges don't bleed dark/light fringes; alpha is area-averaged. Fine for
    /// the small down-scales we do here (e.g. 2:1 on Retina).
    fn resample_rgba(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<u8> {
        let mut out = vec![0u8; dw * dh * 4];
        if sw == 0 || sh == 0 {
            return out;
        }
        let fx = sw as f64 / dw as f64;
        let fy = sh as f64 / dh as f64;
        for dy in 0..dh {
            let sy0 = dy as f64 * fy;
            let sy1 = sy0 + fy;
            let iy0 = sy0.floor() as usize;
            let iy1 = (sy1.ceil() as usize).min(sh);
            for dx in 0..dw {
                let sx0 = dx as f64 * fx;
                let sx1 = sx0 + fx;
                let ix0 = sx0.floor() as usize;
                let ix1 = (sx1.ceil() as usize).min(sw);

                let mut acc_r = 0.0;
                let mut acc_g = 0.0;
                let mut acc_b = 0.0;
                let mut acc_a = 0.0; // Σ alpha·coverage — normalizes premultiplied RGB
                let mut cov_sum = 0.0; // Σ coverage — area-averages alpha

                for sy in iy0..iy1 {
                    let cy = (sy1.min((sy + 1) as f64) - sy0.max(sy as f64)).max(0.0);
                    if cy <= 0.0 {
                        continue;
                    }
                    for sx in ix0..ix1 {
                        let cx = (sx1.min((sx + 1) as f64) - sx0.max(sx as f64)).max(0.0);
                        if cx <= 0.0 {
                            continue;
                        }
                        let cov = cx * cy;
                        let idx = (sy * sw + sx) * 4;
                        let a = f64::from(src[idx + 3]) / 255.0;
                        acc_r += f64::from(src[idx]) * a * cov;
                        acc_g += f64::from(src[idx + 1]) * a * cov;
                        acc_b += f64::from(src[idx + 2]) * a * cov;
                        acc_a += a * cov;
                        cov_sum += cov;
                    }
                }

                let oidx = (dy * dw + dx) * 4;
                let alpha = if cov_sum > 0.0 { acc_a / cov_sum } else { 0.0 };
                let (r, g, b) = if acc_a > 0.0 {
                    (acc_r / acc_a, acc_g / acc_a, acc_b / acc_a)
                } else {
                    (0.0, 0.0, 0.0)
                };
                out[oidx] = r.round().clamp(0.0, 255.0) as u8;
                out[oidx + 1] = g.round().clamp(0.0, 255.0) as u8;
                out[oidx + 2] = b.round().clamp(0.0, 255.0) as u8;
                out[oidx + 3] = (alpha * 255.0).round().clamp(0.0, 255.0) as u8;
            }
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::{clamp_pointer_size, resample_rgba, scale_cursor};

        #[test]
        fn scale_one_is_passthrough() {
            let data = vec![1, 2, 3, 4, 5, 6, 7, 8];
            let (out, w, h, hx, hy) = scale_cursor(data.clone(), 2, 1, 1, 0, 1.0);
            assert_eq!((w, h, hx, hy), (2, 1, 1, 0));
            assert_eq!(out, data);
        }

        #[test]
        fn half_scale_halves_dims_and_hotspot() {
            // 32×32 opaque red, hotspot near center.
            let data = vec![0u8; 32 * 32 * 4]
                .chunks(4)
                .flat_map(|_| [255, 0, 0, 255])
                .collect::<Vec<_>>();
            let (out, w, h, hx, hy) = scale_cursor(data, 32, 32, 16, 16, 0.5);
            assert_eq!((w, h), (16, 16));
            assert_eq!((hx, hy), (8, 8));
            // Solid opaque red must survive the downscale on every pixel.
            assert!(out.chunks_exact(4).all(|p| p == [255, 0, 0, 255]));
        }

        #[test]
        fn clamp_leaves_normal_cursors_untouched() {
            // 64×64 (Retina backing of a 32-pt cursor) is well under the
            // 16383-px area limit and must pass through byte-identical.
            let data = vec![7u8; 64 * 64 * 4];
            let (out, w, h, hx, hy) = clamp_pointer_size(data.clone(), 64, 64, 10, 12);
            assert_eq!((w, h, hx, hy), (64, 64, 10, 12));
            assert_eq!(out, data);
        }

        #[test]
        fn clamp_shrinks_oversized_cursor_under_u16_mask_limit() {
            // 128×128×4 = 65536 bytes — exactly one byte over the u16 XOR-mask
            // limit; the real-world shake-to-locate-at-Retina case.
            let data = vec![255u8; 128 * 128 * 4];
            let (out, w, h, hx, hy) = clamp_pointer_size(data, 128, 128, 64, 64);
            assert!(u32::from(w) * u32::from(h) * 4 <= u32::from(u16::MAX));
            assert_eq!(out.len(), usize::from(w) * usize::from(h) * 4);
            // Hotspot stays centered and in-bounds.
            assert!(hx < w && hy < h);
            assert!((i32::from(hx) - i32::from(w) / 2).abs() <= 1);
            assert!((i32::from(hy) - i32::from(h) / 2).abs() <= 1);
        }

        #[test]
        fn clamp_handles_worst_case_and_extreme_aspect() {
            // 256×256 (the scale_cursor output cap) and a degenerate 256×64
            // strip both land under the limit with sane dims.
            for (sw, sh) in [(256u16, 256u16), (256, 64), (64, 256)] {
                let data = vec![1u8; usize::from(sw) * usize::from(sh) * 4];
                let (out, w, h, _, _) = clamp_pointer_size(data, sw, sh, 0, 0);
                assert!(
                    u32::from(w) * u32::from(h) * 4 <= u32::from(u16::MAX),
                    "{sw}x{sh} -> {w}x{h} still over the u16 mask limit"
                );
                assert!(w >= 1 && h >= 1);
                assert_eq!(out.len(), usize::from(w) * usize::from(h) * 4);
            }
        }

        #[test]
        fn downscale_averages_alpha_without_colour_fringe() {
            // 2×1: one opaque red pixel, one fully transparent pixel.
            // Area-averaging to 1×1 should give red at half alpha — NOT a
            // darkened/blended colour (the premultiplied-weight guarantee).
            let src = [255, 0, 0, 255, 0, 0, 0, 0];
            let out = resample_rgba(&src, 2, 1, 1, 1);
            assert_eq!(out[0], 255, "red preserved");
            assert_eq!(out[1], 0);
            assert_eq!(out[2], 0);
            assert_eq!(out[3], 128, "alpha area-averaged (255+0)/2 rounded");
        }
    }
}
