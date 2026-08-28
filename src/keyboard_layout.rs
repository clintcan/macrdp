//! Server-side keyboard-layout translation for non-US layouts.
//!
//! RDP clients send PS/2 scancodes — *physical key positions*, not characters.
//! `input.rs` maps those to macOS virtual keycodes (also positional), posts
//! them, and macOS turns a keycode into a character using **the Mac's active
//! input source**. That only yields the right character when the Mac's layout
//! happens to match the remote user's, so an AZERTY/QWERTZ/etc. user on a
//! US-configured Mac gets the wrong letters.
//!
//! To serve any layout *without disturbing the local Mac*, we translate
//! `(keycode + modifiers)` → Unicode ourselves via Carbon's `UCKeyTranslate`
//! against the client's chosen layout and post the resulting character as a
//! Unicode string event (the same mechanism the RDP Unicode-keyboard path
//! already uses). Only ordinary typing keys with no Cmd/Ctrl held go through
//! here — Cmd/Ctrl combinations stay on the keycode path so app shortcuts
//! (Cmd+C, Cmd+Q, …) keep working, and dead keys (´ + e → é) compose via
//! `UCKeyTranslate`'s persistent dead-key state.

/// macOS virtual keycodes that produce ordinary text and should be routed
/// through layout translation. This is the contiguous ANSI block
/// (`kVK_ANSI_A`=0x00 … `kVK_ANSI_Grave`=0x32, including `kVK_ISO_Section`=0x0A,
/// the extra key on ISO/European keyboards) minus the three non-character keys
/// that live inside that range: Return (0x24), Tab (0x30) and Space (0x31),
/// which must stay on the keycode path so they behave as keys rather than
/// literal characters. Modifiers, arrows, function keys, the keypad, and
/// editing keys all sit at 0x33+ and are excluded.
///
/// Kept platform-independent (and unit-tested) so `input.rs` can call it from
/// the macOS path while tests run anywhere.
pub fn is_translatable_keycode(vk: u16) -> bool {
    vk <= 0x32 && vk != 0x24 && vk != 0x30 && vk != 0x31
}

#[cfg(target_os = "macos")]
pub use macos::KeyboardLayout;

#[cfg(target_os = "macos")]
mod macos {
    use std::os::raw::c_void;

    use core_foundation::base::{CFRelease, TCFType};
    use core_foundation::data::{CFData, CFDataRef};
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::string::{CFString, CFStringRef};
    use tracing::warn;

    type TISInputSourceRef = *mut c_void;

    #[link(name = "Carbon", kind = "framework")]
    extern "C" {
        static kTISPropertyInputSourceID: CFStringRef;
        static kTISPropertyUnicodeKeyLayoutData: CFStringRef;
        fn TISCreateInputSourceList(properties: *const c_void, include_all: u8) -> *const c_void;
        fn TISCopyInputSourceForLanguage(language: CFStringRef) -> TISInputSourceRef;
        fn TISCopyCurrentKeyboardLayoutInputSource() -> TISInputSourceRef;
        fn TISGetInputSourceProperty(source: TISInputSourceRef, key: CFStringRef) -> *const c_void;
        fn LMGetKbdType() -> u8;
        fn UCKeyTranslate(
            key_layout_ptr: *const c_void,
            virtual_key_code: u16,
            key_action: u16,
            modifier_key_state: u32,
            keyboard_type: u32,
            key_translate_options: u32,
            dead_key_state: *mut u32,
            max_string_length: usize,
            actual_string_length: *mut usize,
            unicode_string: *mut u16,
        ) -> i32;
    }

    // CoreFoundation symbols (linked transitively via the core-foundation crate).
    extern "C" {
        fn CFArrayGetCount(arr: *const c_void) -> isize;
        fn CFArrayGetValueAtIndex(arr: *const c_void, idx: isize) -> *const c_void;
        fn CFDataGetBytePtr(data: *const c_void) -> *const u8;
    }

    const KEY_ACTION_DOWN: u16 = 0;
    // Carbon modifier bits, in the `(eventModifiers >> 8) & 0xFF` form that
    // UCKeyTranslate's `modifierKeyState` expects: shiftKey>>8 = 0x02,
    // alphaLock>>8 = 0x04, optionKey>>8 = 0x08. We deliberately never set the
    // cmd/control bits here — those combinations take the keycode path instead.
    const MOD_SHIFT: u32 = 0x02;
    const MOD_CAPS: u32 = 0x04;
    const MOD_OPTION: u32 = 0x08;

