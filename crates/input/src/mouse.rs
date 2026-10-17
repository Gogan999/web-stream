//! Absolute mouse mapping from the streamed display to SendInput's
//! virtual-desktop coordinates.

/// A rectangle in physical virtual-desktop pixels: a display as DXGI reports
/// it in `DXGI_OUTPUT_DESC::DesktopCoordinates`, or the whole virtual desktop.
/// The host process is per-monitor DPI aware, so the system metrics it reads
/// for the virtual desktop are physical pixels too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayRect {
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
}

/// Maps `x`, `y` (0..=65535 across `display`) to SendInput's
/// `MOUSEEVENTF_VIRTUALDESK` coordinates (0..=65535 across `desktop`).
pub fn to_virtual_desk(x: u16, y: u16, display: DisplayRect, desktop: DisplayRect) -> (i32, i32) {
    let axis = |n: u16, origin: i32, size: i32, d_origin: i32, d_size: i32| {
        // Pixel on the display, then that pixel's position across the desktop.
        let px = origin as i64 + (n as i64 * (size.max(1) as i64 - 1)) / 65535;
        let span = (d_size.max(2) as i64) - 1;
        ((px - d_origin as i64) * 65535 / span).clamp(0, 65535) as i32
    };
    (
        axis(x, display.left, display.width, desktop.left, desktop.width),
        axis(y, display.top, display.height, desktop.top, desktop.height),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn rect(left: i32, top: i32, width: i32, height: i32) -> DisplayRect {
        DisplayRect {
            left,
            top,
            width,
            height,
        }
    }

    #[test]
    fn single_display_maps_corner_to_corner() {
        let d = rect(0, 0, 1920, 1080);
        assert_eq!(to_virtual_desk(0, 0, d, d), (0, 0));
        assert_eq!(to_virtual_desk(65535, 65535, d, d), (65535, 65535));
    }

    #[test]
    fn display_left_of_primary_has_negative_origin() {
        // 2560x1440 to the left of a 1920x1080 primary, top-aligned.
        let left = rect(-2560, 0, 2560, 1440);
        let primary = rect(0, 0, 1920, 1080);
        let desktop = rect(-2560, 0, 4480, 1440);
        assert_eq!(to_virtual_desk(0, 0, left, desktop), (0, 0));
        // Right edge of the left display is pixel -1, just short of the primary.
        let (x, _) = to_virtual_desk(65535, 0, left, desktop);
        assert_eq!(x, (2559 * 65535) / 4479);
        // Top-left of the primary is desktop pixel 2560.
        let (x, y) = to_virtual_desk(0, 0, primary, desktop);
        assert_eq!((x, y), ((2560 * 65535) / 4479, 0));
        // Bottom of the shorter primary is not the bottom of the desktop.
        let (_, y) = to_virtual_desk(0, 65535, primary, desktop);
        assert_eq!(y, (1079 * 65535) / 1439);
    }

    #[test]
    fn display_above_primary() {
        let top = rect(0, -1080, 1920, 1080);
        let desktop = rect(0, -1080, 1920, 2160);
        assert_eq!(to_virtual_desk(0, 0, top, desktop), (0, 0));
        assert_eq!(
            to_virtual_desk(0, 65535, top, desktop).1,
            (1079 * 65535) / 2159
        );
    }

    #[test]
    fn degenerate_sizes_do_not_panic() {
        let zero = rect(0, 0, 0, 0);
        assert_eq!(to_virtual_desk(65535, 65535, zero, zero), (0, 0));
    }
}
