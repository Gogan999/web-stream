//! The machine-readable stamp drawn into the top of every synthetic frame,
//! so a test reading the decoded video in the browser knows which frame it
//! is looking at, which display it came from and when it was captured.
//!
//! Two rows of [`COLS`] square cells across the top of the frame, each cell
//! `width / COLS` pixels wide, black (0) or white (1), most significant bit
//! first:
//!
//! - row 0: the capture time, Unix milliseconds, low 32 bits;
//! - row 1: the display id (4 bits), the frame index (20 bits) and a CRC-8
//!   (polynomial 0x07, initial value 0) over the 7 bytes before it: the
//!   time (big-endian) then the display and frame index (24 bits,
//!   big-endian).
//!
//! The cells are big enough to survive any bitrate the encoder runs at; the
//! CRC catches a read of a frame torn by packet loss. The browser side is
//! `web/test/lib/stamp.js`; both test against the same vector.

/// Cells per row.
pub const COLS: u32 = 32;

/// What a stamp says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    /// Capture time, Unix milliseconds (only the low 32 bits are drawn).
    pub unix_ms: u64,
    /// Only the low 4 bits are drawn.
    pub display: u32,
    /// Only the low 20 bits are drawn.
    pub frame: u64,
}

pub fn crc8(bytes: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &b in bytes {
        crc ^= b;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ 0x07
            } else {
                crc << 1
            };
        }
    }
    crc
}

impl Stamp {
    /// The two rows' 32-bit words.
    pub fn words(&self) -> [u32; 2] {
        let time = self.unix_ms as u32;
        let id = ((self.display & 0xf) << 20) | (self.frame as u32 & 0xf_ffff);
        let t = time.to_be_bytes();
        let i = id.to_be_bytes();
        let crc = crc8(&[t[0], t[1], t[2], t[3], i[1], i[2], i[3]]);
        [time, (id << 8) | u32::from(crc)]
    }

    /// Reads a stamp back from a luma plane, or `None` if the CRC fails.
    pub fn read(y: &[u8], width: usize) -> Option<Self> {
        let cell = width / COLS as usize;
        let mut words = [0u32; 2];
        for (row, word) in words.iter_mut().enumerate() {
            let py = row * cell + cell / 2;
            for col in 0..COLS as usize {
                let px = col * cell + cell / 2;
                *word = (*word << 1) | u32::from(y[py * width + px] >= 128);
            }
        }
        let [time, rest] = words;
        let id = rest >> 8;
        let t = time.to_be_bytes();
        let i = id.to_be_bytes();
        if crc8(&[t[0], t[1], t[2], t[3], i[1], i[2], i[3]]) != rest as u8 {
            return None;
        }
        Some(Self {
            unix_ms: u64::from(time),
            display: id >> 20,
            frame: u64::from(id & 0xf_ffff),
        })
    }
}

/// Luma of a 0 and a 1 cell (video range).
const BLACK: u8 = 16;
const WHITE: u8 = 235;

/// Chroma (U, V) of each fake display's background, so a person watching
/// can tell them apart too.
const TINTS: [(u8, u8); 4] = [(90, 200), (200, 90), (80, 80), (170, 170)];

/// Draws one synthetic frame into an I420 buffer of `width` x `height`
/// (both even): a background that moves every frame (a scrolling gradient
/// and a bar), tinted per display, under the stamp.
pub fn draw(buf: &mut [u8], width: usize, height: usize, stamp: &Stamp) {
    let (y_plane, chroma) = buf.split_at_mut(width * height);
    let (u_plane, v_plane) = chroma.split_at_mut(width * height / 4);
    let cell = width / COLS as usize;
    let band = (cell * 2).min(height);

    // Background: one row, copied down.
    let shift = (stamp.frame as usize * 8) % width.max(1);
    let bar_w = (width / 16).max(2);
    let bar_x = (stamp.frame as usize * 12) % width.max(1);
    let row: Vec<u8> = (0..width)
        .map(|x| {
            if (x + width - bar_x) % width < bar_w {
                220
            } else {
                40 + (((x + shift) % 256) / 2) as u8
            }
        })
        .collect();
    for line in y_plane.chunks_exact_mut(width).skip(band) {
        line.copy_from_slice(&row);
    }

    // The stamp.
    let words = stamp.words();
    for (r, word) in words.iter().enumerate() {
        let mut line = vec![BLACK; width];
        for col in 0..COLS as usize {
            let bit = (word >> (COLS as usize - 1 - col)) & 1;
            let v = if bit == 1 { WHITE } else { BLACK };
            line[col * cell..(col + 1) * cell].fill(v);
        }
        for y in r * cell..((r + 1) * cell).min(height) {
            y_plane[y * width..(y + 1) * width].copy_from_slice(&line);
        }
    }

    let (tu, tv) = TINTS[(stamp.display & 3) as usize];
    let cw = width / 2;
    let chroma_band = band.div_ceil(2);
    for (i, (u, v)) in u_plane
        .chunks_exact_mut(cw)
        .zip(v_plane.chunks_exact_mut(cw))
        .enumerate()
    {
        let (fu, fv) = if i < chroma_band {
            (128, 128)
        } else {
            (tu, tv)
        };
        u.fill(fu);
        v.fill(fv);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The vector `web/test/unit/stamp.test.js` checks too.
    #[test]
    fn shared_vector() {
        let s = Stamp {
            unix_ms: 0x1_2345_6789,
            display: 2,
            frame: 0xabcde,
        };
        assert_eq!(s.words(), [0x2345_6789, 0x2abc_de00 | 0x4d]);
    }

    #[test]
    fn reads_back_what_it_draws() {
        for (w, h) in [(640, 360), (1280, 720), (1922, 1080)] {
            let mut buf = vec![0; w * h * 3 / 2];
            let s = Stamp {
                unix_ms: 1_762_650_000_123,
                display: 1,
                frame: 77,
            };
            draw(&mut buf, w, h, &s);
            let back = Stamp::read(&buf[..w * h], w).expect("crc");
            assert_eq!(back.unix_ms, s.unix_ms & 0xffff_ffff);
            assert_eq!((back.display, back.frame), (1, 77));
        }
    }

    #[test]
    fn a_flipped_bit_fails_the_crc() {
        let (w, h) = (640, 360);
        let mut buf = vec![0; w * h * 3 / 2];
        draw(
            &mut buf,
            w,
            h,
            &Stamp {
                unix_ms: 5,
                display: 0,
                frame: 9,
            },
        );
        let cell = w / COLS as usize;
        let at = (cell / 2) * w + 3 * cell + cell / 2;
        buf[at] = 255 - buf[at];
        assert!(Stamp::read(&buf[..w * h], w).is_none());
    }
}
