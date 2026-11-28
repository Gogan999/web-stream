//! Bitstream writing for parameter sets and headers.

/// Writes bits most significant first.
#[derive(Default)]
pub(crate) struct BitWriter {
    bytes: Vec<u8>,
    /// Bits used in the last byte, 0 when it is full or there is none.
    used: u8,
}

impl BitWriter {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Bits written so far.
    pub(crate) fn len(&self) -> usize {
        if self.used == 0 {
            self.bytes.len() * 8
        } else {
            (self.bytes.len() - 1) * 8 + usize::from(self.used)
        }
    }

    pub(crate) fn bit(&mut self, b: bool) {
        if self.used == 0 {
            self.bytes.push(0);
        }
        if b {
            *self.bytes.last_mut().unwrap() |= 0x80 >> self.used;
        }
        self.used = (self.used + 1) % 8;
    }

    pub(crate) fn flag(&mut self, b: bool) {
        self.bit(b);
    }

    /// The low `n` bits of `v`.
    pub(crate) fn u(&mut self, n: u32, v: u64) {
        for i in (0..n).rev() {
            self.bit((v >> i) & 1 != 0);
        }
    }

    /// Unsigned Exp-Golomb.
    pub(crate) fn ue(&mut self, v: u32) {
        let v = u64::from(v) + 1;
        let bits = 64 - v.leading_zeros();
        self.u(bits - 1, 0);
        self.u(bits, v);
    }

    /// Signed Exp-Golomb.
    pub(crate) fn se(&mut self, v: i32) {
        let mapped = if v > 0 {
            (v as u32) * 2 - 1
        } else {
            v.unsigned_abs() * 2
        };
        self.ue(mapped);
    }

    /// `rbsp_trailing_bits()`.
    pub(crate) fn trailing(&mut self) {
        self.bit(true);
        self.align();
    }

    /// Zero bits up to the next byte boundary.
    pub(crate) fn align(&mut self) {
        while self.used != 0 {
            self.bit(false);
        }
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

/// An Annex-B NAL unit: start code, `header`, and `rbsp` with emulation prevention bytes.
pub(crate) fn nal(header: &[u8], rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + header.len() + rbsp.len() + rbsp.len() / 64);
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend_from_slice(header);
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

/// AV1 `leb128()`.
pub(crate) fn leb128(mut v: u64, out: &mut Vec<u8>) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_golomb() {
        let mut w = BitWriter::new();
        for v in [0, 1, 2, 3, 7] {
            w.ue(v);
        }
        // 1 010 011 00100 0001000
        assert_eq!(w.len(), 1 + 3 + 3 + 5 + 7);
        w.align();
        assert_eq!(w.bytes(), &[0b1010_0110, 0b0100_0001, 0b0000_0000]);
        let mut w = BitWriter::new();
        for v in [0, 1, -1, 2, -2] {
            w.se(v);
        }
        // 1 010 011 00100 00101
        w.align();
        assert_eq!(w.bytes(), &[0b1010_0110, 0b0100_0010, 0b1000_0000]);
    }

    #[test]
    fn fixed_width_and_trailing_bits() {
        let mut w = BitWriter::new();
        w.u(4, 0xa);
        w.u(8, 0xff);
        w.trailing();
        assert_eq!(w.into_bytes(), vec![0xaf, 0xf8]);
    }

    #[test]
    fn emulation_prevention() {
        assert_eq!(
            nal(&[0x67], &[0, 0, 0, 0, 0, 1, 0, 0, 4, 0, 0]),
            vec![0, 0, 0, 1, 0x67, 0, 0, 3, 0, 0, 3, 0, 1, 0, 0, 4, 0, 0]
        );
    }

    #[test]
    fn leb128_values() {
        let enc = |v| {
            let mut out = Vec::new();
            leb128(v, &mut out);
            out
        };
        assert_eq!(enc(0), [0]);
        assert_eq!(enc(127), [0x7f]);
        assert_eq!(enc(128), [0x80, 0x01]);
        assert_eq!(enc(300), [0xac, 0x02]);
    }
}
