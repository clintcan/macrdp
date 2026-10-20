//! Pure keyboard state for the input handler: the client's Num Lock.
//!
//! No platform calls, so it is unit-tested on every target. Ported from
//! macrdp-v1 (crates/macrdp-input/src/flags.rs and keyboard.rs, commits
//! d74b3cf, f708d80, 41eca74, d7a9e68, 236fb8b), relicensed to MIT OR
//! Apache-2.0 by the author.

/// RDP scancode of Num Lock (non-extended).
pub(super) const SCANCODE_NUM_LOCK: u8 = 0x45;

/// RDP scancode of Left Ctrl (non-extended; Right Ctrl is the extended one).
const SCANCODE_LEFT_CTRL: u8 = 0x1D;

/// The client's Num Lock state, as Windows keeps it. macOS has no Num Lock:
/// the server keeps it so that the keypad keys move the cursor when it is
/// off, as on Windows (`scancodes::keypad_navigation_vk`).
#[derive(Debug)]
pub(super) struct NumLock {
    on: bool,
    /// A Num Lock key-down was posted and its key-up was not.
    key_down: bool,
    /// The client's Left Ctrl (scancode 0x1D, not extended) is down.
    left_ctrl: bool,
}

impl Default for NumLock {
    /// On until the client's Synchronize says otherwise: keypad keys type
    /// digits, as before this state existed.
    fn default() -> Self {
        Self {
            on: true,
            key_down: false,
            left_ctrl: false,
        }
    }
}

impl NumLock {
    pub(super) fn is_on(&self) -> bool {
        self.on
    }

    /// Every scancode event, so that Pause can be told from Num Lock.
    pub(super) fn note_scancode(&mut self, scancode: u8, extended: bool, pressed: bool) {
        if scancode == SCANCODE_LEFT_CTRL && !extended {
            self.left_ctrl = pressed;
        }
    }

    /// Scancode 0x45 (not extended), pressed or released. A Num Lock press
    /// toggles the state. Returns whether to post the event (as Num Lock): a
    /// release is posted only after a posted press.
    ///
    /// mstsc and FreeRDP send Pause as Left Ctrl with KBDFLAGS_EXTENDED1, then
    /// 0x45 (MS-RDPBCGR 2.2.8.1.1.3.1.1.1). The server library passes on only
    /// the extended flag, so Pause looks like Ctrl+Num Lock, which Windows
    /// also reads as Pause. Pause is not posted (macOS has no Pause key) and
    /// does not toggle Num Lock.
    pub(super) fn key(&mut self, pressed: bool) -> bool {
        if !pressed {
            return std::mem::take(&mut self.key_down);
        }
        if self.left_ctrl {
            return false;
        }
        self.on = !self.on;
        self.key_down = true;
        true
    }

    /// RDP Synchronize: the client's Num Lock state; all keys are up.
    pub(super) fn synchronize(&mut self, on: bool) {
        self.on = on;
        self.left_ctrl = false;
    }

    /// The posted Num Lock press was released by other means (connection
    /// end): no key-up is due any more, and no client key is down. The state
    /// stays.
    pub(super) fn forget_key(&mut self) {
        self.key_down = false;
        self.left_ctrl = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn num_lock_starts_on_and_follows_synchronize() {
        let mut n = NumLock::default();
        assert!(n.is_on());
        n.synchronize(false);
        assert!(!n.is_on());
        n.synchronize(true);
        assert!(n.is_on());
    }

    #[test]
    fn num_lock_key_toggles_on_press_only() {
        let mut n = NumLock::default();
        assert!(n.key(true));
        assert!(!n.is_on());
        assert!(n.key(false));
        assert!(!n.is_on());
        assert!(n.key(true));
        assert!(n.is_on());
    }

    #[test]
    fn a_release_without_a_posted_press_is_not_posted() {
        let mut n = NumLock::default();
        assert!(!n.key(false));
        assert!(n.key(true));
        n.forget_key();
        assert!(!n.key(false), "the connection end already released it");
        assert!(!n.is_on(), "the state stays");
    }

    /// mstsc Pause: Left Ctrl (EXTENDED1) down, 0x45 down, Ctrl up, 0x45 up.
    #[test]
    fn pause_is_not_num_lock() {
        let mut n = NumLock::default();
        n.note_scancode(0x1D, false, true);
        assert!(!n.key(true), "Pause is not posted");
        n.note_scancode(0x1D, false, false);
        assert!(!n.key(false));
        assert!(n.is_on(), "Pause does not toggle Num Lock");
        // The next Num Lock press is Num Lock again.
        assert!(n.key(true));
        assert!(!n.is_on());
    }

    #[test]
    fn right_ctrl_is_not_the_pause_prefix() {
        let mut n = NumLock::default();
        n.note_scancode(0x1D, true, true);
        assert!(n.key(true));
        assert!(!n.is_on());
    }

    #[test]
    fn synchronize_forgets_a_held_left_ctrl() {
        let mut n = NumLock::default();
        n.note_scancode(0x1D, false, true);
        n.synchronize(true);
        assert!(n.key(true), "Ctrl went up with the Synchronize");
    }
}
