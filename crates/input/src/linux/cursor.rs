//! Reads the pointer from the KMS cursor planes: position from the plane's `CRTC_X`/`CRTC_Y`, image
//! from its framebuffer (`GETFB2`, mapped and converted to RGBA), both turned into the picture as
//! streamed when the primary plane or the panel is rotated.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use libc::{_IOW, _IOWR};
use ws_proto::orientation::{Transform, upright};
use ws_proto::{CursorImage, CursorKind, MAX_CURSOR_SIDE};

use crate::cursor::{CursorLook, ImageIds, fit};

/// An unchanged framebuffer is read again this often.
const REREAD: Duration = Duration::from_secs(1);
/// How often cards and planes are looked for again while none shows a pointer.
const RESCAN: Duration = Duration::from_secs(5);
/// Largest framebuffer read.
const MAX_SIDE: u32 = 512;

const D: u32 = b'd' as u32;

#[repr(C)]
#[derive(Default)]
struct SetClientCap {
    capability: u64,
    value: u64,
}

#[repr(C)]
#[derive(Default)]
struct GemClose {
    handle: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Default)]
struct PrimeHandle {
    handle: u32,
    flags: u32,
    fd: i32,
}

#[repr(C)]
#[derive(Default)]
struct CardRes {
    fb_id_ptr: u64,
    crtc_id_ptr: u64,
    connector_id_ptr: u64,
    encoder_id_ptr: u64,
    count_fbs: u32,
    count_crtcs: u32,
    count_connectors: u32,
    count_encoders: u32,
    min_width: u32,
    max_width: u32,
    min_height: u32,
    max_height: u32,
}

#[repr(C)]
#[derive(Default)]
struct GetEncoder {
    encoder_id: u32,
    encoder_type: u32,
    crtc_id: u32,
    possible_crtcs: u32,
    possible_clones: u32,
}

#[repr(C)]
#[derive(Default)]
struct GetConnector {
    encoders_ptr: u64,
    modes_ptr: u64,
    props_ptr: u64,
    prop_values_ptr: u64,
    count_modes: u32,
    count_props: u32,
    count_encoders: u32,
    encoder_id: u32,
    connector_id: u32,
    connector_type: u32,
    connector_type_id: u32,
    connection: u32,
    mm_width: u32,
    mm_height: u32,
    subpixel: u32,
    pad: u32,
}

/// `struct drm_mode_modeinfo`, only ever written by the kernel.
#[repr(C)]
struct ModeInfo([u8; 68]);

impl ModeInfo {
    /// `hdisplay` and `vdisplay`.
    fn size(&self) -> (u32, u32) {
        let at = |i: usize| u32::from(u16::from_ne_bytes([self.0[i], self.0[i + 1]]));
        (at(4), at(14))
    }
}

#[repr(C)]
struct ModeCrtc {
    set_connectors_ptr: u64,
    count_connectors: u32,
    crtc_id: u32,
    fb_id: u32,
    x: u32,
    y: u32,
    gamma_size: u32,
    mode_valid: u32,
    mode: ModeInfo,
}

#[repr(C)]
#[derive(Default)]
struct GetPlaneRes {
    plane_id_ptr: u64,
    count_planes: u32,
}

#[repr(C)]
#[derive(Default)]
struct GetPlane {
    plane_id: u32,
    crtc_id: u32,
    fb_id: u32,
    possible_crtcs: u32,
    gamma_size: u32,
    count_format_types: u32,
    format_type_ptr: u64,
}

#[repr(C)]
#[derive(Default)]
struct ObjGetProperties {
    props_ptr: u64,
    prop_values_ptr: u64,
    count_props: u32,
    obj_id: u32,
    obj_type: u32,
}

#[repr(C)]
struct GetProperty {
    values_ptr: u64,
    enum_blob_ptr: u64,
    prop_id: u32,
    flags: u32,
    name: [u8; 32],
    count_values: u32,
    count_enum_blobs: u32,
}

#[repr(C)]
#[derive(Default)]
struct FbCmd2 {
    fb_id: u32,
    width: u32,
    height: u32,
    pixel_format: u32,
    flags: u32,
    handles: [u32; 4],
    pitches: [u32; 4],
    offsets: [u32; 4],
    modifier: [u64; 4],
}

#[repr(C)]
#[derive(Default)]
struct MapDumb {
    handle: u32,
    pad: u32,
    offset: u64,
}

