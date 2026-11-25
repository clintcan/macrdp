//! Capture for a multi-monitor session (`--multi-monitor`, phase 1,
//! docs/multi-monitor-design.md): one ScreenCaptureKit stream per monitor's
//! virtual display, each monitor's frames going to its own EGFX surface and
//! H.264 encoder (`Gfx::submit_bgra_monitor`). Before H.264 is up (or for a
//! client without it) frames are pasted into one desktop-sized BGRA frame
//! and sent as legacy bitmaps at the monitor's position. A separate type from `ScreenCaptureUpdates`, so the
//! single-monitor path is unchanged; it covers the essentials only (no NV12,
//! no keyframe-on-change, no live resize yet).
//!
//! Reuses `capture`'s stream and bitmap helpers (crate-visible for this).

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use ironrdp_server::{DisplayUpdate, RdpServerDisplayUpdates, ServerResult};
use screencapturekit::cm::CMTime;
use screencapturekit::cv::CVPixelBufferLockFlags;
use screencapturekit::prelude::{PixelFormat as SckPixelFormat, SCStreamConfiguration};

use crate::capture::macos::{
    find_sck_display, rect_update, DisplayStream, SCK_QUEUE_DEPTH, SUPPRESS_DEBOUNCE,
};
use crate::capture::{display_error, split_strips, SuppressedFlag};
use crate::cursor::CursorState;
use crate::monitor_layout::Layout;

/// How often the cursor is polled while no frame arrives (as in the
/// single-monitor loop).
const CURSOR_POLL_INTERVAL: Duration = Duration::from_millis(66);

/// One monitor's capture.
struct MonitorStream {
    stream: Option<DisplayStream>,
    display_id: u32,
    /// Position and size in the desktop (pixels).
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    config: SCStreamConfiguration,
    /// When the stream stopped (display sleep): retried after a pause.
    stopped_at: Option<Instant>,
    /// The last frame sent over H.264, retained (not copied) for the flush
    /// burst that drains mstsc's presentation buffer on a static screen.
    last_frame: Option<screencapturekit::cv::CVPixelBuffer>,
    /// A frame that couldn't go out yet (EGFX negotiating, or ready but the
    /// surfaces not created): re-sent once it can, since a static display
    /// sends no new frames (the single-monitor path's `held_frame`).
    held: Option<screencapturekit::cv::CVPixelBuffer>,
}

pub struct MultiCaptureUpdates {
    monitors: Vec<MonitorStream>,
    /// The desktop as one BGRA frame, `desktop.0 * 4` bytes per row.
    composite: Vec<u8>,
    desktop: (u16, u16),
    gfx: Option<crate::h264::Gfx>,
    cursor: CursorState,
    pending: VecDeque<DisplayUpdate>,
    last_cursor_poll: Option<Instant>,
    frame_interval: Duration,
    flush_frames: u32,
    flush_remaining: u32,
    display_suppressed: Option<SuppressedFlag>,
    suppressed_since: Option<Instant>,
    was_suppressed: bool,
    first_egfx_frame_sent: bool,
    started_at: Instant,
    /// Where `first_ready` starts polling, rotated so no monitor starves.
    next_start: usize,
    /// The primary monitor's index in the layout.
    primary: usize,
    /// A live layout change (`CaptureDisplay::request_layout`), applied like a
    /// single-monitor live resize: a reactivation at the new desktop size,
    /// after which `updates()` rebuilds the displays, capture and surfaces.
    pending_resize: crate::capture::PendingResize,
    desktop_size: crate::capture::SharedDesktopSize,
    suppress_next_adopt: std::sync::Arc<std::sync::atomic::AtomicBool>,
    fps: u32,
    /// What an in-place re-layout updates (see `relayout_in_place`).
    relayout: Relayout,
}

/// The shared state an in-place re-layout changes: the displays, the session
/// layout (H.264 surfaces are built from it) and the input map.
#[derive(Clone)]
pub struct Relayout {
    pub session_layout: crate::multi_monitor::SessionLayout,
    pub active_monitors: crate::multi_monitor::ActiveMonitors,
    pub virtual_display: std::sync::Arc<std::sync::Mutex<crate::virtual_display::VirtualDisplay>>,
    pub extra_displays: std::sync::Arc<std::sync::Mutex<crate::multi_monitor::ExtraDisplays>>,
}

