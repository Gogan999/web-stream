//! Reads the host pointer's shape and position, so the browser can show it
//! (DXGI leaves the pointer out of the video). On Windows the reader is
//! [`CursorReader`]; poll it every few milliseconds.

use std::collections::HashMap;
use std::sync::Arc;

use ws_proto::{CursorImage, CursorKind};

#[cfg(windows)]
pub use crate::win_cursor::CursorReader;

/// The pointer as last read.
#[derive(Debug, Clone, PartialEq)]
pub struct CursorState {
    pub look: CursorLook,
    /// The hotspot, in virtual-desktop pixels.
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CursorLook {
    pub kind: CursorKind,
    /// The host's image of it, when it could be read; the same id for the
    /// same image for as long as the reader lives.
    pub image: Option<(u32, Arc<CursorImage>)>,
}

impl CursorLook {
    pub const HIDDEN: Self = Self {
        kind: CursorKind::Hidden,
        image: None,
    };
}

/// Gives each distinct custom image a small id, so the browser can cache it
/// and the host sends each image once.
#[derive(Debug, Default)]
pub struct ImageIds {
    ids: HashMap<Arc<CursorImage>, u32>,
    next: u32,
}

/// Distinct images remembered; past this the table starts over (an app
/// animating its pointer by creating new ones).
const MAX_IMAGES: usize = 256;

impl ImageIds {
    pub fn get(&mut self, image: CursorImage) -> (u32, Arc<CursorImage>) {
        if let Some((image, &id)) = self.ids.get_key_value(&image) {
            return (id, image.clone());
        }
        if self.ids.len() >= MAX_IMAGES {
            self.ids.clear();
        }
        // Ids keep counting up across a clear, so the browser never shows a
        // stale image for a reused id.
        let id = self.next;
        self.next = self.next.wrapping_add(1);
        let image = Arc::new(image);
        self.ids.insert(image.clone(), id);
        (id, image)
    }
}

/// `image` scaled down (nearest pixel) to fit `max` per side, hotspot
/// included; smaller images are returned as they are.
pub fn fit(image: CursorImage, max: u32) -> CursorImage {
    let side = image.width.max(image.height);
    if side <= max || max == 0 {
        return image;
    }
    let scale = |v: u32| (u64::from(v) * u64::from(max) / u64::from(side)) as u32;
    let (width, height) = (scale(image.width).max(1), scale(image.height).max(1));
    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height {
        let sy = (u64::from(y) * u64::from(image.height) / u64::from(height)) as usize;
        for x in 0..width {
            let sx = (u64::from(x) * u64::from(image.width) / u64::from(width)) as usize;
            let i = (sy * image.width as usize + sx) * 4;
            rgba.extend_from_slice(&image.rgba[i..i + 4]);
        }
    }
    CursorImage {
        width,
        height,
        hot_x: scale(image.hot_x).min(width - 1),
        hot_y: scale(image.hot_y).min(height - 1),
        rgba,
    }
}

/// RGBA from a colour pointer's 32-bit BGRA pixels. Pointers without an
/// alpha channel take it from the AND mask (also 32-bit, set = transparent).
pub fn rgba_from_color(bgra: &[u8], mask: Option<&[u8]>) -> Vec<u8> {
    let has_alpha = bgra.chunks_exact(4).any(|p| p[3] != 0);
    let mut out = Vec::with_capacity(bgra.len());
    for (i, p) in bgra.chunks_exact(4).enumerate() {
        let alpha = if has_alpha {
            p[3]
        } else {
            match mask.and_then(|m| m.get(i * 4)) {
                Some(&m) if m != 0 => 0,
                _ => 255,
            }
        };
        out.extend_from_slice(&[p[2], p[1], p[0], alpha]);
    }
    out
}

/// RGBA from a monochrome pointer's AND and XOR masks (each 32-bit, one
/// pixel per 4 bytes). Pixels that invert the screen (the I-beam's kind)
/// can't be drawn by the browser; they become black with a white edge so
/// they show on any background.
pub fn rgba_from_monochrome(and: &[u8], xor: &[u8], width: usize) -> Vec<u8> {
    let bit = |m: &[u8], i: usize| m.get(i * 4).is_some_and(|&b| b != 0);
    let n = and.len().min(xor.len()) / 4;
    let mut out = vec![0u8; n * 4];
    let invert = |i: usize| bit(and, i) && bit(xor, i);
    for i in 0..n {
        let px = match (bit(and, i), bit(xor, i)) {
            (false, false) => [0, 0, 0, 255],
            (false, true) => [255, 255, 255, 255],
            (true, false) => [0, 0, 0, 0],
            (true, true) => [0, 0, 0, 255],
        };
        out[i * 4..i * 4 + 4].copy_from_slice(&px);
    }
    // White edge around inverting pixels, on transparent neighbours.
    if let Some(height) = n.checked_div(width) {
        for i in 0..n {
            if out[i * 4 + 3] != 0 {
                continue;
            }
            let (x, y) = (i % width, i / width);
            let near = [(-1, 0), (1, 0), (0, -1), (0, 1)].iter().any(|(dx, dy)| {
                let (nx, ny) = (x as isize + dx, y as isize + dy);
                nx >= 0
                    && ny >= 0
                    && (nx as usize) < width
                    && (ny as usize) < height
                    && invert(ny as usize * width + nx as usize)
            });
            if near {
                out[i * 4..i * 4 + 4].copy_from_slice(&[255, 255, 255, 255]);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(fill: u8) -> CursorImage {
        CursorImage {
            width: 1,
            height: 1,
            hot_x: 0,
            hot_y: 0,
            rgba: vec![fill; 4],
        }
    }

    #[test]
    fn same_image_same_id() {
        let mut ids = ImageIds::default();
        let a = ids.get(image(1)).0;
        let b = ids.get(image(2)).0;
        assert_ne!(a, b);
        assert_eq!(ids.get(image(1)).0, a);
    }

    #[test]
    fn ids_are_not_reused_after_the_table_fills() {
        let mut ids = ImageIds::default();
        let mut seen = std::collections::HashSet::new();
        for i in 0..(MAX_IMAGES as u32 * 2 + 3) {
            let mut img = image(0);
            img.rgba = i.to_le_bytes().to_vec();
            let id = ids.get(img).0;
            assert!(seen.insert(id), "id {id} reused");
        }
    }

    #[test]
    fn big_images_scale_down_with_their_hotspot() {
        let rgba = (0..256 * 128).flat_map(|i: u32| i.to_le_bytes()).collect();
        let big = CursorImage {
            width: 256,
            height: 128,
            hot_x: 100,
            hot_y: 127,
            rgba,
        };
        let small = fit(big, 128);
        assert_eq!((small.width, small.height), (128, 64));
        assert_eq!((small.hot_x, small.hot_y), (50, 63));
        assert_eq!(small.rgba.len(), 128 * 64 * 4);
        // Pixel (1, 1) comes from (2, 2).
        assert_eq!(
            &small.rgba[(128 + 1) * 4..(128 + 2) * 4],
            &(256 * 2 + 2u32).to_le_bytes()
        );
        assert_eq!(fit(image(3), 128), image(3));
    }

    #[test]
    fn color_uses_alpha_or_falls_back_to_mask() {
        // BGRA with alpha: swizzled, alpha kept.
        assert_eq!(rgba_from_color(&[1, 2, 3, 128], None), vec![3, 2, 1, 128]);
        // No alpha anywhere: the mask decides (set = transparent).
        let bgra = [10, 20, 30, 0, 40, 50, 60, 0];
        let mask = [255, 255, 255, 0, 0, 0, 0, 0];
        assert_eq!(
            rgba_from_color(&bgra, Some(&mask)),
            vec![30, 20, 10, 0, 60, 50, 40, 255]
        );
    }

    #[test]
    fn monochrome_masks() {
        let on = [255u8, 255, 255, 0];
        let off = [0u8; 4];
        let and = [off, off, on, on].concat();
        let xor = [off, on, off, on].concat();
        let out = rgba_from_monochrome(&and, &xor, 4);
        assert_eq!(&out[0..4], &[0, 0, 0, 255]); // black
        assert_eq!(&out[4..8], &[255, 255, 255, 255]); // white
        // Transparent, but next to an inverting pixel: white edge.
        assert_eq!(&out[8..12], &[255, 255, 255, 255]);
        assert_eq!(&out[12..16], &[0, 0, 0, 255]); // inverting: black
    }
}
