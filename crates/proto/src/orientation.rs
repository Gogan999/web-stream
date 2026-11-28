//! The eight ways a picture can be laid on a display: quarter turns and mirroring, as KMS planes
//! and panels describe them.

/// `DRM_MODE_ROTATE_*` and `DRM_MODE_REFLECT_*` bits of a plane's `rotation` property.
pub mod drm_rotation {
    pub const ROTATE_0: u64 = 1 << 0;
    pub const ROTATE_90: u64 = 1 << 1;
    pub const ROTATE_180: u64 = 1 << 2;
    pub const ROTATE_270: u64 = 1 << 3;
    pub const REFLECT_X: u64 = 1 << 4;
    pub const REFLECT_Y: u64 = 1 << 5;
}

/// `DRM_MODE_PANEL_ORIENTATION_*` values of a connector's `panel orientation` property.
pub mod panel_orientation {
    pub const BOTTOM_UP: u64 = 1;
    pub const LEFT_UP: u64 = 2;
    pub const RIGHT_UP: u64 = 3;
}

/// A mirror in x (when `mirror`) followed by `quarter` turns counter-clockwise, in image space
/// (y down), the order and direction DRM uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Transform {
    quarter: u8,
    mirror: bool,
}

/// An affine map on the unit square: `x' = m[0][0] x + m[0][1] y + m[0][2]`, same for `y'`.
pub type Affine = [[i32; 3]; 2];

impl Transform {
    pub const IDENTITY: Self = Self {
        quarter: 0,
        mirror: false,
    };

    pub const fn new(quarter: u8, mirror: bool) -> Self {
        Self {
            quarter: quarter % 4,
            mirror,
        }
    }

    /// Every transform.
    pub fn all() -> impl Iterator<Item = Self> {
        (0..8).map(|i| Self::new(i % 4, i >= 4))
    }

    /// Decodes a plane's `rotation` property.
    pub fn from_drm_rotation(bits: u64) -> Self {
        use drm_rotation::*;
        let quarter = match bits & (ROTATE_0 | ROTATE_90 | ROTATE_180 | ROTATE_270) {
            ROTATE_90 => 1,
            ROTATE_180 => 2,
            ROTATE_270 => 3,
            _ => 0,
        };
        let (x, y) = (bits & REFLECT_X != 0, bits & REFLECT_Y != 0);
        Self::new(quarter + if y { 2 } else { 0 }, x != y)
    }

    /// The plane rotation that shows an upright picture on a panel mounted as
    /// `panel orientation` says (as the kernel's `drm_client_rotation`).
    pub fn from_panel_orientation(value: u64) -> Self {
        match value {
            panel_orientation::BOTTOM_UP => Self::new(2, false),
            panel_orientation::LEFT_UP => Self::new(1, false),
            panel_orientation::RIGHT_UP => Self::new(3, false),
            _ => Self::IDENTITY,
        }
    }

    pub fn quarter(self) -> u8 {
        self.quarter
    }

    pub fn mirror(self) -> bool {
        self.mirror
    }

    pub fn is_identity(self) -> bool {
        self == Self::IDENTITY
    }

    /// Whether width and height trade places.
    pub fn swaps(self) -> bool {
        self.quarter % 2 == 1
    }

    /// The size of a `width` x `height` picture once transformed.
    pub fn size(self, width: u32, height: u32) -> (u32, u32) {
        if self.swaps() {
            (height, width)
        } else {
            (width, height)
        }
    }

    /// `self`, then `next`.
    pub fn then(self, next: Self) -> Self {
        let turned = if next.mirror {
            4 - self.quarter
        } else {
            self.quarter
        };
        Self::new(next.quarter + turned, self.mirror != next.mirror)
    }

    pub fn inverse(self) -> Self {
        if self.mirror {
            self
        } else {
            Self::new(4 - self.quarter, false)
        }
    }

