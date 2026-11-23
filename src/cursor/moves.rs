//! Pointer positions that the client's own input caused.
//!
//! The server sends a Pointer Position Update (MS-RDPBCGR 2.2.9.1.1.4.2) only
//! when the Mac cursor moved by itself (an app warped it, a local mouse or
//! trackpad moved it). A position that client input caused must not go back
//! to the client: the client already shows its cursor there, it moves it
//! locally ahead of the server, and a late echo makes the cursor jump back.
//! When the client scales the picture (mstsc smart sizing), an echo is also
//! rounded and sent back as a new move, so the cursor creeps (up to ~47 px
//! seen with mstsc in macrdp-v1).
//!
//! Positions are global display points (CGEvent coordinates) as posted by the
//! input handler. Process-wide: one connection is served at a time, and a new
//! connection forgets the old one's positions ([`forget`]).
//!
//! Ported from rophy/macrdp 9564c90, from macrdp-v1 f52e92e, 8dc1dfb
//! (`note_client_move`, `is_client_move`, `forget_client_moves` and their
//! tests), relicensed to MIT OR Apache-2.0 by the author. Points are `f64`
//! and match within [`SAME_POINT`] (macrdp posts fractional points).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A client move counts as the cause of the cursor position this long (the
/// newest one always counts). All client moves in this window are kept: the
/// cursor poll (every ~66 ms) can see any of them, and a fast mouse posts
/// many per poll.
const CLIENT_MOVE_WINDOW: Duration = Duration::from_millis(250);

/// At most this many client moves are kept (a 1000 Hz mouse posts 250 in
/// [`CLIENT_MOVE_WINDOW`]).
const MAX_CLIENT_MOVES: usize = 1024;

/// Two positions this close (points, each axis) are the same: the
/// WindowServer may round a posted fractional point.
const SAME_POINT: f64 = 1.0;

/// Mac positions the client's own input moved the cursor to (newest last).
#[derive(Debug, Default)]
pub struct ClientMoves {
    recent: VecDeque<((f64, f64), Instant)>,
}

fn same(a: (f64, f64), b: (f64, f64)) -> bool {
    (a.0 - b.0).abs() <= SAME_POINT && (a.1 - b.1).abs() <= SAME_POINT
}

impl ClientMoves {
    pub const fn new() -> Self {
        Self {
            recent: VecDeque::new(),
        }
    }

    /// Remember that client input moved the Mac cursor to `mac`. Moves older
    /// than [`CLIENT_MOVE_WINDOW`] are dropped.
    pub fn note_at(&mut self, mac: (f64, f64), now: Instant) {
        while self
            .recent
            .front()
            .is_some_and(|(_, at)| now.duration_since(*at) > CLIENT_MOVE_WINDOW)
        {
            self.recent.pop_front();
        }
        if let Some(last) = self.recent.back_mut() {
            if last.0 == mac {
                last.1 = now;
                return;
            }
        }
        if self.recent.len() == MAX_CLIENT_MOVES {
            self.recent.pop_front();
        }
        self.recent.push_back((mac, now));
    }

    /// Whether the Mac cursor at `mac` was put there by recent client input.
    /// The newest client position always counts (the client shows its cursor
    /// there). An older one counts only within [`CLIENT_MOVE_WINDOW`], so a
    /// local move to a place the client visited before is still sent.
    pub fn is_client_move_at(&self, mac: (f64, f64), now: Instant) -> bool {
        let mut moves = self.recent.iter().rev();
        if moves.next().is_some_and(|(pos, _)| same(*pos, mac)) {
            return true;
        }
        moves.any(|(pos, at)| same(*pos, mac) && now.duration_since(*at) <= CLIENT_MOVE_WINDOW)
    }

    /// Forget the client's recent positions: after a Pointer Position Update
    /// is sent (the client cursor is now at the server's position) and when a
    /// new connection starts.
    pub fn forget(&mut self) {
        self.recent.clear();
    }
}

/// The captured display in global points (CGEvent coordinates).
#[derive(Debug, Clone, Copy)]
pub struct DisplayRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// The Mac cursor position of one display stream, and when it goes to the client.
#[derive(Debug, Default)]
pub struct PositionPoll {
    /// Last Mac cursor position seen (global points).
    last: Option<(f64, f64)>,
}

