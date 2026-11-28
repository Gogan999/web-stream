//! Grabs the framebuffer a CRTC scans out, as a DMA-BUF.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::rc::Rc;
use std::time::{Duration, Instant};

use super::display::Output;
use super::drm::{self, Card};
use crate::{CaptureError, Result};
use ws_proto::orientation::{self as transform, Transform};

/// A scanout buffer shared with the GPU, never touched by the CPU.
#[derive(Debug)]
pub(crate) struct Frame {
    /// One DMA-BUF per distinct buffer object.
    pub objects: Vec<OwnedFd>,
    pub planes: Vec<FramePlane>,
    pub width: u32,
    pub height: u32,
    /// DRM fourcc.
    pub fourcc: u32,
    /// `DRM_FORMAT_MOD_INVALID` when the layout is implicit.
    pub modifier: u64,
    /// The part of the buffer the CRTC shows.
    pub crop: Crop,
    /// From the cropped buffer to the picture as seen.
    pub transform: Transform,
    /// Identifies the buffer across exports.
    pub inode: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct FramePlane {
    pub object: usize,
    pub offset: u32,
    pub pitch: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Crop {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// What identifies an imported buffer: equal keys are the same memory in the same layout.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct FrameKey {
    inode: u64,
    width: u32,
    height: u32,
    fourcc: u32,
    modifier: u64,
    planes: Vec<FramePlane>,
}

impl Frame {
    pub(crate) fn key(&self) -> FrameKey {
        FrameKey {
            inode: self.inode,
            width: self.width,
            height: self.height,
            fourcc: self.fourcc,
            modifier: self.modifier,
            planes: self.planes.clone(),
        }
    }
}

/// `DRM_FORMAT_XRGB8888` and friends.
pub(crate) mod fourcc {
    const fn code(s: &[u8; 4]) -> u32 {
        u32::from_le_bytes(*s)
    }
    pub const XRGB8888: u32 = code(b"XR24");
    pub const ARGB8888: u32 = code(b"AR24");
    pub const XBGR8888: u32 = code(b"XB24");
    pub const ABGR8888: u32 = code(b"AB24");
    pub const XRGB2101010: u32 = code(b"XR30");
    pub const ARGB2101010: u32 = code(b"AR30");
    pub const XBGR2101010: u32 = code(b"XB30");
    pub const ABGR2101010: u32 = code(b"AB30");
}

pub(crate) enum Acquired {
    Frame(Rc<Frame>),
    Nothing,
    /// The CRTC stopped showing anything.
    Lost,
}

/// The scanout is taken as rendered in place once no flip has happened for this long.
const FRONT_BUFFER_AFTER: Duration = Duration::from_secs(1);
/// Polling interval when the CRTC gives no vblank events.
const POLL_INTERVAL: Duration = Duration::from_millis(16);
/// How long to poll before asking for vblank events again.
const VBLANK_RETRY: Duration = Duration::from_secs(1);

/// Watches one CRTC's primary plane.
pub(crate) struct KmsSource {
    card: Card,
    crtc_id: u32,
    pipe: u32,
    plane: u32,
    /// `SRC_X`, `SRC_Y`, `SRC_W`, `SRC_H` property ids of the plane.
    src_props: Option<[u32; 4]>,
    /// `rotation` property id of the plane.
    rotation_prop: Option<u32>,
    /// How the panel is mounted.
    panel: Transform,
    current: Option<(u32, Rc<Frame>)>,
    last_flip: Option<Instant>,
    vblank_pending: bool,
    /// When vblank events last failed.
    vblank_failed: Option<Instant>,
    last_poll: Option<Instant>,
    /// The CRTC's position in the buffer and its mode size.
    fallback_crop: Crop,
}

impl KmsSource {
    pub(crate) fn open(output: &Output) -> Result<Self> {
        let card = Card::open(&output.card)
            .map_err(|e| CaptureError::Platform(format!("open {}: {e}", output.card.display())))?;
        let crtc = card
            .crtc(output.crtc_id)
            .map_err(|e| CaptureError::Platform(format!("read CRTC {}: {e}", output.crtc_id)))?;
        let mode = crtc
            .mode
            .ok_or(CaptureError::DisplayNotFound(output.info.id))?;
        let plane = primary_plane(&card, output.crtc_id, output.pipe)
            .ok_or(CaptureError::DisplayNotFound(output.info.id))?;
        let (src_props, rotation_prop) = plane_props(&card, plane);
        Ok(Self {
            card,
            crtc_id: output.crtc_id,
            pipe: output.pipe,
            plane,
            src_props,
            rotation_prop,
            panel: output.panel,
            current: None,
            last_flip: None,
            vblank_pending: false,
            vblank_failed: None,
            last_poll: None,
            fallback_crop: Crop {
                x: crtc.x,
                y: crtc.y,
                width: u32::from(mode.hdisplay),
                height: u32::from(mode.vdisplay),
            },
        })
    }

    /// Hands out the current scanout again on the next [`Self::acquire`].
    pub(crate) fn refresh(&mut self) {
        self.current = None;
    }

    /// Waits up to `timeout` for the next refresh and returns the scanout if it may have changed.
    pub(crate) fn acquire(&mut self, timeout: Duration) -> Result<Acquired> {
        if !self.wait(timeout) {
            return Ok(Acquired::Nothing);
        }
        let plane = match self.card.plane(self.plane) {
            Ok(p) => p,
            Err(_) => return Ok(Acquired::Lost),
        };
        if plane.crtc_id != self.crtc_id || plane.fb_id == 0 {
            return Ok(Acquired::Lost);
        }
        let now = Instant::now();
        match &self.current {
            Some((fb, frame)) if *fb == plane.fb_id => {
                let front = self
                    .last_flip
                    .is_none_or(|t| now.saturating_duration_since(t) >= FRONT_BUFFER_AFTER);
                Ok(if front {
                    Acquired::Frame(frame.clone())
                } else {
                    Acquired::Nothing
                })
            }
            previous => {
                let flipped = previous.is_some();
                let frame = Rc::new(self.grab(plane.fb_id)?);
                if flipped {
                    self.last_flip = Some(now);
                }
                self.current = Some((plane.fb_id, frame.clone()));
                Ok(Acquired::Frame(frame))
            }
        }
    }

    /// Waits for a vblank, or polls when the CRTC has none; false if nothing can have changed.
    fn wait(&mut self, timeout: Duration) -> bool {
        let ms = timeout.as_millis().min(1000) as i32;
        let now = Instant::now();
        let retry = self
            .vblank_failed
            .is_none_or(|t| now.saturating_duration_since(t) >= VBLANK_RETRY);
        if retry && !self.vblank_pending {
            match self.card.request_vblank(self.pipe) {
                Ok(()) => {
                    self.vblank_pending = true;
                    self.vblank_failed = None;
                }
                Err(e) => {
                    if self.vblank_failed.is_none() {
                        tracing::debug!(crtc = self.crtc_id, "no vblank events, polling: {e}");
                    }
                    self.vblank_failed = Some(now);
                }
            }
        }
        if self.vblank_pending {
            return match self.card.wait_vblank(ms) {
                Ok(true) => {
                    self.vblank_pending = false;
                    true
                }
                Ok(false) => self.current.is_none(),
                Err(_) => {
                    self.vblank_pending = false;
                    self.vblank_failed = Some(Instant::now());
                    true
                }
            };
        }
        // Polling: one look per interval.
        let due = self.last_poll.map_or(now, |t| t + POLL_INTERVAL);
        let until = due.saturating_duration_since(now);
        if until > timeout {
            std::thread::sleep(timeout);
            return false;
        }
        std::thread::sleep(until);
        self.last_poll = Some(Instant::now());
        true
    }

    /// Exports framebuffer `fb_id`.
    fn grab(&self, fb_id: u32) -> Result<Frame> {
        let fb = self
            .card
            .framebuffer(fb_id)
            .map_err(|e| CaptureError::Platform(format!("DRM_IOCTL_MODE_GETFB2: {e}")))?;
        if fb.handles[0] == 0 {
            return Err(CaptureError::Platform(
                "the kernel gave no framebuffer handle: capture needs root (CAP_SYS_ADMIN)".into(),
            ));
        }
        let mut handles: Vec<u32> = Vec::new();
        let mut planes = Vec::new();
        for i in 0..4 {
            if fb.handles[i] == 0 {
                break;
            }
            let object = match handles.iter().position(|&h| h == fb.handles[i]) {
                Some(o) => o,
                None => {
                    handles.push(fb.handles[i]);
                    handles.len() - 1
                }
            };
            planes.push(FramePlane {
                object,
                offset: fb.offsets[i],
                pitch: fb.pitches[i],
            });
        }
        let objects: io::Result<Vec<OwnedFd>> =
            handles.iter().map(|&h| self.card.export(h)).collect();
        for &h in &handles {
            self.card.close_handle(h);
        }
        let objects = objects.map_err(|e| CaptureError::Platform(format!("PRIME export: {e}")))?;
        let inode = inode(&objects[0])
            .map_err(|e| CaptureError::Platform(format!("fstat DMA-BUF: {e}")))?;
        let modifier = if fb.flags & drm::FB_MODIFIERS != 0 {
            fb.modifier[0]
        } else {
            drm::MOD_INVALID
        };
        let (crop, transform) = self.placement(fb.width, fb.height);
        Ok(Frame {
            objects,
            planes,
            width: fb.width,
            height: fb.height,
            fourcc: fb.pixel_format,
            modifier,
            crop,
            transform,
            inode,
        })
    }

    /// The source rectangle of the plane, clamped to the framebuffer, and the transform to the
    /// picture as seen.
    fn placement(&self, width: u32, height: u32) -> (Crop, Transform) {
        let props = if self.src_props.is_some() || self.rotation_prop.is_some() {
            self.card
                .properties(self.plane, drm::OBJECT_PLANE)
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let get = |id: u32| props.iter().find(|(p, _)| *p == id).map(|(_, v)| *v);
        let rotation = self
            .rotation_prop
            .and_then(get)
            .map_or(Transform::IDENTITY, Transform::from_drm_rotation);
        let from_props = self.src_props.and_then(|ids| {
            let get = |id: u32| get(id).map(|v| (v >> 16) as u32);
            Some(Crop {
                x: get(ids[0])?,
                y: get(ids[1])?,
                width: get(ids[2])?,
                height: get(ids[3])?,
            })
        });
        let crop = from_props.unwrap_or_else(|| {
            let f = self.fallback_crop;
            let (width, height) = rotation.size(f.width, f.height);
            Crop { width, height, ..f }
        });
        (
            clamp_crop(crop, width, height),
            transform::upright(rotation, self.panel),
        )
    }
}

fn clamp_crop(c: Crop, width: u32, height: u32) -> Crop {
    let x = c.x.min(width.saturating_sub(1));
    let y = c.y.min(height.saturating_sub(1));
    let w = if c.width == 0 { width } else { c.width };
    let h = if c.height == 0 { height } else { c.height };
    Crop {
        x,
        y,
        width: w.min(width - x),
        height: h.min(height - y),
    }
}

fn inode(fd: &OwnedFd) -> io::Result<u64> {
    // SAFETY: fstat(2) into a zeroed struct.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(st.st_ino)
}

/// The primary plane showing `crtc_id`, or the primary plane that can.
pub(crate) fn primary_plane(card: &Card, crtc_id: u32, pipe: u32) -> Option<u32> {
    let mut candidate = None;
    for id in card.planes().ok()? {
        let Ok(plane) = card.plane(id) else { continue };
        let Ok(props) = card.properties(id, drm::OBJECT_PLANE) else {
            continue;
        };
        if card.find_property(&props, "type") != Some(drm::PLANE_TYPE_PRIMARY) {
            continue;
        }
        if plane.crtc_id == crtc_id {
            return Some(id);
        }
        if candidate.is_none() && plane.possible_crtcs & (1 << pipe) != 0 {
            candidate = Some(id);
        }
    }
    candidate
}

/// The plane's `SRC_*` and `rotation` property ids.
fn plane_props(card: &Card, plane: u32) -> (Option<[u32; 4]>, Option<u32>) {
    let Ok(props) = card.properties(plane, drm::OBJECT_PLANE) else {
        return (None, None);
    };
    let names: Vec<(u32, String)> = props
        .iter()
        .filter_map(|(p, _)| Some((*p, card.property_name(*p).ok()?)))
        .collect();
    let id = |name: &str| names.iter().find(|(_, n)| n == name).map(|(p, _)| *p);
    let src = match (id("SRC_X"), id("SRC_Y"), id("SRC_W"), id("SRC_H")) {
        (Some(x), Some(y), Some(w), Some(h)) => Some([x, y, w, h]),
        _ => None,
    };
    (src, id("rotation"))
}

/// The rotation of `plane`, identity without a `rotation` property.
pub(crate) fn plane_rotation(card: &Card, plane: u32) -> Transform {
    card.properties(plane, drm::OBJECT_PLANE)
        .ok()
        .and_then(|props| card.find_property(&props, "rotation"))
        .map_or(Transform::IDENTITY, Transform::from_drm_rotation)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crop_stays_inside_the_framebuffer() {
        let c = |x, y, width, height| Crop {
            x,
            y,
            width,
            height,
        };
        assert_eq!(
            clamp_crop(c(0, 0, 1920, 1080), 1920, 1080),
            c(0, 0, 1920, 1080)
        );
        assert_eq!(
            clamp_crop(c(1920, 0, 1920, 1080), 3840, 1080),
            c(1920, 0, 1920, 1080)
        );
        assert_eq!(
            clamp_crop(c(1920, 0, 2560, 1440), 3840, 1080),
            c(1920, 0, 1920, 1080)
        );
        assert_eq!(clamp_crop(c(0, 0, 0, 0), 800, 600), c(0, 0, 800, 600));
    }

    /// Shows a pattern on a free CRTC of a card this process can be master of (vkms in CI),
    /// captures it through the normal path and reads the DMA-BUF back.
    #[test]
    fn captures_a_scanout_buffer() {
        let Some(mut screen) = TestScreen::new() else {
            assert!(
                std::env::var_os("WS_REQUIRE_KMS").is_none(),
                "WS_REQUIRE_KMS is set but no DRM device could be driven"
            );
            eprintln!(
                "skipped: no DRM device this process can drive (needs root and a free card, e.g. vkms)"
            );
            return;
        };
        let output = super::super::display::enumerate()
            .expect("enumerate")
            .into_iter()
            .find(|o| o.card == screen.path && o.crtc_id == screen.crtc_id)
            .expect("the lit display is listed");
        assert_eq!(
            (output.info.width, output.info.height),
            (screen.width, screen.height)
        );
        let mut source = KmsSource::open(&output).expect("open source");
        let frame = loop {
            match source.acquire(Duration::from_millis(100)).expect("acquire") {
                Acquired::Frame(f) => break f,
                Acquired::Nothing => continue,
                Acquired::Lost => panic!("display lost"),
            }
        };
        assert_eq!(frame.fourcc, fourcc::XRGB8888);
        assert_eq!((frame.width, frame.height), (screen.width, screen.height));
        assert_eq!(
            frame.crop,
            Crop {
                x: 0,
                y: 0,
                width: screen.width,
                height: screen.height
            }
        );
        let pixels = read_back(&frame);
        let pitch = frame.planes[0].pitch as usize;
        for (x, y) in [
            (0, 0),
            (17, 3),
            (screen.width - 1, screen.height - 1),
            (screen.width / 2, screen.height / 3),
        ] {
            let at = frame.planes[0].offset as usize + y as usize * pitch + x as usize * 4;
            let px = u32::from_le_bytes(pixels[at..at + 4].try_into().unwrap());
            assert_eq!(px & 0x00ff_ffff, pattern(x, y), "pixel {x},{y}");
        }
        let again = Rc::new(
            source
                .grab(source.current.as_ref().unwrap().0)
                .expect("grab again"),
        );
        assert_eq!(again.key(), frame.key());
        let fb = source.current.as_ref().unwrap().0;
        let runs = 200u32;
        let (mut total, mut worst) = (Duration::ZERO, Duration::ZERO);
        for _ in 0..runs {
            let t = std::time::Instant::now();
            drop(std::hint::black_box(source.grab(fb).expect("grab")));
            let took = t.elapsed();
            total += took;
            worst = worst.max(took);
        }
        eprintln!(
            "KMS grab (GETFB2 + PRIME export) of a {}x{} scanout: {:?} average, {:?} worst over {runs}",
            screen.width,
            screen.height,
            total / runs,
            worst
        );
        screen.show_second();
        let next = loop {
            match source.acquire(Duration::from_millis(100)).expect("acquire") {
                Acquired::Frame(f) if f.key() != frame.key() => break f,
                Acquired::Frame(_) | Acquired::Nothing => continue,
                Acquired::Lost => panic!("display lost"),
            }
        };
        let pixels = read_back(&next);
        assert_eq!(
            u32::from_le_bytes(pixels[0..4].try_into().unwrap()) & 0x00ff_ffff,
            0x00ff_00ff
        );
        drop(source);

        // A plane turned a quarter scans out a portrait buffer as the landscape mode.
        if screen.turn().is_none() {
            eprintln!("rotation skipped: the primary plane cannot turn a quarter");
            return;
        }
        let (w, h) = (screen.height, screen.width);
        let output = super::super::display::enumerate()
            .expect("enumerate")
            .into_iter()
            .find(|o| o.card == screen.path && o.crtc_id == screen.crtc_id)
            .expect("the turned display is listed");
        assert_eq!((output.info.width, output.info.height), (w, h));
        let mut source = KmsSource::open(&output).expect("open source");
        let frame = loop {
            match source.acquire(Duration::from_millis(100)).expect("acquire") {
                Acquired::Frame(f) => break f,
                Acquired::Nothing => continue,
                Acquired::Lost => panic!("display lost"),
            }
        };
        assert_eq!((frame.width, frame.height), (w, h));
        assert_eq!(
            frame.crop,
            Crop {
                x: 0,
                y: 0,
                width: w,
                height: h
            }
        );
        assert_eq!(frame.transform, Transform::IDENTITY);
        let pixels = read_back(&frame);
        let pitch = frame.planes[0].pitch as usize;
        for (x, y) in [(0, 0), (w - 1, 0), (w - 1, h - 1), (w / 3, h / 2)] {
            let at = frame.planes[0].offset as usize + y as usize * pitch + x as usize * 4;
            let px = u32::from_le_bytes(pixels[at..at + 4].try_into().unwrap());
            assert_eq!(px & 0x00ff_ffff, pattern(x, y), "turned pixel {x},{y}");
        }
    }

    fn pattern(x: u32, y: u32) -> u32 {
        ((x & 0xff) << 16) | ((y & 0xff) << 8) | ((x ^ y) & 0xff)
    }

    fn read_back(frame: &Frame) -> Vec<u8> {
        let fd = frame.objects[0].as_raw_fd();
        // SAFETY: test-only mapping of a linear DMA-BUF, unmapped before returning.
        unsafe {
            let size = libc::lseek(fd, 0, libc::SEEK_END);
            assert!(size > 0, "DMA-BUF size");
            let p = libc::mmap(
                std::ptr::null_mut(),
                size as usize,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                0,
            );
            assert_ne!(
                p,
                libc::MAP_FAILED,
                "mmap DMA-BUF: {}",
                io::Error::last_os_error()
            );
            // DMA_BUF_IOCTL_SYNC, DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ, then END.
            let mut sync: u64 = 1;
            libc::ioctl(fd, 0x4008_6200, &mut sync);
            let data = std::slice::from_raw_parts(p.cast::<u8>(), size as usize).to_vec();
            sync = 4 | 1;
            libc::ioctl(fd, 0x4008_6200, &mut sync);
            libc::munmap(p, size as usize);
            data
        }
    }

    struct Dumb {
        handle: u32,
        fb: u32,
    }

    /// A CRTC lit with dumb buffers, torn down on drop.
    struct TestScreen {
        card: Card,
        path: std::path::PathBuf,
        crtc_id: u32,
        connector_id: u32,
        mode: drm::ModeInfo,
        width: u32,
        height: u32,
        buffers: Vec<Dumb>,
        /// The primary plane and its `rotation` property id.
        rotation: Option<(u32, u32)>,
        /// The plane is turned a quarter: buffers are `height` x `width`.
        turned: bool,
    }

    impl TestScreen {
        fn new() -> Option<Self> {
            drm::cards().into_iter().find_map(|path| Self::on(&path))
        }

        fn on(path: &std::path::Path) -> Option<Self> {
            let card = Card::open(path).ok()?;
            card.set_master().ok()?;
            let res = card.resources().ok()?;
            for &id in &res.connectors {
                let Ok(c) = card.connector(id) else { continue };
                if !c.connected || c.modes.is_empty() || c.kind == drm::CONNECTOR_WRITEBACK {
                    continue;
                }
                let (pipe, crtc) = c.encoders.iter().find_map(|&e| {
                    let (_, possible) = card.encoder(e).ok()?;
                    (0..res.crtcs.len())
                        .find(|i| possible & (1 << i) != 0)
                        .map(|i| (i as u32, res.crtcs[i]))
                })?;
                let rotation = primary_plane(&card, crtc, pipe)
                    .and_then(|plane| Some((plane, plane_props(&card, plane).1?)));
                let mode = c.modes[0];
                let mut screen = Self {
                    width: u32::from(mode.hdisplay),
                    height: u32::from(mode.vdisplay),
                    card,
                    path: path.to_owned(),
                    crtc_id: crtc,
                    connector_id: id,
                    mode,
                    buffers: Vec::new(),
                    rotation,
                    turned: false,
                };
                screen.unturn();
                screen.show(pattern)?;
                return Some(screen);
            }
            None
        }

        /// Sets the primary plane's rotation, with the CRTC off.
        fn rotate(&self, bits: u64) -> Option<()> {
            let (plane, prop) = self.rotation?;
            let current = self
                .card
                .properties(plane, drm::OBJECT_PLANE)
                .ok()?
                .into_iter()
                .find(|(p, _)| *p == prop)
                .map(|(_, v)| v)?;
            if current == bits {
                return Some(());
            }
            let _ = self.card.disable_crtc(self.crtc_id);
            self.card
                .set_property(plane, drm::OBJECT_PLANE, prop, bits)
                .ok()
        }

        fn unturn(&mut self) {
            let _ = self.rotate(ws_proto::orientation::drm_rotation::ROTATE_0);
            self.turned = false;
        }

        /// Turns the plane a quarter and shows the pattern on a portrait buffer.
        fn turn(&mut self) -> Option<()> {
            self.rotate(ws_proto::orientation::drm_rotation::ROTATE_90)?;
            self.turned = true;
            self.show(pattern)
        }

        fn show(&mut self, fill: impl Fn(u32, u32) -> u32) -> Option<()> {
            let (width, height) = if self.turned {
                (self.height, self.width)
            } else {
                (self.width, self.height)
            };
            let d = self.card.create_dumb(width, height).ok()?;
            let offset = self.card.map_dumb(d.handle).ok()?;
            // SAFETY: maps the dumb buffer just created, unmapped below.
            unsafe {
                let p = libc::mmap(
                    std::ptr::null_mut(),
                    d.size as usize,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    self.card.raw(),
                    offset as libc::off_t,
                );
                if p == libc::MAP_FAILED {
                    return None;
                }
                for y in 0..height {
                    let row = p.cast::<u8>().add((y * d.pitch) as usize).cast::<u32>();
                    for x in 0..width {
                        row.add(x as usize).write_unaligned(fill(x, y));
                    }
                }
                libc::munmap(p, d.size as usize);
            }
            let mut fb = drm::FbCmd2 {
                width,
                height,
                pixel_format: fourcc::XRGB8888,
                ..Default::default()
            };
            fb.handles[0] = d.handle;
            fb.pitches[0] = d.pitch;
            let fb_id = self.card.add_framebuffer(&mut fb).ok()?;
            self.buffers.push(Dumb {
                handle: d.handle,
                fb: fb_id,
            });
            self.card
                .set_crtc(self.crtc_id, fb_id, self.connector_id, &self.mode)
                .ok()
        }

        fn show_second(&mut self) {
            self.show(|_, _| 0x00ff_00ff).expect("second buffer");
        }
    }

    impl Drop for TestScreen {
        fn drop(&mut self) {
            if self.turned {
                let _ = self.card.disable_crtc(self.crtc_id);
            }
            for b in &self.buffers {
                self.card.remove_framebuffer(b.fb);
                self.card.destroy_dumb(b.handle);
            }
            self.unturn();
            let _ = self.card.drop_master();
        }
    }

    #[test]
    fn fourcc_codes() {
        assert_eq!(fourcc::XRGB8888, 0x3432_5258);
        assert_eq!(fourcc::ABGR2101010, 0x3033_4241);
    }
}