impl Relayout {
    /// Sizes and arranges the displays for `layout` and returns their ids in
    /// the layout's order. Blocking (re-modes wait for the WindowServer).
    fn sync_displays(&self, layout: &Layout) -> Result<Vec<u32>> {
        let mut vd = self
            .virtual_display
            .lock()
            .map_err(|_| anyhow!("virtual display mutex poisoned"))?;
        let p = layout.monitors()[layout.primary()];
        let size = (u32::from(p.width), u32::from(p.height));
        if vd.mode_px() != size {
            vd.resize(size.0, size.1, 1)
                .context("re-moding the main display")?;
        }
        let mut extras = self
            .extra_displays
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        extras.sync(layout, &mut vd)?;
        let ids = display_ids(layout, vd.display_id(), &extras.ids());
        if ids.len() != layout.monitors().len() {
            return Err(anyhow!(
                "{} displays for {} monitors",
                ids.len(),
                layout.monitors().len()
            ));
        }
        Ok(ids)
    }
}

impl Drop for MonitorStream {
    fn drop(&mut self) {
        // Until 2026-11-25 nothing stopped these, so each multi-monitor
        // session (and each in-place re-layout) leaked its captures.
        if let Some(stream) = self.stream.as_ref() {
            stream.stop();
        }
    }
}

/// Opens one capture stream per monitor of `layout`, on `display_ids` (the
/// same order).
async fn open_streams(
    layout: &Layout,
    display_ids: &[u32],
    fps: u32,
) -> Result<Vec<MonitorStream>> {
    if display_ids.len() != layout.monitors().len() {
        return Err(anyhow!(
            "multi-monitor: {} displays for {} monitors",
            display_ids.len(),
            layout.monitors().len()
        ));
    }
    let interval = CMTime::new(1, fps.max(1) as i32);
    let mut monitors = Vec::new();
    for (m, &id) in layout.monitors().iter().zip(display_ids) {
        let display = find_sck_display(id).await?;
        // Captured 1:1 at the monitor's own size: no scaling, no letterbox.
        let config = SCStreamConfiguration::new()
            .with_width(u32::from(m.width))
            .with_height(u32::from(m.height))
            .with_pixel_format(SckPixelFormat::BGRA)
            .with_minimum_frame_interval(&interval)
            .with_shows_cursor(false)
            .with_scales_to_fit(true)
            .with_preserves_aspect_ratio(false)
            .with_queue_depth(SCK_QUEUE_DEPTH);
        let stream = DisplayStream::start(&display, &config)
            .map_err(|e| anyhow!("SCStream::start_capture failed (display {id}): {e:?}"))?;
        monitors.push(MonitorStream {
            stream: Some(stream),
            display_id: id,
            x: m.x,
            y: m.y,
            width: m.width,
            height: m.height,
            config,
            stopped_at: None,
            last_frame: None,
            held: None,
        });
    }
    Ok(monitors)
}

/// The first of `futures` to finish, polling from `start` (mod len) so a
/// monitor that always has a frame ready can't starve the others.
async fn first_ready<F>(mut futures: Vec<F>, start: usize) -> F::Output
where
    F: std::future::Future + Unpin,
{
    std::future::poll_fn(move |cx| {
        let n = futures.len();
        for k in 0..n {
            let f = &mut futures[(start + k) % n];
            if let std::task::Poll::Ready(v) = std::pin::Pin::new(f).poll(cx) {
                return std::task::Poll::Ready(v);
            }
        }
        std::task::Poll::Pending
    })
    .await
}

