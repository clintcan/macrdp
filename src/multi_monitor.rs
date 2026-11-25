//! Multi-monitor sessions (`--multi-monitor`, docs/multi-monitor-design.md).
//!
//! The main virtual display serves the client's primary monitor; this module
//! owns one more virtual display per other monitor and keeps them sized and
//! arranged like the client's monitors. The layout comes from the connection
//! (`ConnectionInfo::monitor_layout`, filled by `conn_hooks::MonitorLayoutHook`).

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use crate::monitor_layout::Layout;
use crate::virtual_display::VirtualDisplay;

/// The current connection's adopted monitor layout, `None` for a
/// single-monitor client. Written when a client connects, read when its
/// display updates start.
pub type SessionLayout = Arc<Mutex<Option<Layout>>>;

/// The monitors being served and the virtual display behind each (in the
/// layout's order), for input to send a pointer event to the monitor it lands
/// on. Set when a multi-monitor capture starts, cleared for a single-monitor
/// session.
pub type ActiveMonitors = Arc<Mutex<Option<(Layout, Vec<u32>)>>>;

/// The descriptor serial for extra display `index`: the pid with the index
/// (from 1) in the top byte, so it never equals the main display's serial
/// (the pid) or another extra's. macOS pids are below 2^24.
pub fn extra_serial(index: usize) -> u32 {
    let index = u32::try_from(index).unwrap_or(0).saturating_add(1) & 0xFF;
    (std::process::id() & 0x00FF_FFFF) | (index << 24)
}

/// The non-primary monitors' rectangles, in the layout's order.
fn others(layout: &Layout) -> Vec<crate::monitor_layout::SessionMonitor> {
    let primary = layout.primary();
    layout
        .monitors()
        .iter()
        .enumerate()
        .filter(|&(i, _)| i != primary)
        .map(|(_, m)| *m)
        .collect()
}

/// The extra virtual displays of a multi-monitor session, one per
/// non-primary monitor. They live as long as macrdp, like the main display,
/// and are resized, added or released when a client brings a different
/// layout.
#[derive(Default)]
pub struct ExtraDisplays {
    displays: Vec<VirtualDisplay>,
}

impl ExtraDisplays {
    /// The extra displays' ids, in the order of the layout's non-primary
    /// monitors.
    pub fn ids(&self) -> Vec<u32> {
        self.displays
            .iter()
            .map(VirtualDisplay::display_id)
            .collect()
    }

    /// Releases every extra display (a single-monitor client).
    pub fn release_all(&mut self) {
        if !self.displays.is_empty() {
            tracing::info!(
                count = self.displays.len(),
                "multi-monitor: releasing the extra displays"
            );
            self.displays.clear();
        }
    }

    /// Makes the extra displays match `layout`'s non-primary monitors, then
    /// arranges all the session's displays as the client's monitors are.
    pub fn sync(&mut self, layout: &Layout, primary: &mut VirtualDisplay) -> Result<()> {
        let wanted = others(layout);
        if self.displays.len() > wanted.len() {
            tracing::info!(
                from = self.displays.len(),
                to = wanted.len(),
                "multi-monitor: releasing extra displays the layout doesn't use"
            );
            self.displays.truncate(wanted.len());
        }
        for (i, m) in wanted.iter().enumerate() {
            let size = (u32::from(m.width), u32::from(m.height));
            if let Some(vd) = self.displays.get_mut(i) {
                if vd.mode_px() != size {
                    vd.resize(size.0, size.1, 1)
                        .with_context(|| format!("re-moding extra display {i}"))?;
                }
            } else {
                let vd =
                    VirtualDisplay::new_with_serial(size.0, size.1, 60, false, 1, extra_serial(i))
                        .with_context(|| format!("creating extra display {i}"))?;
                tracing::info!(
                    index = i,
                    display_id = vd.display_id(),
                    width = size.0,
                    height = size.1,
                    "multi-monitor: extra display attached"
                );
                self.displays.push(vd);
            }
        }
        self.arrange(layout, primary)
    }

    /// Arranges every display of the session (the main one included) like
    /// the client's monitors, with the layout's top-left just right of every
    /// display that isn't ours, so none of them overlaps the Mac's own panels.
    fn arrange(&mut self, layout: &Layout, primary: &mut VirtualDisplay) -> Result<()> {
        if self.displays.is_empty() {
            return Ok(());
        }
        let primary_index = layout.primary();
        let mut ours = self.ids();
        ours.push(primary.display_id());
        let anchor = (crate::virtual_display::right_edge_excluding(&ours), 0.0);
        let origins = layout.display_origins(anchor, 1.0);
        let mut extras = self.displays.iter();
        let placements: Vec<(u32, i32, i32)> = origins
            .iter()
            .enumerate()
            .filter_map(|(i, &(x, y))| {
                let id = if i == primary_index {
                    primary.display_id()
                } else {
                    extras.next()?.display_id()
                };
                Some((id, x.round() as i32, y.round() as i32))
            })
            .collect();
        crate::virtual_display::arrange_displays(&placements)?;
        primary.refresh_bounds();
        for vd in &mut self.displays {
            vd.refresh_bounds();
        }
        tracing::info!(
            ?placements,
            "multi-monitor: arranged the session's displays"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extra_serials_differ_from_the_main_display_and_each_other() {
        let main = std::process::id();
        let serials: Vec<_> = (0..15).map(extra_serial).collect();
        assert!(serials.iter().all(|&s| s != main));
        let mut unique = serials.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), serials.len());
    }

    #[test]
    fn the_extra_displays_follow_the_non_primary_monitors() {
        use ironrdp_pdu::gcc::{Monitor, MonitorFlags};
        let m = |left, width, primary| Monitor {
            left,
            top: 0,
            right: left + width - 1,
            bottom: 767,
            flags: if primary {
                MonitorFlags::PRIMARY
            } else {
                MonitorFlags::empty()
            },
        };
        // Primary second in the client's list: the extras keep the others' order.
        let layout = Layout::from_client(&[
            m(-1366, 1366, false),
            m(0, 1920, true),
            m(1920, 1024, false),
        ])
        .unwrap();
        let widths: Vec<_> = others(&layout).iter().map(|m| m.width).collect();
        assert_eq!(widths, vec![1366, 1024]);
    }
}
