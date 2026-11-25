//! `--main-display-while-connected`: while a client is connected, the
//! session's primary display is the Mac's main display (the one at (0,0)), so
//! the Dock, the menu bar and new windows appear on the remote primary. On the
//! last disconnect the Mac's own main display goes back to (0,0).
//!
//! Plain `--virtual-display` mode keeps the Mac's own screen main, so the Dock
//! and menu bar stay there and the client never sees them (window-follow moves
//! windows, not those). The headless modes move the virtual display to (0,0)
//! too, but also hide the physical screen; this leaves it on, as a secondary
//! display.
//!
//! The moves run in the display-arrange helper process
//! (`virtual_display::arrange_displays`): configuring displays in macrdp's own
//! process froze its view of them (multi-monitor, 2026-11-25). A crash is
//! safe: the virtual displays go with macrdp, and macOS makes the remaining
//! physical display main again.

/// A display's rectangle in the global display space (points).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placed {
    pub id: u32,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// Where everything goes while connected: the session's displays at their
/// positions relative to the session's primary (which is at (0,0) in
/// `session`), and the Mac's own displays, keeping their arrangement, to the
/// right of them, top-aligned with the session.
pub fn while_connected(session: &[Placed], physical: &[Placed]) -> Vec<(u32, i32, i32)> {
    let mut out: Vec<_> = session.iter().map(|d| (d.id, d.x, d.y)).collect();
    let right = session.iter().map(|d| d.x + d.w).max().unwrap_or(0);
    let top = session.iter().map(|d| d.y).min().unwrap_or(0);
    out.extend(shifted_to(physical, right, top));
    out
}

/// Where everything goes after the session: the Mac's main display
/// (`physical_main`, or the first physical display if it's gone) back at
/// (0,0) with the others around it as they were, and the session's displays,
/// keeping their arrangement, to the right of them. Without a physical display
/// nothing moves.
pub fn after_session(
    session: &[Placed],
    physical: &[Placed],
    physical_main: u32,
) -> Vec<(u32, i32, i32)> {
    let Some(main) = physical
        .iter()
        .find(|d| d.id == physical_main)
        .or_else(|| physical.first())
    else {
        return Vec::new();
    };
    let (dx, dy) = (-main.x, -main.y);
    let mut out: Vec<_> = physical
        .iter()
        .map(|d| (d.id, d.x + dx, d.y + dy))
        .collect();
    let right = physical.iter().map(|d| d.x + dx + d.w).max().unwrap_or(0);
    let top = physical.iter().map(|d| d.y + dy).min().unwrap_or(0);
    out.extend(shifted_to(session, right, top));
    out
}

/// `displays` moved together so their bounding box's top-left is `(left, top)`.
fn shifted_to(displays: &[Placed], left: i32, top: i32) -> Vec<(u32, i32, i32)> {
    let (Some(min_x), Some(min_y)) = (
        displays.iter().map(|d| d.x).min(),
        displays.iter().map(|d| d.y).min(),
    ) else {
        return Vec::new();
    };
    displays
        .iter()
        .map(|d| (d.id, d.x - min_x + left, d.y - min_y + top))
        .collect()
}

/// Only the moves that change something (`current` holds the displays as they
/// are now).
pub fn changes(placements: &[(u32, i32, i32)], current: &[Placed]) -> Vec<(u32, i32, i32)> {
    placements
        .iter()
        .copied()
        .filter(|&(id, x, y)| {
            current
                .iter()
                .find(|d| d.id == id)
                .is_none_or(|d| (d.x, d.y) != (x, y))
        })
        .collect()
}

#[cfg(target_os = "macos")]
mod imp {
    use std::sync::Mutex;

    use anyhow::Result;
    use core_graphics::display::CGDisplay;

    use super::{after_session, changes, while_connected, Placed};

