//! Picks a desktop resolution for "match the host to the browser", kept
//! free of OS calls so it can be tested.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Mode {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub bits_per_pixel: u32,
}

/// The largest true-colour mode that fits in `width` x `height`, preferring
/// the current refresh rate, then the highest. `None` if nothing fits.
pub(crate) fn pick_mode(modes: &[Mode], width: u32, height: u32, current_hz: u32) -> Option<Mode> {
    modes
        .iter()
        .filter(|m| m.bits_per_pixel >= 32 && m.width <= width && m.height <= height)
        .max_by_key(|m| {
            (
                u64::from(m.width) * u64::from(m.height),
                m.refresh_hz == current_hz,
                m.refresh_hz,
            )
        })
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(width: u32, height: u32, refresh_hz: u32) -> Mode {
        Mode {
            width,
            height,
            refresh_hz,
            bits_per_pixel: 32,
        }
    }

    #[test]
    fn exact_size_at_current_refresh() {
        let modes = [
            m(2560, 1440, 144),
            m(1920, 1080, 60),
            m(1920, 1080, 144),
            m(1920, 1080, 120),
        ];
        assert_eq!(pick_mode(&modes, 1920, 1080, 144), Some(m(1920, 1080, 144)));
        // The current rate isn't offered at this size: take the highest.
        assert_eq!(pick_mode(&modes, 1920, 1080, 165), Some(m(1920, 1080, 144)));
    }

    #[test]
    fn largest_that_fits_when_no_exact_match() {
        let modes = [
            m(2560, 1440, 60),
            m(1920, 1200, 60),
            m(1920, 1080, 60),
            m(1280, 720, 60),
        ];
        // A 2560x1600 laptop screen: 2560x1440 fits.
        assert_eq!(pick_mode(&modes, 2560, 1600, 60), Some(m(2560, 1440, 60)));
        assert_eq!(pick_mode(&modes, 1366, 768, 60), Some(m(1280, 720, 60)));
        assert_eq!(pick_mode(&modes, 800, 600, 60), None);
    }

    #[test]
    fn skips_low_colour_modes() {
        let low = Mode {
            bits_per_pixel: 16,
            ..m(1920, 1080, 60)
        };
        assert_eq!(
            pick_mode(&[low, m(1280, 720, 60)], 1920, 1080, 60),
            Some(m(1280, 720, 60))
        );
    }
}
