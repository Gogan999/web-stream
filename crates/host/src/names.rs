//! File names that cross the wire: what to keep of them.

/// Neither a control character nor an invisible formatting one (Unicode
/// Cf: bidi overrides like U+202E, zero-width marks), which can make
/// `exe.txt` show as something else.
pub fn printable(c: char) -> bool {
    !c.is_control() && !is_format(c)
}

/// Unicode category Cf.
fn is_format(c: char) -> bool {
    matches!(c,
        '\u{00AD}' | '\u{0600}'..='\u{0605}' | '\u{061C}' | '\u{06DD}' | '\u{070F}'
        | '\u{0890}'..='\u{0891}' | '\u{08E2}' | '\u{180E}' | '\u{200B}'..='\u{200F}'
        | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206F}'
        | '\u{FEFF}' | '\u{FFF9}'..='\u{FFFB}' | '\u{110BD}' | '\u{110CD}'
        | '\u{13430}'..='\u{1343F}' | '\u{1BCA0}'..='\u{1BCA3}' | '\u{1D173}'..='\u{1D17A}'
        | '\u{E0001}' | '\u{E0020}'..='\u{E007F}')
}

/// The longest start of `s` that is at most `max` UTF-16 units, the unit
/// Windows and JavaScript count names in.
pub fn truncate_utf16(s: &str, max: usize) -> &str {
    let mut units = 0;
    for (i, c) in s.char_indices() {
        units += c.len_utf16();
        if units > max {
            return &s[..i];
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_invisible_characters() {
        let name: String = "report\u{202E}fdp.exe\u{200B}"
            .chars()
            .filter(|&c| printable(c))
            .collect();
        assert_eq!(name, "reportfdp.exe");
        assert!(printable('é') && printable('日'));
    }

    #[test]
    fn truncates_in_utf16_units() {
        assert_eq!(truncate_utf16("abc", 2), "ab");
        assert_eq!(truncate_utf16("a😀b", 2), "a");
        assert_eq!(truncate_utf16("a😀b", 3), "a😀");
        assert_eq!(truncate_utf16("ab", 9), "ab");
    }
}