    /// The session's displays from the last arrangement (id, position
    /// relative to the primary): for the restore, and for re-applying the
    /// arrangement when a client connects (see `reapply`).
    static SESSION: Mutex<Vec<(u32, i32, i32)>> = Mutex::new(Vec::new());
    /// The Mac's own main display, recorded at startup before any virtual
    /// display existed.
    static PHYSICAL_MAIN: Mutex<Option<u32>> = Mutex::new(None);

    pub fn set_physical_main(id: u32) {
        *PHYSICAL_MAIN.lock().unwrap_or_else(|e| e.into_inner()) = Some(id);
    }

    fn rect(id: u32) -> Placed {
        let b = CGDisplay::new(id).bounds();
        Placed {
            id,
            x: b.origin.x.round() as i32,
            y: b.origin.y.round() as i32,
            w: b.size.width.round() as i32,
            h: b.size.height.round() as i32,
        }
    }

    fn physical(session: &[u32]) -> Vec<Placed> {
        crate::virtual_display::online_display_ids()
            .into_iter()
            .filter(|id| !session.contains(id))
            .map(rect)
            .collect()
    }

    /// Makes the session's primary the main display: `session` lists the
    /// session's displays with their position relative to the primary (the
    /// primary at (0,0)). True if anything moved.
    pub fn arrange_as_main(session: &[(u32, i32, i32)]) -> Result<bool> {
        let ids: Vec<u32> = session.iter().map(|&(id, _, _)| id).collect();
        *SESSION.lock().unwrap_or_else(|e| e.into_inner()) = session.to_vec();
        let placed: Vec<Placed> = session
            .iter()
            .map(|&(id, x, y)| Placed { x, y, ..rect(id) })
            .collect();
        let physical = physical(&ids);
        let mut current: Vec<Placed> = ids.iter().copied().map(rect).collect();
        current.extend(physical.iter().copied());
        let moves = changes(&while_connected(&placed, &physical), &current);
        if moves.is_empty() {
            return Ok(false);
        }
        crate::virtual_display::arrange_displays_for_session(&moves)?;
        tracing::info!(
            ?moves,
            "the session's primary is the main display while connected"
        );
        Ok(true)
    }

    /// Re-applies the last arrangement when a client connects. The arrangement
    /// is made when the session's displays are set up, which can come before
    /// a restore for the previous disconnect (a reconnect whose capture start
    /// outlasts the session watcher's grace); without this the session would
    /// run without the Dock and menu bar. Moves nothing when already in place.
    pub fn reapply() {
        let session = SESSION.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if session.is_empty() {
            return;
        }
        let session: Vec<(u32, i32, i32)> = session
            .into_iter()
            .map(|(id, x, y)| (crate::virtual_display::current_id(id), x, y))
            .collect();
        if let Err(e) = arrange_as_main(&session) {
            tracing::warn!(
                error = format!("{e:#}"),
                "re-applying the main display failed"
            );
        }
    }