const SET_CLIENT_CAP: libc::Ioctl = _IOW::<SetClientCap>(D, 0x0d);
const GEM_CLOSE: libc::Ioctl = _IOW::<GemClose>(D, 0x09);
const PRIME_HANDLE_TO_FD: libc::Ioctl = _IOWR::<PrimeHandle>(D, 0x2d);
const MODE_GETRESOURCES: libc::Ioctl = _IOWR::<CardRes>(D, 0xa0);
const MODE_GETCRTC: libc::Ioctl = _IOWR::<ModeCrtc>(D, 0xa1);
const MODE_GETENCODER: libc::Ioctl = _IOWR::<GetEncoder>(D, 0xa6);
const MODE_GETCONNECTOR: libc::Ioctl = _IOWR::<GetConnector>(D, 0xa7);
const MODE_GETPROPERTY: libc::Ioctl = _IOWR::<GetProperty>(D, 0xaa);
const MODE_MAP_DUMB: libc::Ioctl = _IOWR::<MapDumb>(D, 0xb3);
const MODE_GETPLANERESOURCES: libc::Ioctl = _IOWR::<GetPlaneRes>(D, 0xb5);
const MODE_GETPLANE: libc::Ioctl = _IOWR::<GetPlane>(D, 0xb6);
const MODE_OBJ_GETPROPERTIES: libc::Ioctl = _IOWR::<ObjGetProperties>(D, 0xb9);
const MODE_GETFB2: libc::Ioctl = _IOWR::<FbCmd2>(D, 0xce);

const CAP_UNIVERSAL_PLANES: u64 = 2;
const CAP_ATOMIC: u64 = 3;
const OBJECT_PLANE: u32 = 0xeeee_eeee;
const OBJECT_CONNECTOR: u32 = 0xc0c0_c0c0;
const PLANE_TYPE_PRIMARY: u64 = 1;
const PLANE_TYPE_CURSOR: u64 = 2;
const CONNECTED: u32 = 1;

const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*s)
}
const ARGB8888: u32 = fourcc(b"AR24");
const XRGB8888: u32 = fourcc(b"XR24");
const ABGR8888: u32 = fourcc(b"AB24");
const MOD_LINEAR: u64 = 0;
const MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

const CONNECTOR_TYPES: [&str; 21] = [
    "Unknown",
    "VGA",
    "DVI-I",
    "DVI-D",
    "DVI-A",
    "Composite",
    "SVIDEO",
    "LVDS",
    "Component",
    "DIN",
    "DP",
    "HDMI-A",
    "HDMI-B",
    "TV",
    "eDP",
    "Virtual",
    "DSI",
    "DPI",
    "Writeback",
    "SPI",
    "USB",
];

/// The connector name the kernel uses, e.g. `HDMI-A-1`.
pub fn connector_name(kind: u32, id: u32) -> String {
    let kind = CONNECTOR_TYPES
        .get(kind as usize)
        .copied()
        .unwrap_or("Unknown");
    format!("{kind}-{id}")
}

