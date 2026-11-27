//! Linux `KEY_*` codes for the keys of [`crate::keymap`] and for typed text.

use crate::keymap::KeyTarget;

pub const KEY_ENTER: u16 = 28;
pub const KEY_LEFTCTRL: u16 = 29;
pub const KEY_LEFTSHIFT: u16 = 42;
pub const KEY_LEFTALT: u16 = 56;
pub const KEY_SPACE: u16 = 57;
pub const KEY_TAB: u16 = 15;
pub const KEY_DELETE: u16 = 111;
pub const KEY_U: u16 = 22;

/// Highest key code the virtual keyboard reports.
pub const KEY_LAST: u16 = 248;

/// `KEY_*` for `a` to `z`.
const LETTERS: [u16; 26] = [
    30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45,
    21, 44,
];

/// The Linux key for a [`KeyTarget`] from [`crate::keymap::lookup`].
pub const fn linux_key(target: KeyTarget) -> Option<u16> {
    let code = match target {
        KeyTarget::Scan {
            code,
            extended: false,
        } => match code {
            0x01..=0x58 => code,
            0x59 => 117,
            0x64..=0x6e => code - 0x64 + 183,
            0x76 => 194,
            0x7e => 121,
            0x73 => 89,
            0x70 => 93,
            0x7d => 124,
            0x79 => 92,
            0x7b => 94,
            _ => return None,
        },
        KeyTarget::Scan {
            code,
            extended: true,
        } => match code {
            0x37 => 99,
            0x52 => 110,
            0x47 => 102,
            0x49 => 104,
            0x53 => 111,
            0x4f => 107,
            0x51 => 109,
            0x4d => 106,
            0x4b => 105,
            0x50 => 108,
            0x48 => 103,
            0x35 => 98,
            0x1c => 96,
            0x5d => 127,
            0x5b => 125,
            0x1d => 97,
            0x38 => 100,
            0x5c => 126,
            _ => return None,
        },
        KeyTarget::Vk(vk) => match vk {
            0x13 => 119,
            0xad => 113,
            0xaf => 115,
            0xae => 114,
            0x15 => 122,
            0x19 => 123,
            _ => return None,
        },
    };
    Some(code)
}

/// The key and Shift state that type `c` on a US layout.
pub fn us_key(c: char) -> Option<(u16, bool)> {
    let plain = |k| Some((k, false));
    let shift = |k| Some((k, true));
    match c {
        'a'..='z' => plain(LETTERS[c as usize - 'a' as usize]),
        'A'..='Z' => shift(LETTERS[c as usize - 'A' as usize]),
        '1'..='9' => plain(c as u16 - '1' as u16 + 2),
        '0' => plain(11),
        ' ' => plain(KEY_SPACE),
        '-' => plain(12),
        '=' => plain(13),
        '[' => plain(26),
        ']' => plain(27),
        ';' => plain(39),
        '\'' => plain(40),
        '`' => plain(41),
        '\\' => plain(43),
        ',' => plain(51),
        '.' => plain(52),
        '/' => plain(53),
        '!' => shift(2),
        '@' => shift(3),
        '#' => shift(4),
        '$' => shift(5),
        '%' => shift(6),
        '^' => shift(7),
        '&' => shift(8),
        '*' => shift(9),
        '(' => shift(10),
        ')' => shift(11),
        '_' => shift(12),
        '+' => shift(13),
        '{' => shift(26),
        '}' => shift(27),
        ':' => shift(39),
        '"' => shift(40),
        '~' => shift(41),
        '|' => shift(43),
        '<' => shift(51),
        '>' => shift(52),
        '?' => shift(53),
        _ => None,
    }
}

/// One keystroke of typed text: a key, with or without Shift, or with Ctrl+Shift held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stroke {
    Key(u16, bool),
    CtrlShift(u16),
}