    /// A resolved keyboard layout we can translate keystrokes against.
    /// `uchr` owns the `UCKeyboardLayout` byte buffer (a retained `CFData`), so
    /// it outlives the input source it came from; `dead_key_state` carries
    /// pending dead-key composition across keystrokes.
    pub struct KeyboardLayout {
        uchr: CFData,
        kbd_type: u32,
        dead_key_state: u32,
        label: String,
    }

    impl KeyboardLayout {
        /// Resolve a user-supplied spec — a macOS input-source id
        /// (`com.apple.keylayout.French`), a short name (`french`, `de`,
        /// `azerty`), or a Windows KLID (`0x040C`, `040c`) — into a layout.
        /// Tries the precise input-source id first, then falls back to the
        /// system's default source for the matching language. Returns `None`
        /// (and warns) if nothing matched, so callers fall back to the Mac's
        /// active input source.
        pub fn resolve(spec: &str) -> Option<KeyboardLayout> {
            let kbd_type = u32::from(unsafe { LMGetKbdType() });
            let spec = spec.trim();

            let id: Option<String> = if spec.contains('.') {
                Some(spec.to_string())
            } else {
                spec_to_source_id(spec).map(str::to_string)
            };
            if let Some(id) = &id {
                if let Some(uchr) = source_data_for_id(id) {
                    return Some(KeyboardLayout {
                        uchr,
                        kbd_type,
                        dead_key_state: 0,
                        label: id.clone(),
                    });
                }
            }
            if let Some(lang) = spec_to_language(spec) {
                if let Some(uchr) = source_data_for_language(lang) {
                    return Some(KeyboardLayout {
                        uchr,
                        kbd_type,
                        dead_key_state: 0,
                        label: format!("lang:{lang}"),
                    });
                }
            }
            warn!(
                spec = %spec,
                "could not resolve keyboard layout — falling back to the Mac's active input source"
            );
            None
        }

        pub fn label(&self) -> &str {
            &self.label
        }

        /// The Mac's CURRENTLY ACTIVE keyboard layout — as opposed to
        /// [`resolve`], which looks up a caller-named layout. Used by the
        /// reconnect auto-unlock path, which must synthesize keystrokes that
        /// produce the right characters *on this Mac*, so the active layout is
        /// the only correct reference.
        pub fn current() -> Option<KeyboardLayout> {
            let kbd_type = u32::from(unsafe { LMGetKbdType() });
            unsafe {
                let source = TISCopyCurrentKeyboardLayoutInputSource();
                if source.is_null() {
                    return None;
                }
                // `uchr_from_source` retains its own copy of the layout data,
                // so the +1 reference from this TISCopy* is ours to release.
                let uchr = uchr_from_source(source);
                CFRelease(source as *const c_void);
                Some(KeyboardLayout {
                    uchr: uchr?,
                    kbd_type,
                    dead_key_state: 0,
                    label: "current".to_string(),
                })
            }
        }

        /// Build a `character -> (keycode, shift, option)` lookup for this
        /// layout by probing every character-producing keycode under each
        /// modifier combination — the REVERSE of [`translate`].
        ///
        /// Exists because synthesizing text as one bulk
        /// `CGEventKeyboardSetUnicodeString` event is NOT equivalent to typing
        /// for macOS's secure password fields: the text appears, but the
        /// field's own text-change bookkeeping never fires, so it ignores a
        /// subsequent Return (live-confirmed 2026-08-28 — even a REAL Return
        /// on the physical keyboard was ignored until a character was manually
        /// added and deleted). Real per-character keycode events are what a
        /// physical keyboard sends, and are what the field actually accepts.
        ///
        /// Deliberately probes with Shift rather than Caps Lock, and resets
        /// `dead_key_state` around every probe so a dead key (which returns
        /// `Some("")` and would otherwise compose into the next probe) can't
        /// pollute the map. Multi-character results are skipped; the first
        /// modifier combination to produce a character wins (unmodified before
        /// shifted before option), so the simplest keystroke is preferred.
        pub fn reverse_map(&mut self) -> std::collections::HashMap<char, (u16, bool, bool)> {
            let mut map: std::collections::HashMap<char, (u16, bool, bool)> =
                std::collections::HashMap::new();
            for (shift, option) in [(false, false), (true, false), (false, true), (true, true)] {
                // 0x00..=0x32 is the contiguous character-producing block (see
                // `is_translatable_keycode`), minus Return/Tab which are keys
                // rather than characters. Space (0x31) IS included here — it is
                // excluded from `is_translatable_keycode` because the RDP input
                // path must treat it as a key, but a password can contain one.
                for vk in 0x00u16..=0x32 {
                    if vk == 0x24 || vk == 0x30 {
                        continue;
                    }
                    self.dead_key_state = 0;
                    let translated = self.translate(vk, shift, option, false);
                    self.dead_key_state = 0;
                    let Some(text) = translated else { continue };
                    let mut chars = text.chars();
                    let (Some(ch), None) = (chars.next(), chars.next()) else {
                        continue;
                    };
                    map.entry(ch).or_insert((vk, shift, option));
                }
            }
            self.dead_key_state = 0;
            map
        }