impl MultiCaptureUpdates {
    /// `display_ids[i]` is the virtual display serving `layout.monitors()[i]`.
    #[allow(clippy::too_many_arguments)]
    pub async fn start(
        layout: &Layout,
        display_ids: &[u32],
        fps: u32,
        gfx: Option<crate::h264::Gfx>,
        cursor_scale: f64,
        flush_frames: u32,
        display_suppressed: Option<SuppressedFlag>,
        desktop_size: crate::capture::SharedDesktopSize,
        pending_resize: crate::capture::PendingResize,
        suppress_next_adopt: std::sync::Arc<std::sync::atomic::AtomicBool>,
        relayout: Relayout,
    ) -> Result<Self> {
        let monitors = open_streams(layout, display_ids, fps).await?;
        let desktop = layout.desktop();
        let primary_id = display_ids[layout.primary()];
        let cursor = CursorState::new(
            desktop_size.clone(),
            primary_id,
            cursor_scale.clamp(0.1, 8.0),
        )?;
        if let Some(flag) = display_suppressed.as_ref() {
            flag.clear();
        }
        tracing::info!(
            displays = ?display_ids,
            desktop = ?desktop,
            "multi-monitor: capturing one stream per monitor"
        );
        Ok(Self {
            monitors,
            composite: vec![0; usize::from(desktop.0) * usize::from(desktop.1) * 4],
            desktop,
            gfx,
            cursor,
            pending: VecDeque::new(),
            last_cursor_poll: None,
            frame_interval: Duration::from_secs_f64(1.0 / f64::from(fps.max(1))),
            flush_frames,
            flush_remaining: 0,
            display_suppressed,
            suppressed_since: None,
            was_suppressed: false,
            first_egfx_frame_sent: false,
            started_at: Instant::now(),
            next_start: 0,
            primary: layout.primary(),
            pending_resize,
            desktop_size,
            suppress_next_adopt,
            fps,
            relayout,
        })
    }

    /// Applies a new multi-monitor layout inside the running session, as
    /// MS-RDPEDISP 1.3 asks of a server using the graphics pipeline ("restart
    /// the graphics pipeline using the surface management commands"): the
    /// displays are re-moded and re-arranged, the captures restarted, and the
    /// next frame deletes the surfaces, sends RESET_GRAPHICS with the new
    /// monitors and creates new ones (`Gfx::setup_locked`). No core
    /// deactivation-reactivation: after one, mstsc sometimes kept a one-monitor
    /// full-screen window and drew both monitors on it (live 2026-11-25).
    /// False if the displays couldn't be set up; the caller then falls back to
    /// a reactivation, which retries them.
    async fn relayout_in_place(&mut self, layout: Layout) -> Result<bool> {
        let relayout = self.relayout.clone();
        let sync_layout = layout.clone();
        let ids = match tokio::task::spawn_blocking(move || relayout.sync_displays(&sync_layout))
            .await
            .map_err(|e| anyhow!("multi-monitor: display sync task failed: {e}"))?
        {
            Ok(ids) => ids,
            Err(e) => {
                tracing::warn!(
                    error = format!("{e:#}"),
                    "multi-monitor: re-laying out the displays in place failed; reactivating instead"
                );
                return Ok(false);
            }
        };
        // Stop the old captures before opening the new ones on the same displays.
        self.monitors.clear();
        let monitors = open_streams(&layout, &ids, self.fps).await?;
        let (w, h) = layout.desktop();
        *self
            .relayout
            .session_layout
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(layout.clone());
        self.desktop_size.set(w, h);
        *self
            .relayout
            .active_monitors
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some((layout.clone(), ids.clone()));
        if let Some(gfx) = self.gfx.as_ref() {
            gfx.reset_for_live_resize();
        }
        self.monitors = monitors;
        self.desktop = (w, h);
        self.composite = vec![0; usize::from(w) * usize::from(h) * 4];
        self.primary = layout.primary();
        self.next_start = 0;
        self.flush_remaining = 0;
        tracing::info!(
            displays = ?ids,
            desktop = ?(w, h),
            layout = ?layout.client_monitors(),
            "multi-monitor: applied the client's layout change in place (graphics reset, no reactivation)"
        );
        Ok(true)
    }

    fn stride(&self) -> usize {
        usize::from(self.desktop.0) * 4
    }