/// The keystrokes that type `c`: the US-layout key, or Ctrl+Shift+U, the hex code point and Space
/// (the GTK/IBus Unicode entry) for anything else.
pub fn strokes(c: char, out: &mut Vec<Stroke>) {
    match c {
        '\n' => out.push(Stroke::Key(KEY_ENTER, false)),
        '\t' => out.push(Stroke::Key(KEY_TAB, false)),
        c => match us_key(c) {
            Some((k, s)) => out.push(Stroke::Key(k, s)),
            None => {
                out.push(Stroke::CtrlShift(KEY_U));
                for d in format!("{:x}", c as u32).chars() {
                    let (k, _) = us_key(d).expect("hex digit");
                    out.push(Stroke::Key(k, false));
                }
                out.push(Stroke::Key(KEY_SPACE, false));
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::lookup;

    /// `hid_keyboard[]` from the kernel's drivers/hid/hid-input.c, 0 where it has no key.
    #[rustfmt::skip]
    const HID_KEYBOARD: [u16; 0x95] = [
          0,  0,  0,  0, 30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38,
         50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44,  2,  3,
          4,  5,  6,  7,  8,  9, 10, 11, 28,  1, 14, 15, 57, 12, 13, 26,
         27, 43, 43, 39, 40, 41, 51, 52, 53, 58, 59, 60, 61, 62, 63, 64,
         65, 66, 67, 68, 87, 88, 99, 70,119,110,102,104,111,107,109,106,
        105,108,103, 69, 98, 55, 74, 78, 96, 79, 80, 81, 75, 76, 77, 71,
         72, 73, 82, 83, 86,127,116,117,183,184,185,186,187,188,189,190,
        191,192,193,194,134,138,130,132,128,129,131,137,133,135,136,113,
        115,114,  0,  0,  0,121,  0, 89, 93,124, 92, 94, 95,  0,  0,  0,
        122,123, 90, 91, 85,
    ];
    const MODIFIERS: [u16; 8] = [29, 42, 56, 125, 97, 54, 100, 126];

    fn kernel(usage: u16) -> Option<u16> {
        let code = match usage {
            0xe0..=0xe7 => MODIFIERS[usage as usize - 0xe0],
            u if (u as usize) < HID_KEYBOARD.len() => HID_KEYBOARD[u as usize],
            _ => 0,
        };
        (code != 0).then_some(code)
    }

    #[test]
    fn every_mapped_usage_gets_the_key_the_kernel_gives_a_usb_keyboard() {
        let mut mapped = 0;
        for u in 0..=0xffu16 {
            if let Some(target) = lookup(u) {
                assert_eq!(linux_key(target), kernel(u), "usage 0x{u:02x}");
                mapped += 1;
            }
        }
        assert!(mapped > 120);
    }

    #[test]
    fn every_mapped_key_is_on_the_virtual_keyboard() {
        for u in 0..=0xffu16 {
            if let Some(k) = lookup(u).and_then(linux_key) {
                assert!(k <= KEY_LAST, "usage 0x{u:02x}");
            }
        }
    }

    #[test]
    fn printable_ascii_types_on_a_us_layout() {
        for c in ' '..='~' {
            assert!(us_key(c).is_some(), "{c:?}");
        }
        assert_eq!(us_key('a'), Some((30, false)));
        assert_eq!(us_key('Q'), Some((16, true)));
        assert_eq!(us_key('0'), Some((11, false)));
        assert_eq!(us_key('?'), Some((53, true)));
        assert_eq!(us_key('\u{e9}'), None);
    }

    #[test]
    fn other_characters_use_unicode_entry() {
        let mut out = Vec::new();
        strokes('\u{e9}', &mut out);
        assert_eq!(
            out,
            [
                Stroke::CtrlShift(KEY_U),
                Stroke::Key(us_key('e').unwrap().0, false),
                Stroke::Key(us_key('9').unwrap().0, false),
                Stroke::Key(KEY_SPACE, false),
            ]
        );
        out.clear();
        strokes('\n', &mut out);
        assert_eq!(out, [Stroke::Key(KEY_ENTER, false)]);
    }
}