    /// Where the transform takes each point of the unit square.
    pub fn affine(self) -> Affine {
        const MIRROR: Affine = [[-1, 0, 1], [0, 1, 0]];
        const TURNS: [Affine; 4] = [
            [[1, 0, 0], [0, 1, 0]],
            [[0, 1, 0], [-1, 0, 1]],
            [[-1, 0, 1], [0, -1, 1]],
            [[0, -1, 1], [1, 0, 0]],
        ];
        let turn = TURNS[self.quarter as usize];
        if self.mirror {
            compose(turn, MIRROR)
        } else {
            turn
        }
    }

    /// Where point `(x, y)` (pixel edges) of a `size` picture lands.
    pub fn map_point(self, (x, y): (i64, i64), size: (u32, u32)) -> (i64, i64) {
        let (w, h) = self.size(size.0, size.1);
        self.map_with(x, y, i64::from(w), i64::from(h))
    }

    /// Where pixel `(x, y)` of a `size` picture lands.
    pub fn map_pixel(self, (x, y): (u32, u32), size: (u32, u32)) -> (u32, u32) {
        let (w, h) = self.size(size.0, size.1);
        let (x, y) = self.map_with(
            i64::from(x),
            i64::from(y),
            i64::from(w) - 1,
            i64::from(h) - 1,
        );
        (x.max(0) as u32, y.max(0) as u32)
    }

    /// Where rectangle `(x, y, width, height)` inside a `size` picture lands.
    pub fn map_rect(
        self,
        (x, y, w, h): (i64, i64, u32, u32),
        size: (u32, u32),
    ) -> (i64, i64, u32, u32) {
        let a = self.map_point((x, y), size);
        let b = self.map_point((x + i64::from(w), y + i64::from(h)), size);
        let (w, h) = self.size(w, h);
        (a.0.min(b.0), a.1.min(b.1), w, h)
    }

    /// The pixels (`width` x `height`, rows from the top) of a picture once transformed.
    pub fn map_pixels<T: Copy + Default>(self, pixels: &[T], size: (u32, u32)) -> Vec<T> {
        let (w, h) = self.size(size.0, size.1);
        let mut out = vec![T::default(); pixels.len()];
        for (i, &p) in pixels.iter().enumerate().take((size.0 * size.1) as usize) {
            let (x, y) = self.map_pixel((i as u32 % size.0, i as u32 / size.0), size);
            if x < w && y < h {
                out[(y * w + x) as usize] = p;
            }
        }
        out
    }

    fn map_with(self, x: i64, y: i64, w: i64, h: i64) -> (i64, i64) {
        let m = self.affine();
        let row = |r: [i32; 3], extent: i64| {
            i64::from(r[0]) * x + i64::from(r[1]) * y + i64::from(r[2]) * extent
        };
        (row(m[0], w), row(m[1], h))
    }
}

/// `outer` after `inner`.
pub fn compose(outer: Affine, inner: Affine) -> Affine {
    let mut m = [[0; 3]; 2];
    for (r, row) in m.iter_mut().enumerate() {
        for (c, v) in row.iter_mut().enumerate() {
            *v = outer[r][0] * inner[0][c] + outer[r][1] * inner[1][c];
        }
        row[2] += outer[r][2];
    }
    m
}

pub fn apply(m: Affine, (x, y): (f64, f64)) -> (f64, f64) {
    (
        f64::from(m[0][0]) * x + f64::from(m[0][1]) * y + f64::from(m[0][2]),
        f64::from(m[1][0]) * x + f64::from(m[1][1]) * y + f64::from(m[1][2]),
    )
}

/// The transform from the scanout buffer to the picture as seen: none for a rotated plane, the
/// panel's mounting undone for an unrotated one.
pub fn upright(plane: Transform, panel: Transform) -> Transform {
    if plane.is_identity() {
        panel.inverse()
    } else {
        Transform::IDENTITY
    }
}

