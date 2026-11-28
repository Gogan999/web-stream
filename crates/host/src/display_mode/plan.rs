//! Display matching, mode choice and layout for a mode change, kept free of OS calls.

use ws_capture::modes::{Mode, pick_mode};

/// A mode a head can be set to.
#[derive(Debug, Clone, PartialEq)]
pub struct HeadMode {
    pub width: u32,
    pub height: u32,
    /// Vertical refresh in millihertz, 0 if unknown.
    pub refresh_mhz: u32,
    /// The backend's id for it; empty for a custom mode.
    pub id: String,
    /// Scales the compositor accepts with this mode; empty when any.
    pub scales: Vec<f64>,
}

impl HeadMode {
    /// Same size and refresh rate.
    pub fn same(&self, other: &HeadMode) -> bool {
        self.width == other.width
            && self.height == other.height
            && self.refresh_mhz == other.refresh_mhz
    }
}

/// An enabled output as the desktop session sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct Head {
    /// The compositor's or X server's name, e.g. `HDMI-A-1` or `HDMI-1`.
    pub name: String,
    pub edid: Option<Vec<u8>>,
    pub x: i32,
    pub y: i32,
    pub scale: f64,
    /// Turned 90 or 270 degrees.
    pub rotated: bool,
    /// Positions are in logical pixels: the mode's size divided by the scale.
    pub logical: bool,
    pub modes: Vec<HeadMode>,
    pub current: HeadMode,
    /// Takes any size as a custom mode.
    pub custom: bool,
}

/// What one head is set to.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub mode: HeadMode,
    pub x: i32,
    pub y: i32,
    pub scale: f64,
}

/// The head that is the kernel connector `connector` (with `edid`): an exact name, the same EDID,
/// the same connector under another naming (`HDMI-A-1` / `HDMI-1`), the only one of its type, or
/// the only head.
pub fn match_head(heads: &[Head], connector: &str, edid: Option<&[u8]>) -> Option<usize> {
    let edid = edid.filter(|e| !e.is_empty());
    let differs = |h: &Head| match (edid, h.edid.as_deref().filter(|e| !e.is_empty())) {
        (Some(a), Some(b)) => a != b,
        _ => false,
    };
    let unique = |pred: &dyn Fn(&Head) -> bool| {
        let mut found = heads.iter().enumerate().filter(|(_, h)| pred(h));
        match (found.next(), found.next()) {
            (Some((i, _)), None) => Some(i),
            _ => None,
        }
    };
    let want = canonical(connector);
    unique(&|h| !differs(h) && h.name.eq_ignore_ascii_case(connector))
        .or_else(|| unique(&|h| edid.is_some() && h.edid.as_deref() == edid))
        .or_else(|| unique(&|h| !differs(h) && want.is_some() && canonical(&h.name) == want))
        .or_else(|| {
            let kind = want.as_ref()?.0.clone();
            unique(&|h| !differs(h) && canonical(&h.name).is_some_and(|(k, _)| k == kind))
        })
        .or_else(|| (heads.len() == 1 && !differs(&heads[0])).then_some(0))
}

/// A connector name as (type, number) with the type spelled one way across the kernel, Xorg
/// drivers and compositors.
pub fn canonical(name: &str) -> Option<(String, u32)> {
    let (kind, number) = name.rsplit_once('-')?;
    let number = number.parse().ok()?;
    let kind = match kind.to_ascii_uppercase().as_str() {
        "HDMI" | "HDMI-A" => "HDMI".to_string(),
        "DP" | "DISPLAYPORT" => "DP".to_string(),
        "DVI" | "DVI-I" | "DVI-D" | "DVI-A" => "DVI".to_string(),
        other => other.to_string(),
    };
    Some((kind, number))
}

/// Smallest custom mode, as (long side, short side).
const MIN_CUSTOM: (u32, u32) = (320, 200);

fn hz(mhz: u32) -> u32 {
    (mhz + 500) / 1000
}