fn ioctl<T>(fd: RawFd, req: libc::Ioctl, arg: &mut T) -> io::Result<()> {
    // SAFETY: `arg` is the struct `req` expects, and any pointers in it point to live buffers
    // the size its counts say.
    let r = unsafe { libc::ioctl(fd, req, arg as *mut T) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// The ids a "count, then fill" ioctl lists.
fn ids(
    fd: RawFd,
    req: libc::Ioctl,
    get: impl Fn(&mut CardRes) -> (&mut u64, &mut u32),
) -> Vec<u32> {
    let mut res = CardRes::default();
    if ioctl(fd, req, &mut res).is_err() {
        return Vec::new();
    }
    let mut out = vec![0u32; *get(&mut res).1 as usize];
    let mut res2 = CardRes::default();
    {
        let (ptr, count) = get(&mut res2);
        *ptr = out.as_mut_ptr() as u64;
        *count = out.len() as u32;
    }
    if ioctl(fd, req, &mut res2).is_err() {
        return Vec::new();
    }
    out.truncate(*get(&mut res2).1 as usize);
    out
}

/// Property values of a mode object, by name.
fn obj_props(
    fd: RawFd,
    obj_id: u32,
    obj_type: u32,
    names: &mut HashMap<u32, String>,
) -> HashMap<String, u64> {
    let mut q = ObjGetProperties {
        obj_id,
        obj_type,
        ..Default::default()
    };
    if ioctl(fd, MODE_OBJ_GETPROPERTIES, &mut q).is_err() {
        return HashMap::new();
    }
    let n = q.count_props as usize;
    let mut props = vec![0u32; n];
    let mut values = vec![0u64; n];
    q.props_ptr = props.as_mut_ptr() as u64;
    q.prop_values_ptr = values.as_mut_ptr() as u64;
    if ioctl(fd, MODE_OBJ_GETPROPERTIES, &mut q).is_err() {
        return HashMap::new();
    }
    let n = (q.count_props as usize).min(n);
    let mut out = HashMap::new();
    for (&id, &value) in props[..n].iter().zip(&values[..n]) {
        let name = names.entry(id).or_insert_with(|| {
            let mut p = GetProperty {
                values_ptr: 0,
                enum_blob_ptr: 0,
                prop_id: id,
                flags: 0,
                name: [0; 32],
                count_values: 0,
                count_enum_blobs: 0,
            };
            if ioctl(fd, MODE_GETPROPERTY, &mut p).is_err() {
                return String::new();
            }
            let end = p.name.iter().position(|&b| b == 0).unwrap_or(32);
            String::from_utf8_lossy(&p.name[..end]).into_owned()
        });
        out.insert(name.clone(), value);
    }
    out
}

/// One cursor plane.
struct Plane {
    id: u32,
    /// The driver reports the hotspot (`HOTSPOT_X`/`HOTSPOT_Y`), as virtual GPUs do.
    hotspot: bool,
}

/// A cursor plane in use: its framebuffer, CRTC, top-left on that CRTC and hotspot.
struct Shown {
    fb: u32,
    crtc: u32,
    x: i32,
    y: i32,
    hot: (u32, u32),
    /// The driver reported `hot`.
    reported: bool,
    /// The plane's `rotation`.
    rotation: Transform,
    /// The plane's `CRTC_W`/`CRTC_H`.
    size: Option<(u32, u32)>,
}

/// A connected output: its connector's name and how its panel is mounted.
struct Output {
    connector: Arc<str>,
    panel: Transform,
}

/// What turns a CRTC's pixels into the picture as streamed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct View {
    /// The mode size.
    mode: Option<(u32, u32)>,
    /// The primary plane's `rotation`.
    plane: Transform,
    panel: Transform,
}

impl View {
    /// From CRTC pixels to the picture as streamed.
    fn to_upright(self) -> Transform {
        match self.mode {
            Some(_) => self.plane.inverse().then(upright(self.plane, self.panel)),
            None => Transform::IDENTITY,
        }
    }

    /// The cursor plane's top-left in the picture as streamed, and the transform from its
    /// framebuffer to the sprite as streamed; `rotation` and `size` are the plane's.
    fn place(
        self,
        rotation: Transform,
        at: (i32, i32),
        size: (u32, u32),
    ) -> ((i64, i64), Transform) {
        let to_upright = self.to_upright();
        let at = (i64::from(at.0), i64::from(at.1));
        let top_left = match self.mode {
            Some(mode) => {
                let (x, y, _, _) = to_upright.map_rect((at.0, at.1, size.0, size.1), mode);
                (x, y)
            }
            None => at,
        };
        (top_left, rotation.then(to_upright))
    }
}

struct Card {
    file: File,
    planes: Vec<Plane>,
    /// Primary plane ids.
    primaries: Vec<u32>,
    /// Each CRTC driving a connected output.
    crtcs: HashMap<u32, Output>,
    /// Each CRTC's view and when it was read.
    views: HashMap<u32, (Instant, View)>,
    /// Property names by id.
    names: HashMap<u32, String>,
}

impl Card {
    fn open(path: &str) -> Option<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC)
            .open(path)
            .ok()?;
        let fd = file.as_raw_fd();
        for cap in [CAP_UNIVERSAL_PLANES, CAP_ATOMIC] {
            let mut c = SetClientCap {
                capability: cap,
                value: 1,
            };
            ioctl(fd, SET_CLIENT_CAP, &mut c).ok()?;
        }
        let mut res = GetPlaneRes::default();
        ioctl(fd, MODE_GETPLANERESOURCES, &mut res).ok()?;
        let mut plane_ids = vec![0u32; res.count_planes as usize];
        res.plane_id_ptr = plane_ids.as_mut_ptr() as u64;
        ioctl(fd, MODE_GETPLANERESOURCES, &mut res).ok()?;
        plane_ids.truncate(res.count_planes as usize);
        let mut names = HashMap::new();
        let mut planes = Vec::new();
        let mut primaries = Vec::new();
        for id in plane_ids {
            let props = obj_props(fd, id, OBJECT_PLANE, &mut names);
            let positioned = props.contains_key("CRTC_X") && props.contains_key("CRTC_Y");
            match props.get("type") {
                Some(&PLANE_TYPE_CURSOR) if positioned => planes.push(Plane {
                    id,
                    hotspot: props.contains_key("HOTSPOT_X") && props.contains_key("HOTSPOT_Y"),
                }),
                Some(&PLANE_TYPE_PRIMARY) => primaries.push(id),
                _ => {}
            }
        }
        if planes.is_empty() {
            return None;
        }
        let crtcs = crtc_outputs(fd, &mut names);
        Some(Self {
            file,
            planes,
            primaries,
            crtcs,
            views: HashMap::new(),
            names,
        })
    }

    /// The view of `crtc`, read again once older than [`REREAD`].
    fn view(&mut self, crtc: u32) -> View {
        if let Some((at, view)) = self.views.get(&crtc) {
            if at.elapsed() < REREAD {
                return *view;
            }
        }
        let fd = self.file.as_raw_fd();
        let mut c = ModeCrtc {
            set_connectors_ptr: 0,
            count_connectors: 0,
            crtc_id: crtc,
            fb_id: 0,
            x: 0,
            y: 0,
            gamma_size: 0,
            mode_valid: 0,
            mode: ModeInfo([0; 68]),
        };
        let mode =
            (ioctl(fd, MODE_GETCRTC, &mut c).is_ok() && c.mode_valid != 0).then(|| c.mode.size());
        let primary = self.primaries.iter().copied().find(|&id| {
            let mut p = GetPlane {
                plane_id: id,
                ..Default::default()
            };
            ioctl(fd, MODE_GETPLANE, &mut p).is_ok() && p.crtc_id == crtc
        });
        let plane = primary
            .and_then(|id| {
                obj_props(fd, id, OBJECT_PLANE, &mut self.names)
                    .get("rotation")
                    .copied()
            })
            .map_or(Transform::IDENTITY, Transform::from_drm_rotation);
        let view = View {
            mode,
            plane,
            panel: self
                .crtcs
                .get(&crtc)
                .map_or(Transform::IDENTITY, |o| o.panel),
        };
        self.views.insert(crtc, (Instant::now(), view));
        view
    }

    /// The first cursor plane in use.
    fn shown(&mut self) -> Option<Shown> {
        let fd = self.file.as_raw_fd();
        for plane in &self.planes {
            let mut p = GetPlane {
                plane_id: plane.id,
                ..Default::default()
            };
            if ioctl(fd, MODE_GETPLANE, &mut p).is_err() || p.fb_id == 0 || p.crtc_id == 0 {
                continue;
            }
            let props = obj_props(fd, plane.id, OBJECT_PLANE, &mut self.names);
            let signed = |name: &str| props.get(name).map(|&v| v as i64 as i32);
            let (Some(x), Some(y)) = (signed("CRTC_X"), signed("CRTC_Y")) else {
                continue;
            };
            let reported = plane.hotspot;
            let hot = if reported {
                (
                    signed("HOTSPOT_X").unwrap_or(0).max(0) as u32,
                    signed("HOTSPOT_Y").unwrap_or(0).max(0) as u32,
                )
            } else {
                (0, 0)
            };
            let size = match (props.get("CRTC_W"), props.get("CRTC_H")) {
                (Some(&w), Some(&h)) if w > 0 && h > 0 => Some((w as u32, h as u32)),
                _ => None,
            };
            return Some(Shown {
                fb: p.fb_id,
                crtc: p.crtc_id,
                x,
                y,
                hot,
                reported,
                rotation: props
                    .get("rotation")
                    .map_or(Transform::IDENTITY, |&v| Transform::from_drm_rotation(v)),
                size,
            });
        }
        None
    }
}