/// The size of the picture as seen, from the CRTC's mode size.
pub fn upright_size(mode: (u32, u32), plane: Transform, panel: Transform) -> (u32, u32) {
    let (w, h) = plane.size(mode.0, mode.1);
    upright(plane, panel).size(w, h)
}

#[cfg(test)]
mod tests {
    use super::drm_rotation::*;
    use super::*;

    const W: f64 = 1.0;
    const H: f64 = 1.0;

    /// The kernel's `drm_rect_rotate` on a point of a `W` x `H` buffer.
    fn kernel_rotate(bits: u64, (mut x, mut y): (f64, f64)) -> (f64, f64) {
        if bits & REFLECT_X != 0 {
            x = W - x;
        }
        if bits & REFLECT_Y != 0 {
            y = H - y;
        }
        match bits & 0xf {
            ROTATE_90 => (y, W - x),
            ROTATE_180 => (W - x, H - y),
            ROTATE_270 => (H - y, x),
            _ => (x, y),
        }
    }

    const POINTS: [(f64, f64); 5] = [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (0.25, 0.75), (0.6, 0.1)];

    fn close(a: (f64, f64), b: (f64, f64)) -> bool {
        (a.0 - b.0).abs() < 1e-9 && (a.1 - b.1).abs() < 1e-9
    }

    #[test]
    fn rotation_property_decodes_as_the_kernel_applies_it() {
        for rotate in [ROTATE_0, ROTATE_90, ROTATE_180, ROTATE_270] {
            for reflect in [0, REFLECT_X, REFLECT_Y, REFLECT_X | REFLECT_Y] {
                let bits = rotate | reflect;
                let t = Transform::from_drm_rotation(bits);
                for p in POINTS {
                    assert!(
                        close(apply(t.affine(), p), kernel_rotate(bits, p)),
                        "{bits:#x} at {p:?}"
                    );
                }
            }
        }
        assert_eq!(Transform::from_drm_rotation(ROTATE_0), Transform::IDENTITY);
        assert_eq!(Transform::from_drm_rotation(0), Transform::IDENTITY);
        assert_eq!(
            Transform::from_drm_rotation(ROTATE_270 | REFLECT_X),
            Transform::new(3, true)
        );
        assert_eq!(
            Transform::from_drm_rotation(ROTATE_0 | REFLECT_Y),
            Transform::new(2, true)
        );
    }

    #[test]
    fn rotate_90_is_counter_clockwise() {
        let t = Transform::from_drm_rotation(ROTATE_90);
        // The top-right corner lands top-left, the top-left corner bottom-left.
        assert!(close(apply(t.affine(), (1.0, 0.0)), (0.0, 0.0)));
        assert!(close(apply(t.affine(), (0.0, 0.0)), (0.0, 1.0)));
    }

    #[test]
    fn panel_orientation_matches_drm_client_rotation() {
        assert_eq!(Transform::from_panel_orientation(0), Transform::IDENTITY);
        assert_eq!(
            Transform::from_panel_orientation(1),
            Transform::from_drm_rotation(ROTATE_180)
        );
        assert_eq!(
            Transform::from_panel_orientation(2),
            Transform::from_drm_rotation(ROTATE_90)
        );
        assert_eq!(
            Transform::from_panel_orientation(3),
            Transform::from_drm_rotation(ROTATE_270)
        );
        assert_eq!(
            Transform::from_panel_orientation(u64::MAX),
            Transform::IDENTITY
        );
    }

    #[test]
    fn composition_and_inverse_follow_the_affine_maps() {
        for a in Transform::all() {
            assert_eq!(a.then(a.inverse()), Transform::IDENTITY, "{a:?}");
            assert_eq!(a.inverse().then(a), Transform::IDENTITY, "{a:?}");
            for b in Transform::all() {
                assert_eq!(
                    a.then(b).affine(),
                    compose(b.affine(), a.affine()),
                    "{a:?} then {b:?}"
                );
            }
        }
        assert_eq!(
            Transform::all()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            8
        );
    }