        /// Translate a virtual keycode under the current modifier state into the
        /// character(s) it produces in this layout. Returns `Some("")` when the
        /// key is a pending dead key (caller should swallow it; the composed
        /// character arrives on the next keystroke), `Some(text)` for a normal
        /// character, or `None` on a translation error.
        pub fn translate(
            &mut self,
            keycode: u16,
            shift: bool,
            option: bool,
            caps: bool,
        ) -> Option<String> {
            let ptr = unsafe { CFDataGetBytePtr(self.uchr.as_concrete_TypeRef() as *const c_void) };
            if ptr.is_null() {
                return None;
            }
            let mut mods = 0u32;
            if shift {
                mods |= MOD_SHIFT;
            }
            if caps {
                mods |= MOD_CAPS;
            }
            if option {
                mods |= MOD_OPTION;
            }
            let mut buf = [0u16; 8];
            let mut actual = 0usize;
            let status = unsafe {
                UCKeyTranslate(
                    ptr as *const c_void,
                    keycode,
                    KEY_ACTION_DOWN,
                    mods,
                    self.kbd_type,
                    0, // translate options: 0 keeps dead-key composition on
                    &mut self.dead_key_state,
                    buf.len(),
                    &mut actual,
                    buf.as_mut_ptr(),
                )
            };
            if status != 0 {
                return None;
            }
            Some(String::from_utf16_lossy(&buf[..actual]))
        }
    }

    /// Extract a retained `UCKeyboardLayout` `CFData` from an input source.
    /// `TISGetInputSourceProperty` returns a borrowed reference, so we retain
    /// it (`wrap_under_get_rule`) to own a copy that outlives `source`.
    unsafe fn uchr_from_source(source: TISInputSourceRef) -> Option<CFData> {
        if source.is_null() {
            return None;
        }
        let data_ref = TISGetInputSourceProperty(source, kTISPropertyUnicodeKeyLayoutData);
        if data_ref.is_null() {
            return None;
        }
        Some(CFData::wrap_under_get_rule(data_ref as CFDataRef))
    }

    fn source_data_for_id(id: &str) -> Option<CFData> {
        unsafe {
            let key = CFString::wrap_under_get_rule(kTISPropertyInputSourceID);
            let value = CFString::new(id);
            let dict = CFDictionary::from_CFType_pairs(&[(key.as_CFType(), value.as_CFType())]);
            // include_all = 1: match even input sources the user hasn't enabled.
            let list = TISCreateInputSourceList(dict.as_concrete_TypeRef() as *const c_void, 1);
            if list.is_null() {
                return None;
            }
            let out = if CFArrayGetCount(list) > 0 {
                uchr_from_source(CFArrayGetValueAtIndex(list, 0) as TISInputSourceRef)
            } else {
                None
            };
            CFRelease(list);
            out
        }
    }

    fn source_data_for_language(lang: &str) -> Option<CFData> {
        unsafe {
            let cflang = CFString::new(lang);
            let source = TISCopyInputSourceForLanguage(cflang.as_concrete_TypeRef());
            if source.is_null() {
                return None;
            }
            let out = uchr_from_source(source);
            CFRelease(source as *const c_void);
            out
        }
    }

    /// Parse a Windows KLID written as hex (`0x040C`, `040c`). Names like `us`
    /// are rejected here (handled by `name_to_source_id` first) even though
    /// some — e.g. `de` — are coincidentally all hex digits.
    fn parse_klid(spec: &str) -> Option<u32> {
        let s = spec
            .strip_prefix("0x")
            .or_else(|| spec.strip_prefix("0X"))
            .unwrap_or(spec);
        if !s.is_empty() && s.len() <= 8 && s.chars().all(|c| c.is_ascii_hexdigit()) {
            u32::from_str_radix(s, 16).ok()
        } else {
            None
        }
    }

