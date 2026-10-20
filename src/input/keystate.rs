//! Pure keyboard state for the input handler: the client's Num Lock.
//!
//! No platform calls, so it is unit-tested on every target. Ported from
//! macrdp-v1 (crates/macrdp-input/src/flags.rs and keyboard.rs, commits
//! d74b3cf, f708d80, 41eca74, d7a9e68, 236fb8b), relicensed to MIT OR
//! Apache-2.0 by the author.

/// RDP scancode of Num Lock (non-extended).
pub(super) const SCANCODE_NUM_LOCK: u8 = 0x45;

/// The client's Num Lock state, as Windows keeps it. macOS has no Num Lock:
/// the server keeps it so that the keypad keys move the cursor when it is
/// off, as on Windows (`scancodes::keypad_navigation_vk`).
#[derive(Debug)]
pub(super) struct NumLock {
    on: bool,
    /// A Num Lock key-down was posted and its key-up was not.
    key_down: bool,
}

impl Default for NumLock {
    /// On until the client's Synchronize says otherwise: keypad keys type
    /// digits, as before this state existed.
    fn default() -> Self {
        Self {
            on: true,
            key_down: false,
        }
    }
}

impl NumLock {
    pub(super) fn is_on(&self) -> bool {
        self.on
    }

    /// The Num Lock key (scancode 0x45, not extended), pressed or released.
    /// A press toggles the state. Returns whether to post the event: a
    /// release is posted only after a posted press.
    pub(super) fn key(&mut self, pressed: bool) -> bool {
        if !pressed {
            return std::mem::take(&mut self.key_down);
        }
        self.on = !self.on;
        self.key_down = true;
        true
    }

    /// RDP Synchronize: the client's Num Lock state.
    pub(super) fn synchronize(&mut self, on: bool) {
        self.on = on;
    }

    /// The posted Num Lock press was released by other means (connection
    /// end): no key-up is due any more. The state stays.
    pub(super) fn forget_key(&mut self) {
        self.key_down = false;
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
}