impl PositionPoll {
    /// The session position (pixels) to send in a Pointer Position Update for
    /// the Mac cursor at `pos`, or `None`. Sent only when the position changed
    /// and the client's input did not cause it (`moves`). Not sent: the first
    /// position of the stream (the client shows its cursor where its user left
    /// it), and a position outside `display()` (the client cannot show it).
    /// After a send, `moves` is cleared: the client cursor is now at the
    /// server's position.
    pub fn decide(
        &mut self,
        pos: (f64, f64),
        moves: &mut ClientMoves,
        now: Instant,
        display: impl FnOnce() -> DisplayRect,
        desktop: (u16, u16),
        letterbox: bool,
    ) -> Option<(u16, u16)> {
        let first = self.last.is_none();
        if self.last == Some(pos) {
            return None;
        }
        self.last = Some(pos);
        if first || moves.is_client_move_at(pos, now) {
            return None;
        }
        let d = display();
        let (mx, my) = (pos.0 - d.x, pos.1 - d.y);
        if mx < 0.0 || my < 0.0 || mx >= d.w || my >= d.h {
            return None;
        }
        let (x, y) =
            crate::input::map_display_to_client(mx, my, desktop.0, desktop.1, d.w, d.h, letterbox);
        moves.forget();
        Some((x, y))
    }
}

/// `--enable-mac-pointer-moves`. Off: no client move is recorded and no
/// Pointer Position Update is sent.
static ENABLED: AtomicBool = AtomicBool::new(false);

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

static CLIENT_MOVES: Mutex<ClientMoves> = Mutex::new(ClientMoves::new());

fn moves() -> std::sync::MutexGuard<'static, ClientMoves> {
    CLIENT_MOVES.lock().unwrap_or_else(|e| e.into_inner())
}

/// The input handler moved the Mac cursor to `mac` (global points) for the client.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn note_client_move(mac: (f64, f64)) {
    if enabled() {
        moves().note_at(mac, Instant::now());
    }
}

/// [`PositionPoll::decide`] with the process-wide client moves.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn decide_position(
    poll: &mut PositionPoll,
    pos: (f64, f64),
    display: impl FnOnce() -> DisplayRect,
    desktop: (u16, u16),
    letterbox: bool,
) -> Option<(u16, u16)> {
    poll.decide(
        pos,
        &mut moves(),
        Instant::now(),
        display,
        desktop,
        letterbox,
    )
}