fn crtc_outputs(fd: RawFd, names: &mut HashMap<u32, String>) -> HashMap<u32, Output> {
    let mut out = HashMap::new();
    let connectors = ids(fd, MODE_GETRESOURCES, |r| {
        (&mut r.connector_id_ptr, &mut r.count_connectors)
    });
    for id in connectors {
        let mut mode = ModeInfo([0; 68]);
        let mut c = GetConnector {
            connector_id: id,
            count_modes: 1,
            modes_ptr: &mut mode as *mut ModeInfo as u64,
            ..Default::default()
        };
        if ioctl(fd, MODE_GETCONNECTOR, &mut c).is_err() || c.connection != CONNECTED {
            continue;
        }
        let mut e = GetEncoder {
            encoder_id: c.encoder_id,
            ..Default::default()
        };
        if c.encoder_id == 0 || ioctl(fd, MODE_GETENCODER, &mut e).is_err() || e.crtc_id == 0 {
            continue;
        }
        let panel = obj_props(fd, id, OBJECT_CONNECTOR, names)
            .get("panel orientation")
            .map_or(Transform::IDENTITY, |&v| {
                Transform::from_panel_orientation(v)
            });
        out.insert(
            e.crtc_id,
            Output {
                connector: connector_name(c.connector_type, c.connector_type_id).into(),
                panel,
            },
        );
    }
    out
}

/// The pointer as the cursor plane shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct KmsCursor {
    pub look: CursorLook,
    /// The hotspot, in pixels of the output `connector` names.
    pub x: i32,
    pub y: i32,
    pub connector: Arc<str>,
}

/// A cursor framebuffer as read: its id, the transform it was read with, when, what it looked like
/// and its size as streamed.
struct LastRead {
    fb: u32,
    sprite: Transform,
    at: Instant,
    look: CursorLook,
    size: (u32, u32),
}

/// Reads the pointer from whichever card shows one; polled from one thread.
pub struct KmsCursorReader {
    cards: Vec<Card>,
    scanned: Option<Instant>,
    ids: ImageIds,
    /// The framebuffer last read.
    last: Option<LastRead>,
    /// A cursor plane was seen in use.
    seen: bool,
    /// Hotspots the driver does not report, learned from where the pointer was moved to.
    hotspots: Hotspots,
    /// The last look with a learned hotspot: the image id it came from, the hotspot and the look.
    hot_look: Option<(u32, (u32, u32), CursorLook)>,
}

impl Default for KmsCursorReader {
    fn default() -> Self {
        Self::new()
    }
}