/// The mode for `head` that best fits a `width` x `height` desktop, by the same rules as Windows
/// (see [`pick_mode`]), then the refresh rate closest to the current one; a custom mode of that
/// size on a head that takes any.
pub fn choose(head: &Head, width: u32, height: u32) -> Option<HeadMode> {
    let (width, height) = if head.rotated {
        (height, width)
    } else {
        (width, height)
    };
    let current = &head.current;
    if head.custom {
        let (width, height) = (width & !1, height & !1);
        let fits = width.min(height) >= MIN_CUSTOM.1 && width.max(height) >= MIN_CUSTOM.0;
        return fits.then(|| HeadMode {
            width,
            height,
            refresh_mhz: current.refresh_mhz,
            id: String::new(),
            scales: Vec::new(),
        });
    }
    let modes: Vec<Mode> = head
        .modes
        .iter()
        .map(|m| Mode {
            width: m.width,
            height: m.height,
            refresh_hz: hz(m.refresh_mhz),
            bits_per_pixel: 32,
        })
        .collect();
    let best = pick_mode(&modes, width, height, hz(current.refresh_mhz))?;
    head.modes
        .iter()
        .filter(|m| {
            m.width == best.width && m.height == best.height && hz(m.refresh_mhz) == best.refresh_hz
        })
        .min_by_key(|m| {
            (
                m.refresh_mhz.abs_diff(current.refresh_mhz),
                m.id != current.id,
            )
        })
        .cloned()
}

/// The size `head` takes on the desktop in `mode` at `scale`.
fn extent(head: &Head, mode: &HeadMode, scale: f64) -> (i32, i32) {
    let (w, h) = if head.rotated {
        (mode.height, mode.width)
    } else {
        (mode.width, mode.height)
    };
    if head.logical && scale > 0.0 {
        (
            (f64::from(w) / scale).round() as i32,
            (f64::from(h) / scale).round() as i32,
        )
    } else {
        (w as i32, h as i32)
    }
}

/// `want` if `mode` takes it, else the current scale if it does, else the nearest it takes.
fn scale_for(mode: &HeadMode, want: f64, current: f64) -> f64 {
    let takes =
        |s: f64| mode.scales.is_empty() || mode.scales.iter().any(|&m| (m - s).abs() < 1e-6);
    if takes(want) {
        return want;
    }
    if takes(current) {
        return current;
    }
    mode.scales
        .iter()
        .copied()
        .min_by(|a, b| (a - want).abs().total_cmp(&(b - want).abs()))
        .unwrap_or(current)
}

/// Every head's setting with head `index` in `mode`: heads beside it on the right or below it move
/// by the change in its size, unless `positions` names a place for every head.
pub fn plan(
    heads: &[Head],
    index: usize,
    mode: HeadMode,
    scale: Option<f64>,
    positions: Option<&[(String, i32, i32)]>,
) -> Vec<Target> {
    let head = &heads[index];
    let scale = scale_for(&mode, scale.unwrap_or(head.scale), head.scale);
    let (ow, oh) = extent(head, &head.current, head.scale);
    let (nw, nh) = extent(head, &mode, scale);
    let overlaps = |a: i32, a_len: i32, b: i32, b_len: i32| a < b + b_len && b < a + a_len;
    let saved = positions.and_then(|p| {
        heads
            .iter()
            .map(|h| {
                p.iter()
                    .find(|(n, _, _)| *n == h.name)
                    .map(|&(_, x, y)| (x, y))
            })
            .collect::<Option<Vec<_>>>()
    });
    heads
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let (x, y) = match &saved {
                Some(p) => p[i],
                None if i == index => (h.x, h.y),
                None => {
                    let (w, ht) = extent(h, &h.current, h.scale);
                    let right = h.x >= head.x + ow && overlaps(h.y, ht, head.y, oh);
                    let below = h.y >= head.y + oh && overlaps(h.x, w, head.x, ow);
                    (
                        if right { h.x + nw - ow } else { h.x },
                        if below { h.y + nh - oh } else { h.y },
                    )
                }
            };
            if i == index {
                Target {
                    mode: mode.clone(),
                    x,
                    y,
                    scale,
                }
            } else {
                Target {
                    mode: h.current.clone(),
                    x,
                    y,
                    scale: h.scale,
                }
            }
        })
        .collect()
}

