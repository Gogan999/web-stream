//! Monitor names from EDID.

const HEADER: [u8; 8] = [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00];

/// The monitor's name: the EDID display name descriptor, else its manufacturer and product code.
pub(crate) fn monitor_name(edid: &[u8]) -> Option<String> {
    if edid.len() < 128 || edid[..8] != HEADER {
        return None;
    }
    let descriptor_name = (0..4).find_map(|i| {
        let d = &edid[54 + i * 18..72 + i * 18];
        if d[0] != 0 || d[1] != 0 || d[3] != 0xfc {
            return None;
        }
        let text: String = d[5..18]
            .iter()
            .take_while(|&&b| b != b'\n' && b != 0)
            .map(|&b| if b.is_ascii_graphic() { b as char } else { ' ' })
            .collect();
        let text = text.trim().to_owned();
        (!text.is_empty()).then_some(text)
    });
    descriptor_name.or_else(|| {
        let id = u16::from_be_bytes([edid[8], edid[9]]);
        let letter = |shift: u16| {
            let v = ((id >> shift) & 0x1f) as u8;
            (1..=26).contains(&v).then(|| (b'A' + v - 1) as char)
        };
        let vendor: String = [letter(10)?, letter(5)?, letter(0)?].iter().collect();
        let product = u16::from_le_bytes([edid[10], edid[11]]);
        Some(format!("{vendor} {product:04X}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edid(name: Option<&[u8]>) -> Vec<u8> {
        let mut e = vec![0u8; 128];
        e[..8].copy_from_slice(&HEADER);
        // "DEL" = 00100 00101 01100.
        e[8] = 0x10;
        e[9] = 0xac;
        e[10] = 0x34;
        e[11] = 0x12;
        // A range limits descriptor first, the name second.
        e[54..59].copy_from_slice(&[0, 0, 0, 0xfd, 0]);
        if let Some(name) = name {
            e[72..77].copy_from_slice(&[0, 0, 0, 0xfc, 0]);
            let mut text = [b' '; 13];
            text[..name.len()].copy_from_slice(name);
            if name.len() < 13 {
                text[name.len()] = b'\n';
            }
            e[77..90].copy_from_slice(&text);
        }
        e
    }

    #[test]
    fn reads_the_display_name_descriptor() {
        assert_eq!(
            monitor_name(&edid(Some(b"DELL U2720Q"))).as_deref(),
            Some("DELL U2720Q")
        );
        assert_eq!(
            monitor_name(&edid(Some(b"ABCDEFGHIJKLM"))).as_deref(),
            Some("ABCDEFGHIJKLM")
        );
    }

    #[test]
    fn falls_back_to_vendor_and_product() {
        assert_eq!(monitor_name(&edid(None)).as_deref(), Some("DEL 1234"));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(monitor_name(&[0u8; 128]), None);
        assert_eq!(monitor_name(&HEADER), None);
    }
}