impl KmsCursorReader {
    pub fn new() -> Self {
        Self {
            cards: Vec::new(),
            scanned: None,
            ids: ImageIds::default(),
            last: None,
            seen: false,
            hotspots: Hotspots::default(),
            hot_look: None,
        }
    }

    /// The pointer now; `None` while no cursor plane has been seen in use. `origin` gives the
    /// desktop position of the output a connector names.
    pub fn read(&mut self, origin: impl Fn(&str) -> (i32, i32)) -> Option<KmsCursor> {
        let rescan = self.cards.is_empty() || !self.seen;
        if rescan && self.scanned.is_none_or(|t| t.elapsed() >= RESCAN) {
            self.scanned = Some(Instant::now());
            self.cards = (0..16)
                .filter_map(|i| Card::open(&format!("/dev/dri/card{i}")))
                .collect();
        }
        for i in 0..self.cards.len() {
            let card = &mut self.cards[i];
            let Some(Shown {
                fb,
                crtc,
                x,
                y,
                hot,
                reported,
                rotation,
                size: plane_size,
            }) = card.shown()
            else {
                continue;
            };
            let view = card.view(crtc);
            let sprite = rotation.then(view.to_upright());
            let fd = card.file.as_raw_fd();
            let connector = card
                .crtcs
                .get(&crtc)
                .map_or_else(|| "".into(), |o| o.connector.clone());
            self.seen = true;
            let (look, size) = match &self.last {
                Some(last)
                    if last.fb == fb && last.sprite == sprite && last.at.elapsed() < REREAD =>
                {
                    (last.look.clone(), last.size)
                }
                _ => {
                    let read = read_image(fd, fb, hot, sprite);
                    let size = read.as_ref().map_or((0, 0), |(_, size)| *size);
                    let image = read.map(|(i, _)| self.ids.get(i));
                    let kind = if image.is_some() {
                        CursorKind::Custom
                    } else {
                        CursorKind::Default
                    };
                    let look = CursorLook { kind, image };
                    self.last = Some(LastRead {
                        fb,
                        sprite,
                        at: Instant::now(),
                        look: look.clone(),
                        size,
                    });
                    (look, size)
                }
            };
            let fb_size = sprite.size(size.0, size.1);
            let plane_size = plane_size.unwrap_or_else(|| rotation.size(fb_size.0, fb_size.1));
            let ((ux, uy), _) = view.place(rotation, (x, y), plane_size);
            let clamp = |v: i64| v.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
            let (ux, uy) = (clamp(ux), clamp(uy));
            let (look, hot) = if reported {
                let hot = if fb_size.0 > 0 && fb_size.1 > 0 {
                    sprite.map_pixel(hot, fb_size)
                } else {
                    hot
                };
                (look, hot)
            } else {
                let (ox, oy) = origin(&connector);
                self.learned(look, size, (ux.saturating_add(ox), uy.saturating_add(oy)))
            };
            return Some(KmsCursor {
                look,
                x: ux.saturating_add(hot.0 as i32),
                y: uy.saturating_add(hot.1 as i32),
                connector,
            });
        }
        self.seen.then(|| KmsCursor {
            look: CursorLook::HIDDEN,
            x: 0,
            y: 0,
            connector: "".into(),
        })
    }
}

impl KmsCursorReader {
    /// `look` with the hotspot learned for its image, and that hotspot in sprite pixels;
    /// `top_left` is the sprite's desktop position and `size` its size, both as streamed.
    fn learned(
        &mut self,
        look: CursorLook,
        size: (u32, u32),
        top_left: (i32, i32),
    ) -> (CursorLook, (u32, u32)) {
        let Some((id, image)) = look.image.clone() else {
            return (look, (0, 0));
        };
        self.hotspots
            .observe(id, size, top_left, super::last_absolute(), Instant::now());
        let Some(hot) = self.hotspots.get(id) else {
            return (look, (0, 0));
        };
        if let Some((from, h, cached)) = &self.hot_look {
            if *from == id && *h == hot {
                return (cached.clone(), hot);
            }
        }
        let scale = |v: u32, image_side: u32, fb_side: u32| {
            (u64::from(v) * u64::from(image_side) / u64::from(fb_side.max(1))) as u32
        };
        let with_hot = CursorImage {
            hot_x: scale(hot.0, image.width, size.0).min(image.width.saturating_sub(1)),
            hot_y: scale(hot.1, image.height, size.1).min(image.height.saturating_sub(1)),
            ..(*image).clone()
        };
        let out = CursorLook {
            kind: look.kind,
            image: Some(self.ids.get(with_hot)),
        };
        self.hot_look = Some((id, hot, out.clone()));
        (out, hot)
    }
}

/// How long the pointer must rest after a move before the cursor plane is sure to show it there.
const REST: Duration = Duration::from_millis(40);
/// Learned hotspots kept; past this the table starts over.
const MAX_HOTSPOTS: usize = 256;

/// Hotspots learned per cursor image: once the absolute pointer has rested at a desktop pixel,
/// the hotspot is that pixel less the cursor plane's top-left.
#[derive(Default)]
struct Hotspots {
    by_image: HashMap<u32, (u32, u32)>,
    /// The move and image last learned from.
    used: Option<(Instant, u32)>,
}

