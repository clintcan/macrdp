//! A multi-monitor session's layout (docs/multi-monitor-design.md).
//!
//! The client describes its monitors relative to its primary monitor, whose
//! top-left is (0,0); a monitor to its left or above has negative coordinates
//! (MS-RDPBCGR 2.2.1.3.6.1). The session's desktop is the monitors' bounding
//! box, and everything unsigned (the desktop size, EGFX surface origins,
//! bitmap updates, input) is relative to that box's top-left. This module
//! turns the client's monitors into that desktop view and answers the two
//! questions the rest of macrdp asks of it: which monitor a desktop point is
//! on, and where each monitor's virtual display goes on the Mac.
//!
//! Pure data, so it is the same on every platform and fully unit-tested.

use ironrdp_pdu::gcc::{Monitor, MonitorFlags};

/// One monitor of the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionMonitor {
    /// Top-left in desktop coordinates (relative to the bounding box).
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub primary: bool,
}

impl SessionMonitor {
    fn contains(&self, x: u16, y: u16) -> bool {
        let (x, y) = (u32::from(x), u32::from(y));
        let (left, top) = (u32::from(self.x), u32::from(self.y));
        x >= left
            && y >= top
            && x < left + u32::from(self.width)
            && y < top + u32::from(self.height)
    }

    /// Squared distance from a point to this monitor (0 inside it).
    fn distance_sq(&self, x: u16, y: u16) -> u64 {
        let axis = |p: u16, start: u16, len: u16| -> u64 {
            let (p, start) = (i64::from(p), i64::from(start));
            let end = start + i64::from(len) - 1;
            let d = if p < start {
                start - p
            } else {
                (p - end).max(0)
            };
            d.unsigned_abs()
        };
        let dx = axis(x, self.x, self.width);
        let dy = axis(y, self.y, self.height);
        dx * dx + dy * dy
    }
}

/// A session's monitors, in desktop coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    monitors: Vec<SessionMonitor>,
    desktop: (u16, u16),
    /// The client's rectangles as it sent them, for the messages that carry
    /// them unchanged (Monitor Layout PDU, EGFX `RESET_GRAPHICS`).
    client: Vec<Monitor>,
}

impl Layout {
    /// The layout for the client's monitors, as the session adopted them
    /// (`ConnectionInfo::monitor_layout`). `None` for an empty list or one
    /// whose rectangles don't fit the 16-bit desktop.
    pub fn from_client(monitors_in: &[Monitor]) -> Option<Self> {
        let monitors = monitors_in;
        let left = monitors.iter().map(|m| m.left).min()?;
        let top = monitors.iter().map(|m| m.top).min()?;
        let right = monitors.iter().map(|m| m.right).max()?;
        let bottom = monitors.iter().map(|m| m.bottom).max()?;
        let span = |from: i32, to: i32| u16::try_from(to.checked_sub(from)?.checked_add(1)?).ok();
        let desktop = (span(left, right)?, span(top, bottom)?);
        let monitors = monitors
            .iter()
            .map(|m| {
                Some(SessionMonitor {
                    x: u16::try_from(m.left.checked_sub(left)?).ok()?,
                    y: u16::try_from(m.top.checked_sub(top)?).ok()?,
                    width: span(m.left, m.right)?,
                    height: span(m.top, m.bottom)?,
                    primary: m.flags.contains(MonitorFlags::PRIMARY),
                })
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            monitors,
            desktop,
            client: monitors_in.to_vec(),
        })
    }

