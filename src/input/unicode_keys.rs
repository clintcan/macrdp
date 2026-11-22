//! RDP Unicode keyboard events (MS-RDPBCGR `TS_UNICODE_KEYBOARD_EVENT`) carry
//! one UTF-16 unit each, with a key down and a key up. A character outside the
//! BMP (an emoji) comes as a high and a low surrogate in two events. A macOS
//! key event carries whole text, and a lone surrogate is not valid text (some
//! apps, such as Terminal, drop it), so the pair is joined into one character.
//!
//! Ported from rophy/macrdp 2492459 (macrdp-v1 41eca74 and d7a9e68),
//! relicensed to MIT OR Apache-2.0 by the author.

/// What to post for one RDP Unicode keyboard event.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum UnicodeAction {
    /// A key down carrying these UTF-16 units (one character).
    Down(Vec<u16>),
    /// A key up carrying these UTF-16 units (the character of its key down).
    Up(Vec<u16>),
    /// Nothing to post.
    None,
}

/// Joins surrogate pairs of RDP Unicode key events.
#[derive(Debug, Default)]
pub(super) struct UnicodeKeys {
    /// A high surrogate whose low surrogate has not come yet.
    high: Option<u16>,
    /// The pair posted as a key down: the release of its low surrogate posts
    /// the key up.
    pair_down: Option<[u16; 2]>,
}

impl UnicodeKeys {
    pub(super) fn feed(&mut self, unit: u16, pressed: bool) -> UnicodeAction {
        match unit {
            // High surrogate: wait for the low one. Its release posts nothing.
            0xD800..=0xDBFF => {
                if pressed {
                    self.high = Some(unit);
                }
                UnicodeAction::None
            }
            0xDC00..=0xDFFF => match (pressed, self.high.take()) {
                (true, Some(high)) => {
                    self.pair_down = Some([high, unit]);
                    UnicodeAction::Down(vec![high, unit])
                }
                // A low surrogate without a high one is not valid text.
                (true, None) => UnicodeAction::None,
                (false, high) => {
                    // A release does not end a pending high surrogate.
                    self.high = high;
                    match self.pair_down.take() {
                        Some(pair) => UnicodeAction::Up(pair.to_vec()),
                        None => UnicodeAction::None,
                    }
                }
            },
            _ => {
                if pressed {
                    self.high = None;
                    UnicodeAction::Down(vec![unit])
                } else {
                    UnicodeAction::Up(vec![unit])
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SMILE: [u16; 2] = [0xD83D, 0xDE00]; // U+1F600

    #[test]
    fn a_bmp_character_is_posted_as_is() {
        let mut u = UnicodeKeys::default();
        assert_eq!(u.feed(0x00E9, true), UnicodeAction::Down(vec![0x00E9]));
        assert_eq!(u.feed(0x00E9, false), UnicodeAction::Up(vec![0x00E9]));
    }

    #[test]
    fn a_surrogate_pair_becomes_one_character() {
        // SendInput order: high down, high up, low down, low up.
        let mut u = UnicodeKeys::default();
        assert_eq!(u.feed(SMILE[0], true), UnicodeAction::None);
        assert_eq!(u.feed(SMILE[0], false), UnicodeAction::None);
        assert_eq!(u.feed(SMILE[1], true), UnicodeAction::Down(SMILE.to_vec()));
        assert_eq!(u.feed(SMILE[1], false), UnicodeAction::Up(SMILE.to_vec()));
    }

    #[test]
    fn a_surrogate_pair_with_nested_releases() {
        // High down, low down, low up, high up.
        let mut u = UnicodeKeys::default();
        assert_eq!(u.feed(SMILE[0], true), UnicodeAction::None);
        assert_eq!(u.feed(SMILE[1], true), UnicodeAction::Down(SMILE.to_vec()));
        assert_eq!(u.feed(SMILE[1], false), UnicodeAction::Up(SMILE.to_vec()));
        assert_eq!(u.feed(SMILE[0], false), UnicodeAction::None);
    }

    #[test]
    fn lone_surrogates_are_dropped() {
        let mut u = UnicodeKeys::default();
        assert_eq!(u.feed(0xDE00, true), UnicodeAction::None);
        assert_eq!(u.feed(0xDE00, false), UnicodeAction::None);
        // A high surrogate followed by a BMP character: only the character.
        assert_eq!(u.feed(0xD83D, true), UnicodeAction::None);
        assert_eq!(u.feed(0x0041, true), UnicodeAction::Down(vec![0x0041]));
        assert_eq!(u.feed(0xDE00, true), UnicodeAction::None);
    }

    #[test]
    fn two_emoji_in_a_row() {
        let mut u = UnicodeKeys::default();
        for _ in 0..2 {
            assert_eq!(u.feed(SMILE[0], true), UnicodeAction::None);
            assert_eq!(u.feed(SMILE[0], false), UnicodeAction::None);
            assert_eq!(u.feed(SMILE[1], true), UnicodeAction::Down(SMILE.to_vec()));
            assert_eq!(u.feed(SMILE[1], false), UnicodeAction::Up(SMILE.to_vec()));
        }
    }
}