    /// Restarts a stream SCK stopped (its display slept), at most every second.
    async fn restart_stopped(&mut self) {
        for m in &mut self.monitors {
            if m.stream.is_some()
                || m.stopped_at
                    .is_some_and(|t| t.elapsed() < Duration::from_secs(1))
            {
                continue;
            }
            match find_sck_display(m.display_id).await {
                Ok(display) => match DisplayStream::start(&display, &m.config) {
                    Ok(stream) => {
                        tracing::info!(
                            display_id = m.display_id,
                            "multi-monitor: capture restarted"
                        );
                        m.stream = Some(stream);
                        m.stopped_at = None;
                    }
                    Err(e) => {
                        tracing::warn!(display_id = m.display_id, error = ?e, "multi-monitor: restart failed");
                        m.stopped_at = Some(Instant::now());
                    }
                },
                Err(e) => {
                    tracing::debug!(display_id = m.display_id, error = ?e, "multi-monitor: display not listed yet");
                    m.stopped_at = Some(Instant::now());
                }
            }
        }
    }

    /// The next sample from any monitor: `Ok(Some((index, sample)))`, `Ok(None)`
    /// on timeout.
    async fn next_sample(
        &mut self,
        wait: Duration,
    ) -> Option<(usize, Option<screencapturekit::cm::CMSampleBuffer>)> {
        let futures: Vec<_> = self
            .monitors
            .iter()
            .enumerate()
            .filter_map(|(i, m)| {
                let stream = m.stream.as_ref()?;
                Some(Box::pin(
                    async move { (i, stream.next().await.map(|(s, _)| s)) },
                ))
            })
            .collect();
        if futures.is_empty() {
            tokio::time::sleep(wait).await;
            return None;
        }
        self.next_start = self.next_start.wrapping_add(1);
        tokio::time::timeout(wait, first_ready(futures, self.next_start))
            .await
            .ok()
    }

    /// Sends monitor `i`'s frame: to its H.264 surface once EGFX is up,
    /// otherwise pasted into the desktop frame and sent as legacy bitmaps at
    /// the monitor's position (unless EGFX is still negotiating: then it waits).
    fn send(
        &mut self,
        i: usize,
        pb: &screencapturekit::cv::CVPixelBuffer,
        keyframe: bool,
        flush: bool,
    ) -> Result<()> {
        let guard = pb
            .lock(CVPixelBufferLockFlags::READ_ONLY)
            .map_err(|e| anyhow!("CVPixelBuffer::lock OSStatus {e}"))?;
        let src = guard.as_slice();
        let src_stride = guard.bytes_per_row();
        if let Some(gfx) = self.gfx.as_ref() {
            match gfx.submit_bgra_monitor(i, self.primary, src, src_stride, keyframe, flush) {
                Ok(true) => {
                    if !flush {
                        self.monitors[i].held = None;
                    }
                    if i == self.primary {
                        self.first_egfx_frame_sent = true;
                    }
                    if !flush {
                        drop(guard);
                        self.monitors[i].last_frame = Some(pb.clone());
                        self.flush_remaining = self.flush_frames;
                    }
                    return Ok(());
                }
                Ok(false) if flush => return Ok(()),
                Ok(false) if gfx.hold_legacy(self.started_at) => {
                    drop(guard);
                    self.monitors[i].held = Some(pb.clone());
                    return Ok(());
                }
                // Ready, but the primary's first frame hasn't set the surfaces
                // up yet: wait for it (no legacy bitmaps under a live EGFX).
                // Still negotiating past the hold limit falls through to legacy
                // bitmaps, as in the single-monitor path: a client that never
                // opens EGFX (seen live: Windows App reconnecting after a
                // server restart) must still get a picture.
                Ok(false) if gfx.pipeline_ready() => {
                    drop(guard);
                    self.monitors[i].held = Some(pb.clone());
                    return Ok(());
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(monitor = i, error = ?e, "multi-monitor: EGFX submit failed");
                    return Ok(());
                }
            }
        }
        if flush {
            return Ok(());
        }
        self.monitors[i].held = None;
        // Legacy bitmaps: paste, then send this monitor's area.
        let m = &self.monitors[i];
        let (mx, my, mw, mh) = (m.x, m.y, m.width, m.height);
        let rows = usize::from(mh).min(guard.height());
        let row_bytes = usize::from(mw).min(guard.width()) * 4;
        let stride = self.stride();
        let (x, y) = (usize::from(mx), usize::from(my));
        for row in 0..rows {
            let from = row * src_stride;
            let to = (y + row) * stride + x * 4;
            let (Some(s), Some(d)) = (
                src.get(from..from + row_bytes),
                self.composite.get_mut(to..to + row_bytes),
            ) else {
                break;
            };
            d.copy_from_slice(s);
        }
        for (sx, sy, sw, sh) in split_strips(mx, my, mw, mh) {
            if let Some(update) = rect_update(&self.composite, stride, sx, sy, sw, sh) {
                self.pending.push_back(update);
            }
        }
        Ok(())
    }

