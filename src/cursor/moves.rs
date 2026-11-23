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
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How many recent client-caused cursor positions are kept to recognise an
/// echo. The cursor poll reads the position after some of the client's moves
/// were posted, so it can see any of the last few, not only the newest one.
const RECENT_CLIENT_MOVES: usize = 32;

/// An older client move counts as the cause of the cursor position only this
/// long. Later, the same position comes from a local move and must be sent.
const CLIENT_MOVE_WINDOW: Duration = Duration::from_millis(250);

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

    /// Remember that client input moved the Mac cursor to `mac`.
    pub fn note_at(&mut self, mac: (f64, f64), now: Instant) {
        if let Some(last) = self.recent.back_mut() {
            if last.0 == mac {
                last.1 = now;
                return;
            }
        }
        if self.recent.len() == RECENT_CLIENT_MOVES {
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

static CLIENT_MOVES: Mutex<ClientMoves> = Mutex::new(ClientMoves::new());

fn moves() -> std::sync::MutexGuard<'static, ClientMoves> {
    CLIENT_MOVES.lock().unwrap_or_else(|e| e.into_inner())
}

/// The input handler moved the Mac cursor to `mac` (global points) for the client.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn note_client_move(mac: (f64, f64)) {
    moves().note_at(mac, Instant::now());
}

/// See [`ClientMoves::is_client_move_at`].
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn is_client_move(mac: (f64, f64)) -> bool {
    moves().is_client_move_at(mac, Instant::now())
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
    fn only_recent_client_moves_are_kept() {
        let mut m = ClientMoves::new();
        let t0 = Instant::now();
        for i in 0..=RECENT_CLIENT_MOVES {
            m.note_at((i as f64 * 10.0, 0.0), t0);
        }
        assert!(!m.is_client_move_at((0.0, 0.0), t0));
        assert!(m.is_client_move_at((10.0, 0.0), t0));
        assert!(m.is_client_move_at((RECENT_CLIENT_MOVES as f64 * 10.0, 0.0), t0));
    }
}