impl Hotspots {
    fn observe(
        &mut self,
        image: u32,
        size: (u32, u32),
        top_left: (i32, i32),
        moved: Option<((i32, i32), Instant)>,
        now: Instant,
    ) {
        let Some((pointer, at)) = moved else {
            return;
        };
        if now.saturating_duration_since(at) < REST || self.used == Some((at, image)) {
            return;
        }
        self.used = Some((at, image));
        let (hx, hy) = (
            i64::from(pointer.0) - i64::from(top_left.0),
            i64::from(pointer.1) - i64::from(top_left.1),
        );
        if hx < 0 || hy < 0 || hx >= i64::from(size.0) || hy >= i64::from(size.1) {
            return;
        }
        if self.by_image.len() >= MAX_HOTSPOTS {
            self.by_image.clear();
        }
        self.by_image.insert(image, (hx as u32, hy as u32));
    }

    fn get(&self, image: u32) -> Option<(u32, u32)> {
        self.by_image.get(&image).copied()
    }
}

/// Closes GEM handles on drop.
struct Handles<'a>(RawFd, &'a [u32]);

impl Drop for Handles<'_> {
    fn drop(&mut self) {
        let mut done = Vec::new();
        for &h in self.1 {
            if h != 0 && !done.contains(&h) {
                done.push(h);
                let _ = ioctl(self.0, GEM_CLOSE, &mut GemClose { handle: h, pad: 0 });
            }
        }
    }
}

/// A read-only mapping, unmapped on drop.
struct Mapping {
    ptr: *mut libc::c_void,
    len: usize,
}

impl Mapping {
    fn new(fd: RawFd, len: usize, offset: i64) -> io::Result<Self> {
        // SAFETY: maps `len` bytes of `fd` read-only; unmapped on drop.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                offset,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { ptr, len })
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: the mapping is `len` readable bytes for as long as `self` lives.
        unsafe { std::slice::from_raw_parts(self.ptr.cast(), self.len) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: unmaps exactly what `new` mapped.
        unsafe { libc::munmap(self.ptr, self.len) };
    }
}

/// Maps a GEM handle: as a dumb buffer, else through a dma-buf.
fn map(fd: RawFd, handle: u32, len: usize) -> io::Result<Mapping> {
    let mut m = MapDumb {
        handle,
        ..Default::default()
    };
    if ioctl(fd, MODE_MAP_DUMB, &mut m).is_ok() {
        if let Ok(map) = Mapping::new(fd, len, m.offset as i64) {
            return Ok(map);
        }
    }
    let mut p = PrimeHandle {
        handle,
        flags: libc::O_CLOEXEC as u32,
        fd: -1,
    };
    ioctl(fd, PRIME_HANDLE_TO_FD, &mut p)?;
    // SAFETY: a fresh dma-buf fd we own.
    let buf = unsafe { OwnedFd::from_raw_fd(p.fd) };
    Mapping::new(buf.as_raw_fd(), len, 0)
}

/// The cursor framebuffer turned by `sprite`, with `hot` (framebuffer pixels), and its size once
/// turned.
fn read_image(
    fd: RawFd,
    fb_id: u32,
    hot: (u32, u32),
    sprite: Transform,
) -> Option<(CursorImage, (u32, u32))> {
    let mut fb = FbCmd2 {
        fb_id,
        ..Default::default()
    };
    ioctl(fd, MODE_GETFB2, &mut fb).ok()?;
    let _close = Handles(fd, &fb.handles);
    let (w, h, pitch) = (fb.width, fb.height, fb.pitches[0]);
    let linear = fb.flags & 2 == 0 || matches!(fb.modifier[0], MOD_LINEAR | MOD_INVALID);
    if fb.handles[0] == 0 || !linear || w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE {
        return None;
    }
    if !matches!(fb.pixel_format, ARGB8888 | XRGB8888 | ABGR8888) || pitch < w * 4 {
        return None;
    }
    let offset = fb.offsets[0] as usize;
    let len = offset + pitch as usize * h as usize;
    let mapping = map(fd, fb.handles[0], len).ok()?;
    let rgba = rgba_from_premultiplied(
        &mapping.bytes()[offset..],
        w as usize,
        h as usize,
        pitch as usize,
        fb.pixel_format,
    );
    let image = turn(
        CursorImage {
            width: w,
            height: h,
            hot_x: hot.0.min(w - 1),
            hot_y: hot.1.min(h - 1),
            rgba,
        },
        sprite,
    );
    let size = (image.width, image.height);
    Some((fit(image, MAX_CURSOR_SIDE), size))
}

/// `image` and its hotspot turned by `t`.
fn turn(image: CursorImage, t: Transform) -> CursorImage {
    if t.is_identity() {
        return image;
    }
    let size = (image.width, image.height);
    let pixels: Vec<[u8; 4]> = image
        .rgba
        .chunks_exact(4)
        .map(|p| [p[0], p[1], p[2], p[3]])
        .collect();
    let (width, height) = t.size(size.0, size.1);
    let (hot_x, hot_y) = t.map_pixel((image.hot_x, image.hot_y), size);
    CursorImage {
        width,
        height,
        hot_x,
        hot_y,
        rgba: t.map_pixels(&pixels, size).concat(),
    }
}

/// Straight-alpha RGBA from premultiplied 32-bit little-endian ARGB, XRGB or ABGR rows.
pub fn rgba_from_premultiplied(
    src: &[u8],
    width: usize,
    height: usize,
    pitch: usize,
    format: u32,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(width * height * 4);
    for row in src.chunks(pitch).take(height) {
        for p in row[..width * 4].chunks_exact(4) {
            let (r, g, b, a) = match format {
                ABGR8888 => (p[0], p[1], p[2], p[3]),
                XRGB8888 => (p[2], p[1], p[0], 255),
                _ => (p[2], p[1], p[0], p[3]),
            };
            let un = |c: u8| -> u8 {
                match a {
                    0 => 0,
                    255 => c,
                    a => ((u32::from(c) * 255 + u32::from(a) / 2) / u32::from(a)).min(255) as u8,
                }
            };
            out.extend_from_slice(&[un(r), un(g), un(b), a]);
        }
    }
    out
}

const _: () = assert!(size_of::<GetConnector>() == 80);
const _: () = assert!(size_of::<FbCmd2>() == 104);
const _: () = assert!(size_of::<GetProperty>() == 64);
const _: () = assert!(size_of::<CardRes>() == 64);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mouse::DisplayRect;