/// See [`ClientMoves::forget`].
pub fn forget() {
    moves().forget();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_moves_are_recognised_as_echo() {
        let mut m = ClientMoves::new();
        let t0 = Instant::now();
        m.note_at((625.0, 190.0), t0);
        m.note_at((626.0, 191.0), t0);
        let t1 = t0 + Duration::from_millis(16);
        assert!(m.is_client_move_at((625.0, 190.0), t1));
        assert!(m.is_client_move_at((626.0, 191.0), t1));
        // The WindowServer may round a fractional posted point.
        assert!(m.is_client_move_at((625.5, 190.4), t1));
        // A position the client never caused (local user, app warp) is not an echo.
        assert!(!m.is_client_move_at((625.0, 188.0), t1));
    }

    #[test]
    fn a_local_move_back_to_the_client_position_is_sent() {
        // Client moves to P and stops; a local move to Q is sent; later a
        // local move back to P must be sent too (the client shows its cursor at Q).
        let mut m = ClientMoves::new();
        let t0 = Instant::now();
        let (p, q) = ((640.0, 360.0), (900.0, 300.0));
        m.note_at(p, t0);
        assert!(!m.is_client_move_at(q, t0 + Duration::from_secs(1)));
        m.forget(); // the poll sent Q
        assert!(!m.is_client_move_at(p, t0 + Duration::from_secs(2)));
    }

    #[test]
    fn an_old_client_position_is_not_an_echo() {
        let mut m = ClientMoves::new();
        let t0 = Instant::now();
        m.note_at((640.0, 360.0), t0);
        m.note_at((102.0, 458.0), t0 + Duration::from_millis(10));
        // The poll may still see the older position shortly after.
        assert!(m.is_client_move_at((640.0, 360.0), t0 + Duration::from_millis(100)));
        // Seconds later a local move back to (640, 360) must reach the client.
        assert!(!m.is_client_move_at((640.0, 360.0), t0 + Duration::from_secs(5)));
        // The newest client position never expires.
        assert!(m.is_client_move_at((102.0, 458.0), t0 + Duration::from_secs(5)));
    }

    #[test]
    fn all_client_moves_in_the_window_are_kept() {
        // A fast mouse: many moves between two cursor polls.
        let mut m = ClientMoves::new();
        let t0 = Instant::now();
        for i in 0..200 {
            m.note_at((i as f64 * 10.0, 0.0), t0 + Duration::from_millis(i / 2));
        }
        let poll = t0 + Duration::from_millis(150);
        assert!(m.is_client_move_at((0.0, 0.0), poll));
        assert!(m.is_client_move_at((990.0, 0.0), poll));
        // Older than the window: dropped at the next move.
        m.note_at((5000.0, 0.0), t0 + Duration::from_millis(400));
        assert_eq!(m.recent.len(), 1);
    }

    #[test]
    fn at_most_max_client_moves_are_kept() {
        let mut m = ClientMoves::new();
        let t0 = Instant::now();
        for i in 0..=MAX_CLIENT_MOVES {
            m.note_at((i as f64 * 10.0, 0.0), t0);
        }
        assert_eq!(m.recent.len(), MAX_CLIENT_MOVES);
        assert!(!m.is_client_move_at((0.0, 0.0), t0));
        assert!(m.is_client_move_at((10.0, 0.0), t0));
    }

    const DISPLAY: DisplayRect = DisplayRect {
        x: 0.0,
        y: 0.0,
        w: 1280.0,
        h: 720.0,
    };

    fn decide(
        p: &mut PositionPoll,
        m: &mut ClientMoves,
        pos: (f64, f64),
        now: Instant,
    ) -> Option<(u16, u16)> {
        p.decide(pos, m, now, || DISPLAY, (960, 540), true)
    }

    #[test]
    fn the_first_position_is_not_sent() {
        let (mut p, mut m, t0) = (PositionPoll::default(), ClientMoves::new(), Instant::now());
        assert_eq!(decide(&mut p, &mut m, (100.0, 100.0), t0), None);
        assert_eq!(decide(&mut p, &mut m, (100.0, 100.0), t0), None);
        assert_eq!(
            decide(&mut p, &mut m, (1100.0, 600.0), t0),
            Some((825, 450))
        );
    }

    #[test]
    fn a_client_move_is_not_sent() {
        let (mut p, mut m, t0) = (PositionPoll::default(), ClientMoves::new(), Instant::now());
        decide(&mut p, &mut m, (640.0, 360.0), t0);
        m.note_at((700.0, 400.0), t0);
        assert_eq!(decide(&mut p, &mut m, (700.0, 400.0), t0), None);
    }

    #[test]
    fn a_position_on_another_display_is_not_sent() {
        let (mut p, mut m, t0) = (PositionPoll::default(), ClientMoves::new(), Instant::now());
        decide(&mut p, &mut m, (640.0, 360.0), t0);
        assert_eq!(decide(&mut p, &mut m, (1280.0, 360.0), t0), None);
        assert_eq!(decide(&mut p, &mut m, (-1.0, 360.0), t0), None);
        assert_eq!(decide(&mut p, &mut m, (640.0, 720.0), t0), None);
        // Back on the display: sent.
        assert_eq!(decide(&mut p, &mut m, (0.0, 0.0), t0), Some((0, 0)));
    }

    #[test]
    fn a_send_forgets_the_client_moves() {
        let (mut p, mut m, t0) = (PositionPoll::default(), ClientMoves::new(), Instant::now());
        decide(&mut p, &mut m, (640.0, 360.0), t0);
        m.note_at((640.0, 360.0), t0);
        assert_eq!(decide(&mut p, &mut m, (100.0, 100.0), t0), Some((75, 75)));
        assert!(m.recent.is_empty());
        // The Mac cursor goes back where the client left it: the client shows
        // its cursor at (75, 75) now, so this is sent too.
        assert_eq!(decide(&mut p, &mut m, (640.0, 360.0), t0), Some((480, 270)));
    }
}
