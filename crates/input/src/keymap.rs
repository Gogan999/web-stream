//! Maps USB HID keyboard usages (page 0x07) to what Windows injects.
//!
//! Most keys go in as PS/2 set-1 scan codes, because games reading raw input
//! or DirectInput see scan codes, not virtual keys. Keys with no scan code
//! SendInput can produce (Pause, volume, IME keys) go in as virtual keys.

/// What to inject for one physical key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyTarget {
    /// Set-1 scan code; `extended` adds the E0 prefix.
    Scan { code: u16, extended: bool },
    /// Windows virtual-key code.
    Vk(u16),
}

const fn sc(code: u16) -> Option<KeyTarget> {
    Some(KeyTarget::Scan {
        code,
        extended: false,
    })
}

const fn ext(code: u16) -> Option<KeyTarget> {
    Some(KeyTarget::Scan {
        code,
        extended: true,
    })
}

const fn vk(code: u16) -> Option<KeyTarget> {
    Some(KeyTarget::Vk(code))
}

/// Set-1 scan codes for usages 0x04 (A) to 0x1d (Z).
const LETTERS: [u16; 26] = [
    0x1e, 0x30, 0x2e, 0x20, 0x12, 0x21, 0x22, 0x23, 0x17, 0x24, 0x25, 0x26, 0x32, 0x31, 0x18, 0x19,
    0x10, 0x13, 0x1f, 0x14, 0x16, 0x2f, 0x11, 0x2d, 0x15, 0x2c,
];

/// Set-1 scan codes for usages 0x68 (F13) to 0x73 (F24).
const F13_F24: [u16; 12] = [
    0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x6b, 0x6c, 0x6d, 0x6e, 0x76,
];