    #[test]
    fn ioctl_numbers_match_the_kernel_headers() {
        assert_eq!(MODE_GETFB2 as u32, 0xc06864ce);
        assert_eq!(MODE_GETPLANE as u32, 0xc02064b6);
        assert_eq!(MODE_OBJ_GETPROPERTIES as u32, 0xc02064b9);
        assert_eq!(MODE_GETCONNECTOR as u32, 0xc05064a7);
        assert_eq!(MODE_MAP_DUMB as u32, 0xc01064b3);
        assert_eq!(SET_CLIENT_CAP as u32, 0x4010640d);
    }

    #[test]
    fn premultiplied_argb_becomes_straight_rgba() {
        // B, G, R, A in memory.
        let src = [0, 0, 128, 128, 10, 20, 30, 255, 0, 0, 0, 0, 0xff, 0xff];
        let out = rgba_from_premultiplied(&src, 3, 1, 14, ARGB8888);
        assert_eq!(&out[0..4], &[255, 0, 0, 128]);
        assert_eq!(&out[4..8], &[30, 20, 10, 255]);
        assert_eq!(&out[8..12], &[0, 0, 0, 0]);
    }

    #[test]
    fn rows_honour_the_pitch_and_xrgb_is_opaque() {
        let src = [1, 2, 3, 0, 9, 9, 4, 5, 6, 0, 9, 9];
        let out = rgba_from_premultiplied(&src, 1, 2, 6, XRGB8888);
        assert_eq!(out, [3, 2, 1, 255, 6, 5, 4, 255]);
        let abgr = rgba_from_premultiplied(&[7, 8, 9, 255], 1, 1, 4, ABGR8888);
        assert_eq!(abgr, [7, 8, 9, 255]);
    }

    #[test]
    fn connector_names_follow_the_kernel() {
        assert_eq!(connector_name(11, 1), "HDMI-A-1");
        assert_eq!(connector_name(10, 2), "DP-2");
        assert_eq!(connector_name(14, 1), "eDP-1");
        assert_eq!(connector_name(99, 1), "Unknown-1");
    }

    #[test]
    fn hotspots_are_learned_once_the_pointer_rests() {
        let t0 = Instant::now();
        let moved = Some(((105, 204), t0));
        let mut h = Hotspots::default();
        h.observe(
            1,
            (64, 64),
            (100, 200),
            moved,
            t0 + Duration::from_millis(10),
        );
        assert_eq!(h.get(1), None);
        h.observe(1, (64, 64), (100, 200), moved, t0 + REST);
        assert_eq!(h.get(1), Some((5, 4)));
        // A later read of the same move and image does not learn again.
        h.observe(1, (64, 64), (90, 190), moved, t0 + REST * 2);
        assert_eq!(h.get(1), Some((5, 4)));
        // Another image under the resting pointer is learned from the same move.
        h.observe(2, (64, 64), (89, 188), moved, t0 + REST * 2);
        assert_eq!(h.get(2), Some((16, 16)));
    }