    /// Re-sends held frames once they can go out: EGFX ready (primary first,
    /// since its frame creates the surfaces) or the negotiation hold over
    /// (then as legacy bitmaps).
    fn send_held(&mut self) -> Result<()> {
        if self.monitors.iter().all(|m| m.held.is_none()) {
            return Ok(());
        }
        let can_send = self
            .gfx
            .as_ref()
            .is_none_or(|gfx| gfx.pipeline_ready() || !gfx.hold_legacy(self.started_at));
        if !can_send {
            return Ok(());
        }
        let order = std::iter::once(self.primary)
            .chain((0..self.monitors.len()).filter(|&k| k != self.primary));
        for k in order.collect::<Vec<_>>() {
            if let Some(pb) = self.monitors[k].held.take() {
                self.send(k, &pb, true, false)?;
            }
        }
        Ok(())
    }

    async fn capture_next_update(&mut self) -> Result<Option<DisplayUpdate>> {
        loop {
            if let Some(gfx) = self.gfx.as_ref() {
                if crate::RESYNC_VIDEO.swap(false, Ordering::Relaxed) {
                    gfx.force_keyframe();
                }
                if let Some((w, h)) = gfx.take_reactivate_request() {
                    return Ok(Some(DisplayUpdate::Resize(ironrdp_server::DesktopSize {
                        width: w,
                        height: h,
                    })));
                }
            }

            // A live layout change, as the single-monitor loop applies a live
            // resize: the new size now, surfaces and encoders rebuilt after
            // the reactivation, and the reactivation's size not re-adopted
            // from the client's echo.
            if let Some((w, h)) = self
                .pending_resize
                .take_settled(crate::capture::RESIZE_DEBOUNCE)
            {
                // Another multi-monitor layout, with H.264 running: in place.
                // Without the graphics pipeline, or to one monitor (the
                // single-monitor capture loop takes over), a reactivation.
                let egfx = self.gfx.as_ref().is_some_and(|g| g.pipeline_ready());
                if let (true, Some(Some(layout))) = (egfx, self.pending_resize.peek_layout()) {
                    if self.relayout_in_place(layout).await? {
                        self.pending_resize.take_layout();
                        continue;
                    }
                }
                self.desktop_size.set(w, h);
                self.suppress_next_adopt.store(true, Ordering::Relaxed);
                if let Some(gfx) = self.gfx.as_ref() {
                    gfx.reset_for_live_resize();
                }
                tracing::info!(w, h, "multi-monitor: applying the client's layout change");
                return Ok(Some(DisplayUpdate::Resize(ironrdp_server::DesktopSize {
                    width: w,
                    height: h,
                })));
            }

            if self
                .last_cursor_poll
                .is_none_or(|t| t.elapsed() >= CURSOR_POLL_INTERVAL)
            {
                self.last_cursor_poll = Some(Instant::now());
                let mut updates = self.cursor.poll();
                if !updates.is_empty() {
                    let first = updates.remove(0);
                    for u in updates.into_iter().rev() {
                        self.pending.push_front(u);
                    }
                    return Ok(Some(first));
                }
            }

            self.send_held()?;

            if let Some(update) = self.pending.pop_front() {
                return Ok(Some(update));
            }

            // Minimized client: stop capturing (as in the single-monitor loop,
            // debounced and only after the first EGFX frame).
            if self.first_egfx_frame_sent {
                if let Some(flag) = self.display_suppressed.as_ref() {
                    if flag.load() {
                        let since = *self.suppressed_since.get_or_insert_with(Instant::now);
                        if since.elapsed() >= SUPPRESS_DEBOUNCE {
                            self.was_suppressed = true;
                            self.flush_remaining = 0;
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            continue;
                        }
                    } else {
                        self.suppressed_since = None;
                    }
                }
            }

            self.restart_stopped().await;

            let wait = if self.flush_remaining > 0 {
                self.frame_interval
            } else {
                CURSOR_POLL_INTERVAL
            };
            let Some((i, sample)) = self.next_sample(wait).await else {
                if self.flush_remaining > 0 {
                    self.flush_remaining -= 1;
                    for k in 0..self.monitors.len() {
                        if let Some(pb) = self.monitors[k].last_frame.clone() {
                            self.send(k, &pb, false, true)?;
                        }
                    }
                }
                continue;
            };
            let Some(sample) = sample else {
                tracing::warn!(
                    display_id = self.monitors[i].display_id,
                    "multi-monitor: SCK stopped a capture; retrying"
                );
                if let Some(stream) = self.monitors[i].stream.take() {
                    stream.stop();
                }
                self.monitors[i].stopped_at = Some(Instant::now());
                continue;
            };
            if let Some(status) = sample.frame_status() {
                if !status.has_content() {
                    continue;
                }
            }
            let Some(pb) = sample.image_buffer() else {
                continue;
            };
            let resume = std::mem::take(&mut self.was_suppressed);
            if resume {
                if let Some(gfx) = self.gfx.as_ref() {
                    gfx.demigrate_on_resume();
                    // Every monitor resumes from a keyframe.
                    gfx.request_keyframe_all();
                }
            }
            self.send(i, &pb, resume, false)
                .context("multi-monitor: sending a frame")?;
        }
    }
}

