//! Reads the pointer from the KMS cursor planes: position from the plane's `CRTC_X`/`CRTC_Y`, image
//! from its framebuffer (`GETFB2`, mapped and converted to RGBA).

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use libc::{_IOW, _IOWR};
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

/// Property values of a plane, by name.
fn plane_props(fd: RawFd, plane: u32, names: &mut HashMap<u32, String>) -> HashMap<String, u64> {
    let mut q = ObjGetProperties {
        obj_id: plane,
        obj_type: OBJECT_PLANE,
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
}

struct Card {
    file: File,
    planes: Vec<Plane>,
    /// Connector name of each CRTC driving a connected output.
    crtcs: HashMap<u32, Arc<str>>,
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
        let planes: Vec<Plane> = plane_ids
            .into_iter()
            .filter_map(|id| {
                let props = plane_props(fd, id, &mut names);
                let positioned = props.contains_key("CRTC_X") && props.contains_key("CRTC_Y");
                (props.get("type") == Some(&PLANE_TYPE_CURSOR) && positioned).then(|| Plane {
                    id,
                    hotspot: props.contains_key("HOTSPOT_X") && props.contains_key("HOTSPOT_Y"),
                })
            })
            .collect();
        if planes.is_empty() {
            return None;
        }
        let crtcs = crtc_connectors(fd);
        Some(Self {
            file,
            planes,
            crtcs,
            names,
        })
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
            let props = plane_props(fd, plane.id, &mut self.names);
            let signed = |name: &str| props.get(name).map(|&v| v as i64 as i32);
            let (Some(x), Some(y)) = (signed("CRTC_X"), signed("CRTC_Y")) else {
                continue;
            };
            let hot = if plane.hotspot {
                (
                    signed("HOTSPOT_X").unwrap_or(0).max(0) as u32,
                    signed("HOTSPOT_Y").unwrap_or(0).max(0) as u32,
                )
            } else {
                (0, 0)
            };
            return Some(Shown {
                fb: p.fb_id,
                crtc: p.crtc_id,
                x,
                y,
                hot,
            });
        }
        None
    }
}

fn crtc_connectors(fd: RawFd) -> HashMap<u32, Arc<str>> {
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
        out.insert(
            e.crtc_id,
            connector_name(c.connector_type, c.connector_type_id).into(),
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

/// Reads the pointer from whichever card shows one; polled from one thread.
pub struct KmsCursorReader {
    cards: Vec<Card>,
    scanned: Option<Instant>,
    ids: ImageIds,
    /// The framebuffer last read and what it looked like.
    last: Option<(u32, Instant, CursorLook)>,
    /// A cursor plane was seen in use.
    seen: bool,
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
        }
    }

    /// The pointer now; `None` while no cursor plane has been seen in use.
    pub fn read(&mut self) -> Option<KmsCursor> {
        let rescan = self.cards.is_empty() || !self.seen;
        if rescan && self.scanned.is_none_or(|t| t.elapsed() >= RESCAN) {
            self.scanned = Some(Instant::now());
            self.cards = (0..16)
                .filter_map(|i| Card::open(&format!("/dev/dri/card{i}")))
                .collect();
        }
        for card in &mut self.cards {
            let Some(Shown {
                fb,
                crtc,
                x,
                y,
                hot,
            }) = card.shown()
            else {
                continue;
            };
            self.seen = true;
            let look = match &self.last {
                Some((last, at, look)) if *last == fb && at.elapsed() < REREAD => look.clone(),
                _ => {
                    let image = read_image(card.file.as_raw_fd(), fb, hot).map(|i| self.ids.get(i));
                    let kind = if image.is_some() {
                        CursorKind::Custom
                    } else {
                        CursorKind::Default
                    };
                    let look = CursorLook { kind, image };
                    self.last = Some((fb, Instant::now(), look.clone()));
                    look
                }
            };
            return Some(KmsCursor {
                look,
                x: x.saturating_add(hot.0 as i32),
                y: y.saturating_add(hot.1 as i32),
                connector: card.crtcs.get(&crtc).cloned().unwrap_or_else(|| "".into()),
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

fn read_image(fd: RawFd, fb_id: u32, hot: (u32, u32)) -> Option<CursorImage> {
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
    Some(fit(
        CursorImage {
            width: w,
            height: h,
            hot_x: hot.0.min(w - 1),
            hot_y: hot.1.min(h - 1),
            rgba,
        },
        MAX_CURSOR_SIDE,
    ))
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
}