    #[test]
    fn a_pointer_outside_the_cursor_image_teaches_nothing() {
        let t0 = Instant::now();
        let mut h = Hotspots::default();
        let late = t0 + REST;
        h.observe(1, (64, 64), (100, 200), Some(((99, 204), t0)), late);
        h.observe(
            1,
            (64, 64),
            (100, 200),
            Some(((164, 204), t0 + REST)),
            late + REST,
        );
        h.observe(1, (64, 64), (100, 200), None, late);
        assert_eq!(h.get(1), None);
        h.observe(
            1,
            (64, 64),
            (100, 200),
            Some(((163, 263), late + Duration::from_millis(1))),
            late + REST * 2,
        );
        assert_eq!(h.get(1), Some((63, 63)));
    }

    #[test]
    fn desktop_pixels_span_the_display() {
        let d = DisplayRect {
            left: 1920,
            top: 0,
            width: 2560,
            height: 1440,
        };
        assert_eq!(super::super::desktop_pixel(0, 0, d), (1920, 0));
        assert_eq!(
            super::super::desktop_pixel(65535, 65535, d),
            (1920 + 2559, 1439)
        );
    }

    /// A 3x2 sprite with distinct pixels, its hotspot on the right edge.
    fn sprite() -> CursorImage {
        CursorImage {
            width: 3,
            height: 2,
            hot_x: 2,
            hot_y: 1,
            rgba: (0..24).collect(),
        }
    }

    #[test]
    fn the_cursor_lands_where_the_streamed_picture_shows_it() {
        let streamed = (640, 480);
        let image = sprite();
        let at = (100i64, 50i64);
        let pointer = (at.0 + 2, at.1 + 1);
        // `to_panel` takes the streamed picture to the CRTC's pixels.
        let check =
            |view: View, to_panel: Transform, rotation: Transform, fb: &CursorImage, case: &str| {
                let (x, y, w, h) = to_panel.map_rect((at.0, at.1, 3, 2), streamed);
                let ((ux, uy), t) = view.place(rotation, (x as i32, y as i32), (w, h));
                assert_eq!((ux, uy), at, "{case}: top-left");
                assert_eq!(turn(fb.clone(), t), image, "{case}: sprite and hotspot");
                let hot = t.map_pixel((fb.hot_x, fb.hot_y), (fb.width, fb.height));
                assert_eq!(
                    (ux + i64::from(hot.0), uy + i64::from(hot.1)),
                    pointer,
                    "{case}: reported hotspot"
                );
                let t0 = Instant::now();
                let mut learned = Hotspots::default();
                let size = t.size(fb.width, fb.height);
                learned.observe(
                    1,
                    size,
                    (ux as i32 + 1920, uy as i32),
                    Some(((pointer.0 as i32 + 1920, pointer.1 as i32), t0)),
                    t0 + REST,
                );
                assert_eq!(learned.get(1), Some((2, 1)), "{case}: learned hotspot");
            };
        for t in Transform::all() {
            let mode = Some(t.size(streamed.0, streamed.1));
            let plane = View {
                mode,
                plane: t,
                panel: Transform::IDENTITY,
            };
            check(plane, t, t, &image, &format!("{t:?} on both planes"));
            check(
                plane,
                t,
                Transform::IDENTITY,
                &turn(image.clone(), t),
                &format!("{t:?} on the primary plane, sprite turned in software"),
            );
            let panel = View {
                mode,
                plane: Transform::IDENTITY,
                panel: t,
            };
            check(
                panel,
                t,
                Transform::IDENTITY,
                &turn(image.clone(), t),
                &format!("{t:?} panel"),
            );
        }
        let unknown = View {
            mode: None,
            plane: Transform::new(1, false),
            panel: Transform::IDENTITY,
        };
        assert_eq!(
            unknown.place(Transform::IDENTITY, (7, 8), (64, 64)),
            ((7, 8), Transform::IDENTITY)
        );
    }

    #[test]
    fn turned_sprites_move_every_pixel() {
        let image = sprite();
        let r90 = turn(image.clone(), Transform::new(1, false));
        assert_eq!((r90.width, r90.height), (2, 3));
        // Counter-clockwise: the top-right pixel lands top-left, the hotspot bottom-right.
        assert_eq!(&r90.rgba[0..4], &image.rgba[8..12]);
        assert_eq!((r90.hot_x, r90.hot_y), (1, 0));
        for t in Transform::all() {
            let turned = turn(image.clone(), t);
            let mut sorted: Vec<&[u8]> = turned.rgba.chunks(4).collect();
            sorted.sort();
            assert_eq!(sorted, image.rgba.chunks(4).collect::<Vec<_>>(), "{t:?}");
            assert_eq!(turn(turned, t.inverse()), image, "{t:?}");
        }
    }

    #[test]
    fn mode_size_comes_from_the_mode_info() {
        let mut m = ModeInfo([0; 68]);
        m.0[4..6].copy_from_slice(&1920u16.to_ne_bytes());
        m.0[14..16].copy_from_slice(&1080u16.to_ne_bytes());
        assert_eq!(m.size(), (1920, 1080));
        assert_eq!(MODE_GETCRTC as u32, 0xc06864a1);
        assert_eq!(size_of::<ModeCrtc>(), 104);
    }
}