    /// Map a spec to a precise macOS input-source id. Names are matched before
    /// KLIDs so a name that looks like hex (`de`, `be`) resolves as a name.
    fn spec_to_source_id(spec: &str) -> Option<&'static str> {
        let low = spec.to_ascii_lowercase();
        if let Some(id) = name_to_source_id(&low) {
            return Some(id);
        }
        parse_klid(&low).and_then(klid_to_source_id)
    }

    fn name_to_source_id(low: &str) -> Option<&'static str> {
        Some(match low {
            "us" | "en" | "english" | "ansi" => "com.apple.keylayout.US",
            "uk" | "gb" | "british" => "com.apple.keylayout.British",
            "fr" | "french" | "azerty" => "com.apple.keylayout.French",
            "de" | "german" | "qwertz" => "com.apple.keylayout.German",
            "es" | "spanish" => "com.apple.keylayout.Spanish-ISO",
            "it" | "italian" => "com.apple.keylayout.Italian",
            "pt" | "portuguese" => "com.apple.keylayout.Portuguese",
            "br" | "brazilian" => "com.apple.keylayout.Brazilian",
            "nl" | "dutch" => "com.apple.keylayout.Dutch",
            "be" | "belgian" => "com.apple.keylayout.Belgian",
            "se" | "swedish" => "com.apple.keylayout.Swedish",
            "no" | "norwegian" => "com.apple.keylayout.Norwegian",
            "dk" | "danish" => "com.apple.keylayout.Danish",
            "fi" | "finnish" => "com.apple.keylayout.Finnish",
            "ru" | "russian" => "com.apple.keylayout.Russian",
            "pl" | "polish" => "com.apple.keylayout.Polish",
            "cz" | "czech" => "com.apple.keylayout.Czech",
            "hu" | "hungarian" => "com.apple.keylayout.Hungarian",
            "ch" | "swiss" | "swissgerman" => "com.apple.keylayout.SwissGerman",
            _ => return None,
        })
    }

    fn klid_to_source_id(klid: u32) -> Option<&'static str> {
        // Match on the low 16 bits (the LANGID); the high word is the layout
        // variant, which we don't distinguish here.
        Some(match klid & 0xFFFF {
            0x0409 => "com.apple.keylayout.US",
            0x0809 => "com.apple.keylayout.British",
            0x040C => "com.apple.keylayout.French",
            0x080C => "com.apple.keylayout.Belgian",
            0x0C0C => "com.apple.keylayout.Canadian-CSA",
            0x0407 => "com.apple.keylayout.German",
            0x0807 => "com.apple.keylayout.SwissGerman",
            0x040A => "com.apple.keylayout.Spanish-ISO",
            0x0410 => "com.apple.keylayout.Italian",
            0x0413 => "com.apple.keylayout.Dutch",
            0x0813 => "com.apple.keylayout.Belgian",
            0x0416 => "com.apple.keylayout.Brazilian",
            0x0816 => "com.apple.keylayout.Portuguese",
            0x041D => "com.apple.keylayout.Swedish",
            0x0414 => "com.apple.keylayout.Norwegian",
            0x0406 => "com.apple.keylayout.Danish",
            0x040B => "com.apple.keylayout.Finnish",
            0x0419 => "com.apple.keylayout.Russian",
            0x0415 => "com.apple.keylayout.Polish",
            0x0405 => "com.apple.keylayout.Czech",
            0x040E => "com.apple.keylayout.Hungarian",
            _ => return None,
        })
    }

    /// Best-effort language code for the language fallback (used when the
    /// precise input-source id isn't installed).
    fn spec_to_language(spec: &str) -> Option<&'static str> {
        let low = spec.to_ascii_lowercase();
        let by_name = match low.as_str() {
            "us" | "en" | "english" | "uk" | "gb" | "british" => "en",
            "fr" | "french" | "azerty" => "fr",
            "de" | "german" | "qwertz" | "ch" | "swiss" | "swissgerman" => "de",
            "es" | "spanish" => "es",
            "it" | "italian" => "it",
            "pt" | "portuguese" | "br" | "brazilian" => "pt",
            "nl" | "dutch" | "be" | "belgian" => "nl",
            "se" | "swedish" => "sv",
            "no" | "norwegian" => "nb",
            "dk" | "danish" => "da",
            "fi" | "finnish" => "fi",
            "ru" | "russian" => "ru",
            "pl" | "polish" => "pl",
            "cz" | "czech" => "cs",
            "hu" | "hungarian" => "hu",
            _ => "",
        };
        if !by_name.is_empty() {
            return Some(by_name);
        }
        let klid = parse_klid(&low)?;
        // Primary language id = low 10 bits of the LANGID.
        Some(match klid & 0x3FF {
            0x09 => "en",
            0x0C => "fr",
            0x07 => "de",
            0x0A => "es",
            0x10 => "it",
            0x16 => "pt",
            0x13 => "nl",
            0x1D => "sv",
            0x14 => "nb",
            0x06 => "da",
            0x0B => "fi",
            0x19 => "ru",
            0x15 => "pl",
            0x05 => "cs",
            0x0E => "hu",
            _ => return None,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn spec_resolution_prefers_names_then_klids() {
            assert_eq!(
                spec_to_source_id("french"),
                Some("com.apple.keylayout.French")
            );
            assert_eq!(spec_to_source_id("FR"), Some("com.apple.keylayout.French"));
            // `de` is all-hex-digits but must resolve as the German *name*,
            // not KLID 0x00DE.
            assert_eq!(spec_to_source_id("de"), Some("com.apple.keylayout.German"));
            assert_eq!(
                spec_to_source_id("0x040C"),
                Some("com.apple.keylayout.French")
            );
            assert_eq!(
                spec_to_source_id("040c"),
                Some("com.apple.keylayout.French")
            );
            assert_eq!(
                spec_to_source_id("0x0407"),
                Some("com.apple.keylayout.German")
            );
            assert_eq!(spec_to_source_id("nonsense"), None);
        }

        #[test]
        fn us_layout_translates_basic_keys() {
            // The US layout is always installed, so this exercises the full
            // TIS + UCKeyTranslate pipeline on the build machine.
            let mut us = KeyboardLayout::resolve("us").expect("US layout resolves");
            // kVK_ANSI_A = 0x00
            assert_eq!(
                us.translate(0x00, false, false, false).as_deref(),
                Some("a")
            );
            assert_eq!(us.translate(0x00, true, false, false).as_deref(), Some("A"));
            assert_eq!(us.translate(0x00, false, false, true).as_deref(), Some("A"));
            // kVK_ANSI_1 = 0x12 → '1', Shift+1 → '!'
            assert_eq!(
                us.translate(0x12, false, false, false).as_deref(),
                Some("1")
            );
            assert_eq!(us.translate(0x12, true, false, false).as_deref(), Some("!"));
        }

        #[test]
        fn reverse_map_round_trips_password_characters() {
            // The reverse map is what the auto-unlock path types with, so it
            // must resolve the character classes a real password contains —
            // and every entry must translate back to the character it's
            // filed under, or auto-unlock would type the wrong password.
            let mut us = KeyboardLayout::resolve("us").expect("US layout resolves");
            let map = us.reverse_map();

            for ch in "abz".chars().chain("ABZ".chars()).chain("109".chars()) {
                assert!(map.contains_key(&ch), "no keystroke mapped for {ch:?}");
            }
            // Symbols that commonly appear in passwords, including a shifted
            // one and space (deliberately included despite being excluded
            // from `is_translatable_keycode`).
            for ch in ['!', '@', '-', '.', ' '] {
                assert!(map.contains_key(&ch), "no keystroke mapped for {ch:?}");
            }
            // Known-good specifics on the US layout.
            assert_eq!(map.get(&'a'), Some(&(0x00, false, false)));
            assert_eq!(map.get(&'A'), Some(&(0x00, true, false)));
            assert_eq!(map.get(&' '), Some(&(0x31, false, false)));

            // Every mapping must actually produce its character.
            let mut layout = KeyboardLayout::resolve("us").expect("US layout resolves");
            for (ch, &(vk, shift, option)) in &map {
                let got = layout.translate(vk, shift, option, false);
                assert_eq!(
                    got.as_deref(),
                    Some(ch.to_string().as_str()),
                    "keystroke for {ch:?} translates back to {got:?}"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_translatable_keycode;

    #[test]
    fn translatable_range_excludes_return_tab_space() {
        assert!(is_translatable_keycode(0x00)); // A
        assert!(is_translatable_keycode(0x0A)); // ISO section key
        assert!(is_translatable_keycode(0x32)); // Grave
        assert!(!is_translatable_keycode(0x24)); // Return
        assert!(!is_translatable_keycode(0x30)); // Tab
        assert!(!is_translatable_keycode(0x31)); // Space
        assert!(!is_translatable_keycode(0x33)); // Delete
        assert!(!is_translatable_keycode(0x37)); // Command
        assert!(!is_translatable_keycode(0x7E)); // Up arrow
    }
}