    #[test]
    fn integer_maps_follow_the_affine_map() {
        let size = (6, 4);
        for t in Transform::all() {
            let (w, h) = t.size(size.0, size.1);
            for p in [(0, 0), (6, 0), (0, 4), (6, 4), (2, 3), (-3, 5)] {
                let n = apply(t.affine(), (p.0 as f64 / 6.0, p.1 as f64 / 4.0));
                let want = (
                    (n.0 * f64::from(w)).round() as i64,
                    (n.1 * f64::from(h)).round() as i64,
                );
                assert_eq!(t.map_point(p, size), want, "{t:?} at {p:?}");
            }
            for x in 0..6 {
                for y in 0..4 {
                    let (cx, cy) = t.map_point((2 * x + 1, 2 * y + 1), (12, 8));
                    let want = (((cx - 1) / 2) as u32, ((cy - 1) / 2) as u32);
                    assert_eq!(t.map_pixel((x as u32, y as u32), size), want, "{t:?}");
                }
            }
            let (x, y, rw, rh) = t.map_rect((1, -1, 2, 3), size);
            let corners = [(1, -1), (3, -1), (1, 2), (3, 2)].map(|c| t.map_point(c, size));
            assert_eq!(x, corners.iter().map(|c| c.0).min().unwrap(), "{t:?}");
            assert_eq!(y, corners.iter().map(|c| c.1).min().unwrap(), "{t:?}");
            assert_eq!((rw, rh), t.size(2, 3));
            let pixels: Vec<u32> = (0..24).collect();
            let out = t.map_pixels(&pixels, size);
            for (i, &v) in pixels.iter().enumerate() {
                let (x, y) = t.map_pixel((i as u32 % 6, i as u32 / 6), size);
                assert_eq!(out[(y * w + x) as usize], v, "{t:?}");
            }
            assert_eq!(t.inverse().map_pixels(&out, (w, h)), pixels, "{t:?}");
        }
    }

    #[test]
    fn sizes_swap_for_quarter_turns() {
        for t in Transform::all() {
            let expect = if t.quarter() % 2 == 1 {
                (1080, 1920)
            } else {
                (1920, 1080)
            };
            assert_eq!(t.size(1920, 1080), expect, "{t:?}");
        }
    }

    #[test]
    fn upright_takes_a_rotated_plane_as_is_and_undoes_a_turned_panel() {
        let r90 = Transform::from_drm_rotation(ROTATE_90);
        let left_up = Transform::from_panel_orientation(2);
        let right_up = Transform::from_panel_orientation(3);
        assert_eq!(
            upright(Transform::IDENTITY, Transform::IDENTITY),
            Transform::IDENTITY
        );
        assert_eq!(upright(r90, Transform::IDENTITY), Transform::IDENTITY);
        assert_eq!(upright(left_up, left_up), Transform::IDENTITY);
        assert_eq!(upright(Transform::IDENTITY, left_up), left_up.inverse());
        assert_eq!(upright(Transform::IDENTITY, right_up), r90);
        // A 1920x1080 mode on a plane turned a quarter scans out a 1080x1920 desktop.
        assert_eq!(
            upright_size((1920, 1080), r90, Transform::IDENTITY),
            (1080, 1920)
        );
        assert_eq!(
            upright_size(
                (1920, 1080),
                Transform::from_drm_rotation(ROTATE_180 | REFLECT_X),
                Transform::IDENTITY
            ),
            (1920, 1080)
        );
        // An 800x1280 panel mounted on its side shows a 1280x800 desktop either way.
        assert_eq!(
            upright_size((800, 1280), Transform::IDENTITY, right_up),
            (1280, 800)
        );
        assert_eq!(upright_size((800, 1280), right_up, right_up), (1280, 800));
        assert_eq!(
            upright_size((1920, 1080), Transform::IDENTITY, Transform::IDENTITY),
            (1920, 1080)
        );
    }
}