#[async_trait::async_trait]
impl RdpServerDisplayUpdates for MultiCaptureUpdates {
    async fn next_update(&mut self) -> ServerResult<Option<DisplayUpdate>> {
        self.capture_next_update().await.map_err(display_error)
    }
}

/// The virtual display ids for `layout`'s monitors, in layout order: the main
/// display for the primary monitor, the extras for the others (in order).
pub fn display_ids(layout: &Layout, main: u32, extras: &[u32]) -> Vec<u32> {
    let primary = layout.primary();
    let mut extras = extras.iter();
    (0..layout.monitors().len())
        .map(|i| {
            if i == primary {
                main
            } else {
                extras.next().copied().unwrap_or(main)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironrdp_pdu::gcc::{Monitor, MonitorFlags};

    fn mon(left: i32, width: i32, primary: bool) -> Monitor {
        Monitor {
            left,
            top: 0,
            right: left + width - 1,
            bottom: 767,
            flags: if primary {
                MonitorFlags::PRIMARY
            } else {
                MonitorFlags::empty()
            },
        }
    }

    #[test]
    fn the_main_display_serves_the_primary_and_the_extras_the_rest_in_order() {
        // mstsc lists the secondary monitor first (seen live).
        let layout = Layout::from_client(&[mon(1920, 1366, false), mon(0, 1920, true)]).unwrap();
        assert_eq!(display_ids(&layout, 18, &[19]), vec![19, 18]);
        let three = Layout::from_client(&[
            mon(-1024, 1024, false),
            mon(0, 1920, true),
            mon(1920, 1366, false),
        ])
        .unwrap();
        assert_eq!(display_ids(&three, 18, &[19, 20]), vec![19, 18, 20]);
    }

    #[tokio::test]
    async fn first_ready_takes_turns_between_ready_futures() {
        let ready = |v: u8| Box::pin(async move { v });
        assert_eq!(first_ready(vec![ready(0), ready(1), ready(2)], 0).await, 0);
        assert_eq!(first_ready(vec![ready(0), ready(1), ready(2)], 1).await, 1);
        assert_eq!(first_ready(vec![ready(0), ready(1), ready(2)], 5).await, 2);
        // A pending one is skipped.
        let pending: std::pin::Pin<Box<dyn std::future::Future<Output = u8>>> =
            Box::pin(std::future::pending());
        let done: std::pin::Pin<Box<dyn std::future::Future<Output = u8>>> = Box::pin(async { 7 });
        assert_eq!(first_ready(vec![pending, done], 0).await, 7);
    }
}