    /// The layout a multi-monitor session adopts from a mid-session layout
    /// (MS-RDPEDISP), with the rules the acceptor applies at connect
    /// (IronRDP fork, acceptor divergence (8)): 2 to 16 monitors, exactly one
    /// primary at (0,0), each at least 200 pixels and at most `max`, none
    /// overlapping. `None` for anything else.
    pub fn from_mid_session(monitors: &[Monitor], max: (u16, u16)) -> Option<Self> {
        if !(2..=16).contains(&monitors.len()) {
            return None;
        }
        let mut primaries = monitors
            .iter()
            .filter(|m| m.flags.contains(MonitorFlags::PRIMARY));
        match (primaries.next(), primaries.next()) {
            (Some(p), None) if p.left == 0 && p.top == 0 => {}
            _ => return None,
        }
        let layout = Self::from_client(monitors)?;
        let usable = |m: &SessionMonitor| {
            (200..=max.0).contains(&m.width) && (200..=max.1).contains(&m.height)
        };
        if !layout.monitors.iter().all(usable) {
            return None;
        }
        let overlaps = |a: &SessionMonitor, b: &SessionMonitor| {
            let (ax, ay) = (u32::from(a.x), u32::from(a.y));
            let (bx, by) = (u32::from(b.x), u32::from(b.y));
            ax < bx + u32::from(b.width)
                && bx < ax + u32::from(a.width)
                && ay < by + u32::from(b.height)
                && by < ay + u32::from(a.height)
        };
        for (i, a) in layout.monitors.iter().enumerate() {
            if layout.monitors[i + 1..].iter().any(|b| overlaps(a, b)) {
                return None;
            }
        }
        Some(layout)
    }

    pub fn monitors(&self) -> &[SessionMonitor] {
        &self.monitors
    }

    /// The client's rectangles as it sent them (primary-relative).
    pub fn client_monitors(&self) -> &[Monitor] {
        &self.client
    }

    /// The desktop size: the monitors' bounding box.
    pub fn desktop(&self) -> (u16, u16) {
        self.desktop
    }

    /// The primary monitor's index (the first monitor if none is flagged).
    pub fn primary(&self) -> usize {
        self.monitors.iter().position(|m| m.primary).unwrap_or(0)
    }

    /// The monitor a desktop point is on. A point in a gap between monitors
    /// (the bounding box isn't always covered) goes to the nearest monitor,
    /// so a pointer crossing a gap never lands nowhere.
    pub fn monitor_at(&self, x: u16, y: u16) -> usize {
        if let Some(i) = self.monitors.iter().position(|m| m.contains(x, y)) {
            return i;
        }
        self.monitors
            .iter()
            .enumerate()
            .min_by_key(|(_, m)| m.distance_sq(x, y))
            .map_or(0, |(i, _)| i)
    }