/// Whether applying `targets` would change nothing.
pub fn unchanged(heads: &[Head], targets: &[Target]) -> bool {
    heads.iter().zip(targets).all(|(h, t)| {
        h.current.same(&t.mode) && h.x == t.x && h.y == t.y && (h.scale - t.scale).abs() < 1e-6
    })
}

/// The desktop size of `mode` on `head` (turned with the head).
pub fn desktop_size(head: &Head, mode: &HeadMode) -> (u32, u32) {
    if head.rotated {
        (mode.height, mode.width)
    } else {
        (mode.width, mode.height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(width: u32, height: u32, refresh_mhz: u32) -> HeadMode {
        HeadMode {
            width,
            height,
            refresh_mhz,
            id: format!("{width}x{height}@{refresh_mhz}"),
            scales: Vec::new(),
        }
    }

    fn head(name: &str, x: i32, y: i32, current: HeadMode, modes: Vec<HeadMode>) -> Head {
        Head {
            name: name.into(),
            edid: None,
            x,
            y,
            scale: 1.0,
            rotated: false,
            logical: false,
            modes,
            current,
            custom: false,
        }
    }

    fn named(names: &[&str]) -> Vec<Head> {
        names
            .iter()
            .map(|n| head(n, 0, 0, mode(1920, 1080, 60000), vec![]))
            .collect()
    }

    #[test]
    fn connector_names_across_namings() {
        assert_eq!(canonical("HDMI-A-1"), Some(("HDMI".into(), 1)));
        assert_eq!(canonical("HDMI-1"), Some(("HDMI".into(), 1)));
        assert_eq!(canonical("DisplayPort-0"), Some(("DP".into(), 0)));
        assert_eq!(canonical("eDP-1"), Some(("EDP".into(), 1)));
        assert_eq!(canonical("DVI-D-2"), Some(("DVI".into(), 2)));
        assert_eq!(canonical("screen"), None);
    }

    #[test]
    fn matches_exact_then_other_namings() {
        let heads = named(&["eDP-1", "HDMI-A-1", "DP-1"]);
        assert_eq!(match_head(&heads, "HDMI-A-1", None), Some(1));
        let xorg = named(&["eDP-1", "HDMI-1", "DP-1", "DP-2"]);
        assert_eq!(match_head(&xorg, "HDMI-A-1", None), Some(1));
        assert_eq!(match_head(&xorg, "DP-2", None), Some(3));
        let amd = named(&["HDMI-A-0", "DisplayPort-0", "DisplayPort-1"]);
        assert_eq!(match_head(&amd, "HDMI-A-1", None), Some(0));
        assert_eq!(match_head(&amd, "DP-3", None), None);
        assert_eq!(match_head(&named(&["screen"]), "Virtual-1", None), Some(0));
        assert_eq!(match_head(&named(&["a", "b"]), "Virtual-1", None), None);
    }

    #[test]
    fn edid_decides_between_lookalike_names() {
        let mut heads = named(&["DP-0", "DP-1"]);
        heads[0].edid = Some(vec![1, 2, 3]);
        heads[1].edid = Some(vec![4, 5, 6]);
        assert_eq!(match_head(&heads, "DP-1", Some(&[1, 2, 3])), Some(0));
        assert_eq!(match_head(&heads, "DP-1", Some(&[4, 5, 6])), Some(1));
        assert_eq!(match_head(&heads, "DP-1", None), Some(1));
        assert_eq!(match_head(&heads, "DP-1", Some(&[9])), None);
    }

    #[test]
    fn chooses_like_windows_then_nearest_refresh() {
        let modes = vec![
            mode(2560, 1440, 59951),
            mode(1920, 1080, 60000),
            mode(1920, 1080, 59940),
            mode(1920, 1080, 144000),
            mode(1280, 720, 60000),
        ];
        let h = head("DP-1", 0, 0, mode(2560, 1440, 59951), modes);
        assert_eq!(choose(&h, 1920, 1080), Some(mode(1920, 1080, 59940)));
        assert_eq!(choose(&h, 2560, 1600), Some(mode(2560, 1440, 59951)));
        assert_eq!(choose(&h, 1366, 768), Some(mode(1280, 720, 60000)));
        assert_eq!(choose(&h, 800, 600), None);
        let h = Head {
            current: mode(1920, 1080, 60000),
            ..h
        };
        assert_eq!(choose(&h, 1920, 1080), Some(mode(1920, 1080, 60000)));
        let h = Head {
            current: mode(1920, 1080, 144000),
            ..h
        };
        assert_eq!(choose(&h, 1920, 1080), Some(mode(1920, 1080, 144000)));
    }

    #[test]
    fn rotated_heads_compare_turned_sizes() {
        let mut h = head(
            "DP-1",
            0,
            0,
            mode(1920, 1080, 60000),
            vec![mode(1920, 1080, 60000), mode(1280, 720, 60000)],
        );
        h.rotated = true;
        let m = choose(&h, 720, 1280).unwrap();
        assert_eq!(m, mode(1280, 720, 60000));
        assert_eq!(desktop_size(&h, &m), (720, 1280));
    }

    #[test]
    fn custom_heads_take_the_exact_size() {
        let mut h = head(
            "HEADLESS-1",
            0,
            0,
            mode(1920, 1080, 60000),
            vec![mode(1920, 1080, 60000)],
        );
        assert_eq!(choose(&h, 1280, 720), None);
        h.custom = true;
        let m = choose(&h, 1365, 768).unwrap();
        assert_eq!((m.width, m.height, m.refresh_mhz), (1364, 768, 60000));
        assert!(m.id.is_empty());
        assert_eq!(choose(&h, 16, 16), None);
        assert!(choose(&h, 200, 320).is_some());
    }

    #[test]
    fn neighbours_move_with_the_changed_edge() {
        let heads = vec![
            head("DP-1", 0, 0, mode(2560, 1440, 60000), vec![]),
            head("DP-2", 2560, 0, mode(1920, 1080, 60000), vec![]),
            head("DP-3", 0, 1440, mode(1920, 1080, 60000), vec![]),
        ];
        let t = plan(&heads, 0, mode(1920, 1080, 60000), None, None);
        assert_eq!((t[0].x, t[0].y, t[0].mode.width), (0, 0, 1920));
        assert_eq!((t[1].x, t[1].y), (1920, 0));
        assert_eq!((t[2].x, t[2].y), (0, 1080));
        assert_eq!(t[1].mode, heads[1].current);

        let t = plan(&heads, 1, mode(1280, 720, 60000), None, None);
        assert_eq!((t[0].x, t[1].x, t[2].y), (0, 2560, 1440));
    }

    #[test]
    fn saved_positions_win_when_complete() {
        let heads = vec![
            head("DP-1", 0, 0, mode(1920, 1080, 60000), vec![]),
            head("DP-2", 1920, 0, mode(1920, 1080, 60000), vec![]),
        ];
        let saved = vec![("DP-1".to_string(), 0, 0), ("DP-2".to_string(), 2560, 0)];
        let t = plan(&heads, 0, mode(2560, 1440, 60000), None, Some(&saved));
        assert_eq!((t[0].x, t[1].x), (0, 2560));
        let partial = vec![("DP-1".to_string(), 0, 0)];
        let t = plan(&heads, 0, mode(2560, 1440, 60000), None, Some(&partial));
        assert_eq!(t[1].x, 2560);
        assert!(!unchanged(&heads, &t));
        let same = plan(&heads, 0, heads[0].current.clone(), None, None);
        assert!(unchanged(&heads, &same));
    }

    #[test]
    fn logical_layout_divides_by_scale_and_keeps_a_valid_scale() {
        let mut big = mode(3840, 2160, 60000);
        big.scales = vec![1.0, 1.5, 2.0];
        let mut small = mode(1920, 1080, 60000);
        small.scales = vec![1.0, 1.25];
        let mut a = head("eDP-1", 0, 0, big, vec![]);
        a.scale = 2.0;
        a.logical = true;
        let mut b = head("HDMI-1", 1920, 0, mode(1920, 1080, 60000), vec![]);
        b.logical = true;
        let heads = vec![a, b];
        let t = plan(&heads, 0, small.clone(), None, None);
        assert_eq!((t[0].scale, t[1].x), (1.25, 1536));
        let t = plan(&heads, 0, small, Some(1.0), None);
        assert_eq!((t[0].scale, t[1].x), (1.0, 1920));
    }
}