    /// Gives the main display back to the Mac after the last client left.
    /// The session's record is kept, so a failed restore is retried by the
    /// next disconnect, and a reconnect can re-apply it.
    pub fn restore() {
        let ids: Vec<u32> = SESSION
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|&(id, _, _)| crate::virtual_display::current_id(id))
            .collect();
        if ids.is_empty() {
            return;
        }
        let Some(physical_main) = *PHYSICAL_MAIN.lock().unwrap_or_else(|e| e.into_inner()) else {
            return;
        };
        let session: Vec<Placed> = ids.iter().copied().map(rect).collect();
        let physical = physical(&ids);
        let mut current = session.clone();
        current.extend(physical.iter().copied());
        let moves = changes(&after_session(&session, &physical, physical_main), &current);
        if moves.is_empty() {
            return;
        }
        match crate::virtual_display::arrange_displays_for_session(&moves) {
            Ok(()) => tracing::info!(?moves, "the Mac's own display is main again"),
            Err(e) => tracing::warn!(
                error = format!("{e:#}"),
                "giving the main display back failed"
            ),
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use anyhow::Result;

    pub fn set_physical_main(_id: u32) {}

    pub fn arrange_as_main(_session: &[(u32, i32, i32)]) -> Result<bool> {
        Ok(false)
    }

    pub fn reapply() {}

    pub fn restore() {}
}

pub use imp::{arrange_as_main, reapply, restore, set_physical_main};

/// The session watcher's guard (see `spawn_primary_overlay_watcher`). The
/// arrangement is made when the session's displays are set up; installing the
/// guard on connect re-applies it in case a late restore undid it (see
/// [`reapply`]), and dropping it on the last disconnect gives the main
/// display back.
pub struct Guard;

impl Guard {
    pub fn install(_vd_id: u32) -> anyhow::Result<Self> {
        reapply();
        Ok(Self)
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(id: u32, x: i32, y: i32, w: i32, h: i32) -> Placed {
        Placed { id, x, y, w, h }
    }

    #[test]
    fn one_display_becomes_main_and_the_mac_moves_aside() {
        // The vd at its creation spot right of the Mac's 1920x1080 screen.
        let session = [d(9, 0, 0, 1366, 768)];
        let physical = [d(1, 0, 0, 1920, 1080)];
        assert_eq!(
            while_connected(&session, &physical),
            vec![(9, 0, 0), (1, 1366, 0)]
        );
    }

    #[test]
    fn a_two_monitor_session_keeps_its_layout_with_the_primary_at_origin() {
        // Primary 1920x1080, a 1366x768 monitor to its left.
        let session = [d(9, 0, 0, 1920, 1080), d(10, -1366, 0, 1366, 768)];
        let physical = [d(1, 0, 0, 1920, 1080)];
        assert_eq!(
            while_connected(&session, &physical),
            vec![(9, 0, 0), (10, -1366, 0), (1, 1920, 0)]
        );
    }

    #[test]
    fn the_mac_goes_right_of_a_monitor_right_of_the_primary() {
        // The Windows PC's layout: 1920x1080 primary, 1366x768 to its right.
        let session = [d(9, 0, 0, 1920, 1080), d(10, 1920, 0, 1366, 768)];
        let physical = [d(1, 0, 0, 1920, 1080)];
        assert_eq!(
            while_connected(&session, &physical),
            vec![(9, 0, 0), (10, 1920, 0), (1, 3286, 0)]
        );
    }

    #[test]
    fn the_macs_own_displays_keep_their_arrangement() {
        let session = [d(9, 0, 0, 1920, 1080)];
        let physical = [d(1, 0, 0, 1512, 982), d(2, 1512, -200, 2560, 1440)];
        assert_eq!(
            while_connected(&session, &physical),
            vec![(9, 0, 0), (1, 1920, 200), (2, 3432, 0)]
        );
    }

    #[test]
    fn after_the_session_the_mac_is_main_and_the_session_moves_right() {
        let session = [d(9, 0, 0, 1920, 1080), d(10, -1366, 0, 1366, 768)];
        let physical = [d(1, 1920, 0, 1920, 1080)];
        assert_eq!(
            after_session(&session, &physical, 1),
            vec![(1, 0, 0), (9, 3286, 0), (10, 1920, 0)]
        );
    }

    #[test]
    fn without_the_recorded_main_the_first_physical_display_is_main() {
        let session = [d(9, 0, 0, 1920, 1080)];
        let physical = [d(2, 1920, 0, 1280, 720)];
        assert_eq!(
            after_session(&session, &physical, 1),
            vec![(2, 0, 0), (9, 1280, 0)]
        );
        // No physical display at all (a closed MacBook lid): nothing moves.
        assert!(after_session(&session, &[], 1).is_empty());
    }

    #[test]
    fn only_moves_that_change_something_are_kept() {
        let current = [d(9, 0, 0, 1920, 1080), d(1, 1920, 0, 1920, 1080)];
        assert!(changes(&[(9, 0, 0), (1, 1920, 0)], &current).is_empty());
        assert_eq!(
            changes(&[(9, 0, 0), (1, 2000, 0)], &current),
            vec![(1, 2000, 0)]
        );
    }
}