    /// Where each monitor's virtual display goes in the Mac's global display
    /// space (points), keeping the client's arrangement: the bounding box's
    /// top-left lands at `anchor`, and `points_per_pixel` converts the
    /// client's pixels (0.5 on a 2x HiDPI display, 1.0 otherwise).
    pub fn display_origins(&self, anchor: (f64, f64), points_per_pixel: f64) -> Vec<(f64, f64)> {
        self.monitors
            .iter()
            .map(|m| {
                (
                    anchor.0 + f64::from(m.x) * points_per_pixel,
                    anchor.1 + f64::from(m.y) * points_per_pixel,
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mon(left: i32, top: i32, width: i32, height: i32, primary: bool) -> Monitor {
        Monitor {
            left,
            top,
            right: left + width - 1,
            bottom: top + height - 1,
            flags: if primary {
                MonitorFlags::PRIMARY
            } else {
                MonitorFlags::empty()
            },
        }
    }

    fn sm(x: u16, y: u16, width: u16, height: u16, primary: bool) -> SessionMonitor {
        SessionMonitor {
            x,
            y,
            width,
            height,
            primary,
        }
    }

    #[test]
    fn side_by_side_monitors_keep_their_positions() {
        let layout =
            Layout::from_client(&[mon(0, 0, 3440, 1440, true), mon(3440, 0, 1920, 1080, false)])
                .unwrap();
        assert_eq!(layout.desktop(), (5360, 1440));
        assert_eq!(
            layout.monitors(),
            &[sm(0, 0, 3440, 1440, true), sm(3440, 0, 1920, 1080, false)]
        );
        assert_eq!(layout.primary(), 0);
    }

    #[test]
    fn a_monitor_left_of_and_above_the_primary_shifts_the_desktop() {
        // Client coordinates: primary at (0,0), the other at (-2560,-360).
        let layout = Layout::from_client(&[
            mon(0, 0, 1920, 1080, true),
            mon(-2560, -360, 2560, 1440, false),
        ])
        .unwrap();
        assert_eq!(layout.desktop(), (4480, 1440));
        assert_eq!(
            layout.monitors(),
            &[sm(2560, 360, 1920, 1080, true), sm(0, 0, 2560, 1440, false)]
        );
        assert_eq!(layout.primary(), 0);
    }

    #[test]
    fn a_point_maps_to_its_monitor_or_the_nearest_one() {
        // Primary 3440x1440, a 1920x1080 to its right, top-aligned: the area
        // below the second monitor (y >= 1080, x >= 3440) is a gap.
        let layout =
            Layout::from_client(&[mon(0, 0, 3440, 1440, true), mon(3440, 0, 1920, 1080, false)])
                .unwrap();
        assert_eq!(layout.monitor_at(0, 0), 0);
        assert_eq!(layout.monitor_at(3439, 1439), 0);
        assert_eq!(layout.monitor_at(3440, 0), 1);
        assert_eq!(layout.monitor_at(5359, 1079), 1);
        // In the gap: just right of the primary's bottom edge, nearer the primary.
        assert_eq!(layout.monitor_at(3445, 1400), 0);
        // In the gap, just under the second monitor: nearer the second.
        assert_eq!(layout.monitor_at(5000, 1085), 1);
    }

    #[test]
    fn display_origins_keep_the_arrangement_at_the_anchor() {
        let layout = Layout::from_client(&[
            mon(0, 0, 1920, 1080, true),
            mon(-2560, -360, 2560, 1440, false),
        ])
        .unwrap();
        assert_eq!(
            layout.display_origins((1512.0, 0.0), 1.0),
            vec![(1512.0 + 2560.0, 360.0), (1512.0, 0.0)]
        );
        // A 2x HiDPI display: half the pixels in points.
        assert_eq!(
            layout.display_origins((0.0, 0.0), 0.5),
            vec![(1280.0, 180.0), (0.0, 0.0)]
        );
    }

    #[test]
    fn a_mid_session_layout_follows_the_connect_rules() {
        let max = (8192, 8192);
        // The layout Windows App sent live (2026-11-25): a 1280x720 secondary.
        let live = [mon(1920, 0, 1280, 720, false), mon(0, 0, 1920, 1080, true)];
        let layout = Layout::from_mid_session(&live, max).unwrap();
        assert_eq!(layout.desktop(), (3200, 1080));
        assert_eq!(layout.client_monitors(), &live);
        // One monitor: not a multi-monitor layout.
        assert_eq!(
            Layout::from_mid_session(&[mon(0, 0, 1920, 1080, true)], max),
            None
        );
        // No primary, two primaries, a primary away from (0,0).
        assert_eq!(
            Layout::from_mid_session(
                &[mon(0, 0, 1920, 1080, false), mon(1920, 0, 800, 600, false)],
                max
            ),
            None
        );
        assert_eq!(
            Layout::from_mid_session(
                &[mon(0, 0, 1920, 1080, true), mon(1920, 0, 800, 600, true)],
                max
            ),
            None
        );
        assert_eq!(
            Layout::from_mid_session(
                &[mon(10, 0, 1920, 1080, true), mon(1930, 0, 800, 600, false)],
                max
            ),
            None
        );
        // Too small, larger than the maximum.
        assert_eq!(
            Layout::from_mid_session(
                &[mon(0, 0, 1920, 1080, true), mon(1920, 0, 199, 600, false)],
                max
            ),
            None
        );
        assert_eq!(
            Layout::from_mid_session(
                &[mon(0, 0, 1920, 1080, true), mon(1920, 0, 2560, 1440, false)],
                (1920, 1080)
            ),
            None
        );
        // Overlapping; touching edges are fine.
        assert_eq!(
            Layout::from_mid_session(
                &[mon(0, 0, 1920, 1080, true), mon(1919, 0, 800, 600, false)],
                max
            ),
            None
        );
        assert!(Layout::from_mid_session(
            &[mon(0, 0, 1920, 1080, true), mon(-800, 0, 800, 600, false)],
            max
        )
        .is_some());
    }

    #[test]
    fn unusable_input_has_no_layout() {
        assert_eq!(Layout::from_client(&[]), None);
        // Wider than a 16-bit desktop.
        assert_eq!(
            Layout::from_client(&[
                mon(0, 0, 40_000, 1080, true),
                mon(40_000, 0, 40_000, 1080, false)
            ]),
            None
        );
        // An inverted rectangle.
        let inverted = Monitor {
            left: 100,
            top: 0,
            right: 0,
            bottom: 99,
            flags: MonitorFlags::PRIMARY,
        };
        assert_eq!(Layout::from_client(&[inverted]), None);
    }
}
