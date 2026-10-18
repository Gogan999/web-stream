//! Fits a captured display into the fixed stream size.

/// A rectangle in output pixels, `right`/`bottom` exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Rect {
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
}

/// Largest rectangle with the source's aspect ratio that fits centered in
/// `dst_w` x `dst_h`, with even edges so NV12 chroma lines up.
pub(crate) fn letterbox(src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Rect {
    let full = Rect {
        left: 0,
        top: 0,
        right: dst_w,
        bottom: dst_h,
    };
    if src_w == 0 || src_h == 0 {
        return full;
    }
    let (sw, sh, dw, dh) = (src_w as u64, src_h as u64, dst_w as u64, dst_h as u64);
    let (w, h) = if sw * dh > sh * dw {
        (dw, (sh * dw / sw).max(2))
    } else {
        ((sw * dh / sh).max(2), dh)
    };
    let even = |v: u64| (v & !1) as u32;
    let (w, h) = (even(w), even(h));
    let left = even((dw - w as u64) / 2);
    let top = even((dh - h as u64) / 2);
    Rect {
        left,
        top,
        right: left + w,
        bottom: top + h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_aspect_fills() {
        assert_eq!(
            letterbox(3840, 2160, 1920, 1080),
            Rect {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080
            }
        );
    }

    #[test]
    fn ultrawide_gets_bars_top_and_bottom() {
        let r = letterbox(3440, 1440, 1920, 1080);
        assert_eq!((r.left, r.right), (0, 1920));
        assert_eq!(r.bottom - r.top, 802);
        assert_eq!(r.top % 2, 0);
    }

    #[test]
    fn portrait_gets_bars_left_and_right() {
        let r = letterbox(1080, 1920, 1920, 1080);
        assert_eq!((r.top, r.bottom), (0, 1080));
        assert_eq!(r.right - r.left, 606);
        assert!(r.left > 0 && r.right < 1920);
    }
}