/// Returns `None` for usages with no Windows equivalent or that are refused.
pub fn lookup(usage: u16) -> Option<KeyTarget> {
    match usage {
        0x04..=0x1d => sc(LETTERS[(usage - 0x04) as usize]),
        // 1..9 then 0.
        0x1e..=0x27 => sc(usage - 0x1e + 0x02),
        0x28 => sc(0x1c),                       // Enter
        0x29 => sc(0x01),                       // Escape
        0x2a => sc(0x0e),                       // Backspace
        0x2b => sc(0x0f),                       // Tab
        0x2c => sc(0x39),                       // Space
        0x2d => sc(0x0c),                       // Minus
        0x2e => sc(0x0d),                       // Equal
        0x2f => sc(0x1a),                       // BracketLeft
        0x30 => sc(0x1b),                       // BracketRight
        0x31 => sc(0x2b),                       // Backslash
        0x32 => sc(0x2b),                       // IntlHash shares the Backslash position
        0x33 => sc(0x27),                       // Semicolon
        0x34 => sc(0x28),                       // Quote
        0x35 => sc(0x29),                       // Backquote
        0x36 => sc(0x33),                       // Comma
        0x37 => sc(0x34),                       // Period
        0x38 => sc(0x35),                       // Slash
        0x39 => sc(0x3a),                       // CapsLock
        0x3a..=0x43 => sc(usage - 0x3a + 0x3b), // F1..F10
        0x44 => sc(0x57),                       // F11
        0x45 => sc(0x58),                       // F12
        0x46 => ext(0x37),                      // PrintScreen
        0x47 => sc(0x46),                       // ScrollLock
        // Pause's scan sequence (E1 1D 45) cannot be sent through SendInput.
        0x48 => vk(0x13),
        0x49 => ext(0x52), // Insert
        0x4a => ext(0x47), // Home
        0x4b => ext(0x49), // PageUp
        0x4c => ext(0x53), // Delete
        0x4d => ext(0x4f), // End
        0x4e => ext(0x51), // PageDown
        0x4f => ext(0x4d), // ArrowRight
        0x50 => ext(0x4b), // ArrowLeft
        0x51 => ext(0x50), // ArrowDown
        0x52 => ext(0x48), // ArrowUp
        0x53 => sc(0x45),  // NumLock
        0x54 => ext(0x35), // NumpadDivide
        0x55 => sc(0x37),  // NumpadMultiply
        0x56 => sc(0x4a),  // NumpadSubtract
        0x57 => sc(0x4e),  // NumpadAdd
        0x58 => ext(0x1c), // NumpadEnter
        0x59 => sc(0x4f),  // Numpad1
        0x5a => sc(0x50),
        0x5b => sc(0x51),
        0x5c => sc(0x4b),
        0x5d => sc(0x4c),
        0x5e => sc(0x4d),
        0x5f => sc(0x47),
        0x60 => sc(0x48),
        0x61 => sc(0x49),  // Numpad9
        0x62 => sc(0x52),  // Numpad0
        0x63 => sc(0x53),  // NumpadDecimal
        0x64 => sc(0x56),  // IntlBackslash
        0x65 => ext(0x5d), // ContextMenu
        // 0x66 Power is deliberately unmapped: a remote client must not be
        // able to shut the host down with one key.
        0x67 => sc(0x59), // NumpadEqual
        0x68..=0x73 => sc(F13_F24[(usage - 0x68) as usize]),
        0x7f => vk(0xad),  // AudioVolumeMute
        0x80 => vk(0xaf),  // AudioVolumeUp
        0x81 => vk(0xae),  // AudioVolumeDown
        0x85 => sc(0x7e),  // NumpadComma
        0x87 => sc(0x73),  // IntlRo
        0x88 => sc(0x70),  // KanaMode
        0x89 => sc(0x7d),  // IntlYen
        0x8a => sc(0x79),  // Convert
        0x8b => sc(0x7b),  // NonConvert
        0x90 => vk(0x15),  // Lang1 (Hangul/Kana toggle)
        0x91 => vk(0x19),  // Lang2 (Hanja)
        0xe0 => sc(0x1d),  // ControlLeft
        0xe1 => sc(0x2a),  // ShiftLeft
        0xe2 => sc(0x38),  // AltLeft
        0xe3 => ext(0x5b), // MetaLeft
        0xe4 => ext(0x1d), // ControlRight
        0xe5 => sc(0x36),  // ShiftRight
        0xe6 => ext(0x38), // AltRight
        0xe7 => ext(0x5c), // MetaRight
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn s(code: u16) -> Option<KeyTarget> {
        sc(code)
    }

    #[test]
    fn common_gaming_keys() {
        assert_eq!(lookup(0x1a), s(0x11), "W");
        assert_eq!(lookup(0x04), s(0x1e), "A");
        assert_eq!(lookup(0x16), s(0x1f), "S");
        assert_eq!(lookup(0x07), s(0x20), "D");
        assert_eq!(lookup(0x1d), s(0x2c), "Z");
        assert_eq!(lookup(0x1e), s(0x02), "1");
        assert_eq!(lookup(0x27), s(0x0b), "0");
        assert_eq!(lookup(0x2c), s(0x39), "Space");
        assert_eq!(lookup(0x3a), s(0x3b), "F1");
        assert_eq!(lookup(0x43), s(0x44), "F10");
        assert_eq!(lookup(0x45), s(0x58), "F12");
        assert_eq!(lookup(0x73), s(0x76), "F24");
        assert_eq!(lookup(0x52), ext(0x48), "ArrowUp");
        assert_eq!(lookup(0xe4), ext(0x1d), "ControlRight");
        assert_eq!(lookup(0x48), vk(0x13), "Pause");
    }

    #[test]
    fn power_and_unknown_usages_are_refused() {
        assert_eq!(lookup(0x66), None);
        assert_eq!(lookup(0x00), None);
        assert_eq!(lookup(0xffff), None);
    }

    #[test]
    fn every_usage_the_browser_sends_is_mapped() {
        // The usages in web/js/keymap.js, minus Power.
        let mut usages: Vec<u16> = (0x04..=0x65).collect();
        usages.extend([
            0x67, 0x7f, 0x80, 0x81, 0x85, 0x87, 0x88, 0x89, 0x8a, 0x8b, 0x90, 0x91,
        ]);
        usages.extend(0x68..=0x73);
        usages.extend(0xe0..=0xe7);
        for u in usages {
            assert!(lookup(u).is_some(), "usage 0x{u:02x}");
        }
    }

    #[test]
    fn no_two_usages_share_a_scan_code_except_intl_hash() {
        let mut seen = std::collections::HashMap::new();
        for u in 0..=0xffu16 {
            if let Some(t) = lookup(u) {
                if let Some(prev) = seen.insert(t, u) {
                    assert_eq!(
                        (prev, u),
                        (0x31, 0x32),
                        "0x{prev:02x} and 0x{u:02x} collide"
                    );
                }
            }
        }
    }
}
