//! Turns text into the keystrokes that type it, for `SendInput`.

/// One keystroke of typed text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextKey {
    /// A UTF-16 code unit, sent as a `KEYEVENTF_UNICODE` press and release
    /// (a character outside the BMP is two of these, high surrogate first).
    Unit(u16),
    /// Enter: most programs ignore a Unicode line feed.
    Enter,
    Tab,
}

/// The keystrokes that type `text`. A CR LF pair (a Windows or browser
/// line break) is one Enter; control characters other than Tab and line
/// breaks are dropped, since they would do something other than type.
pub fn text_keys(text: &str) -> Vec<TextKey> {
    let mut out = Vec::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push(TextKey::Enter);
            }
            '\n' => out.push(TextKey::Enter),
            '\t' => out.push(TextKey::Tab),
            c if c.is_control() => {}
            c => {
                let mut buf = [0u16; 2];
                out.extend(c.encode_utf16(&mut buf).iter().map(|&u| TextKey::Unit(u)));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_and_non_ascii() {
        assert_eq!(
            text_keys("a\u{e9}"),
            [TextKey::Unit(0x61), TextKey::Unit(0xe9)]
        );
    }

    #[test]
    fn astral_is_a_surrogate_pair() {
        assert_eq!(
            text_keys("\u{1f600}"),
            [TextKey::Unit(0xd83d), TextKey::Unit(0xde00)]
        );
    }

    #[test]
    fn line_breaks_and_controls() {
        assert_eq!(
            text_keys("a\r\nb\nc\rd\te\u{7}"),
            [
                TextKey::Unit(0x61),
                TextKey::Enter,
                TextKey::Unit(0x62),
                TextKey::Enter,
                TextKey::Unit(0x63),
                TextKey::Enter,
                TextKey::Unit(0x64),
                TextKey::Tab,
                TextKey::Unit(0x65),
            ]
        );
    }
}
