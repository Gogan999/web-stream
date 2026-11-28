//! Raw DRM/KMS ioctls (`drm.h`, `drm_mode.h`), enough to read the scanout state and export
//! framebuffers.

use std::ffi::c_void;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};

const fn iowr(nr: u32, size: usize) -> u64 {
    ((3u64) << 30) | ((size as u64) << 16) | ((b'd' as u64) << 8) | nr as u64
}

const fn iow(nr: u32, size: usize) -> u64 {
    ((1u64) << 30) | ((size as u64) << 16) | ((b'd' as u64) << 8) | nr as u64
}

#[cfg(test)]
const fn io(nr: u32) -> u64 {
    ((b'd' as u64) << 8) | nr as u64
}

#[repr(C)]
struct DrmVersion {
    major: i32,
    minor: i32,
    patch: i32,
    name_len: usize,
    name: *mut u8,
    date_len: usize,
    date: *mut u8,
    desc_len: usize,
    desc: *mut u8,
}

impl Default for DrmVersion {
    fn default() -> Self {
        Self {
            major: 0,
            minor: 0,
            patch: 0,
            name_len: 0,
            name: std::ptr::null_mut(),
            date_len: 0,
            date: std::ptr::null_mut(),
            desc_len: 0,
            desc: std::ptr::null_mut(),
        }
    }
}

#[repr(C)]
#[derive(Default)]
struct GemClose {
    handle: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Default)]
struct SetClientCap {
    capability: u64,
    value: u64,
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
struct WaitVblankRequest {
    kind: u32,
    sequence: u32,
    signal: u64,
    /// Room for the union's reply member.
    reply_pad: u64,
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

/// `struct drm_mode_modeinfo`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ModeInfo {
    pub clock: u32,
    pub hdisplay: u16,
    pub hsync_start: u16,
    pub hsync_end: u16,
    pub htotal: u16,
    pub hskew: u16,
    pub vdisplay: u16,
    pub vsync_start: u16,
    pub vsync_end: u16,
    pub vtotal: u16,
    pub vscan: u16,
    pub vrefresh: u32,
    pub flags: u32,
    pub kind: u32,
    pub name: [u8; 32],
}

const MODE_FLAG_INTERLACE: u32 = 1 << 4;
const MODE_FLAG_DBLSCAN: u32 = 1 << 5;

impl ModeInfo {
    /// Refresh rate in millihertz, from the pixel clock and timings.
    pub(crate) fn refresh_mhz(&self) -> u32 {
        let mut den = u64::from(self.htotal) * u64::from(self.vtotal);
        if den == 0 {
            return self.vrefresh.saturating_mul(1000);
        }
        let mut num = u64::from(self.clock) * 1_000_000;
        if self.flags & MODE_FLAG_INTERLACE != 0 {
            num *= 2;
        }
        if self.flags & MODE_FLAG_DBLSCAN != 0 {
            den *= 2;
        }
        if self.vscan > 1 {
            den *= u64::from(self.vscan);
        }
        let mhz = num / den;
        u32::try_from(mhz).unwrap_or(u32::MAX)
    }
}

#[repr(C)]
#[derive(Default)]
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
struct GetBlob {
    blob_id: u32,
    length: u32,
    data: u64,
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

/// `struct drm_mode_fb_cmd2`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct FbCmd2 {
    pub fb_id: u32,
    pub width: u32,
    pub height: u32,
    pub pixel_format: u32,
    pub flags: u32,
    pub handles: [u32; 4],
    pub pitches: [u32; 4],
    pub offsets: [u32; 4],
    pub modifier: [u64; 4],
}

/// `DRM_MODE_FB_MODIFIERS`.
pub(crate) const FB_MODIFIERS: u32 = 2;
/// `DRM_FORMAT_MOD_INVALID`.
pub(crate) const MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

#[repr(C)]
#[derive(Default)]
struct ObjGetProperties {
    props_ptr: u64,
    prop_values_ptr: u64,
    count_props: u32,
    obj_id: u32,
    obj_type: u32,
}

#[cfg(test)]
#[repr(C)]
#[derive(Default)]
struct ObjSetProperty {
    value: u64,
    prop_id: u32,
    obj_id: u32,
    obj_type: u32,
}

#[cfg(test)]
#[repr(C)]
#[derive(Default)]
pub(crate) struct CreateDumb {
    pub height: u32,
    pub width: u32,
    pub bpp: u32,
    pub flags: u32,
    pub handle: u32,
    pub pitch: u32,
    pub size: u64,
}

#[cfg(test)]
#[repr(C)]
#[derive(Default)]
pub(crate) struct MapDumb {
    pub handle: u32,
    pub pad: u32,
    pub offset: u64,
}

const IOCTL_VERSION: u64 = iowr(0x00, size_of::<DrmVersion>());
const IOCTL_GEM_CLOSE: u64 = iow(0x09, size_of::<GemClose>());
const IOCTL_SET_CLIENT_CAP: u64 = iow(0x0d, size_of::<SetClientCap>());
#[cfg(test)]
const IOCTL_SET_MASTER: u64 = io(0x1e);
#[cfg(test)]
const IOCTL_DROP_MASTER: u64 = io(0x1f);
const IOCTL_PRIME_HANDLE_TO_FD: u64 = iowr(0x2d, size_of::<PrimeHandle>());
const IOCTL_WAIT_VBLANK: u64 = iowr(0x3a, size_of::<WaitVblankRequest>());
const IOCTL_MODE_GETRESOURCES: u64 = iowr(0xa0, size_of::<CardRes>());
const IOCTL_MODE_GETCRTC: u64 = iowr(0xa1, size_of::<ModeCrtc>());
#[cfg(test)]
const IOCTL_MODE_SETCRTC: u64 = iowr(0xa2, size_of::<ModeCrtc>());
const IOCTL_MODE_GETENCODER: u64 = iowr(0xa6, size_of::<GetEncoder>());
const IOCTL_MODE_GETCONNECTOR: u64 = iowr(0xa7, size_of::<GetConnector>());
const IOCTL_MODE_GETPROPERTY: u64 = iowr(0xaa, size_of::<GetProperty>());
const IOCTL_MODE_GETPROPBLOB: u64 = iowr(0xac, size_of::<GetBlob>());
#[cfg(test)]
const IOCTL_MODE_RMFB: u64 = iowr(0xaf, size_of::<u32>());
#[cfg(test)]
const IOCTL_MODE_CREATE_DUMB: u64 = iowr(0xb2, size_of::<CreateDumb>());
#[cfg(test)]
const IOCTL_MODE_MAP_DUMB: u64 = iowr(0xb3, size_of::<MapDumb>());
#[cfg(test)]
const IOCTL_MODE_DESTROY_DUMB: u64 = iowr(0xb4, size_of::<u32>());
const IOCTL_MODE_GETPLANERESOURCES: u64 = iowr(0xb5, size_of::<GetPlaneRes>());
const IOCTL_MODE_GETPLANE: u64 = iowr(0xb6, size_of::<GetPlane>());
#[cfg(test)]
const IOCTL_MODE_ADDFB2: u64 = iowr(0xb8, size_of::<FbCmd2>());
const IOCTL_MODE_OBJ_GETPROPERTIES: u64 = iowr(0xb9, size_of::<ObjGetProperties>());
#[cfg(test)]
const IOCTL_MODE_OBJ_SETPROPERTY: u64 = iowr(0xba, size_of::<ObjSetProperty>());
const IOCTL_MODE_GETFB2: u64 = iowr(0xce, size_of::<FbCmd2>());

pub(crate) const OBJECT_PLANE: u32 = 0xeeee_eeee;

const CLIENT_CAP_UNIVERSAL_PLANES: u64 = 2;
const VBLANK_RELATIVE: u32 = 0x1;
const VBLANK_EVENT: u32 = 0x400_0000;
const VBLANK_SECONDARY: u32 = 0x2000_0000;
const VBLANK_HIGH_CRTC_MASK: u32 = 0x3e;
const EVENT_VBLANK: u32 = 0x01;

/// `DRM_PLANE_TYPE_PRIMARY`.
pub(crate) const PLANE_TYPE_PRIMARY: u64 = 1;

/// A connector's state.
#[derive(Debug, Clone)]
pub(crate) struct Connector {
    pub kind: u32,
    pub kind_id: u32,
    pub connected: bool,
    pub encoder_id: u32,
    #[cfg_attr(not(test), allow(dead_code))]
    pub encoders: Vec<u32>,
    #[cfg_attr(not(test), allow(dead_code))]
    pub modes: Vec<ModeInfo>,
    pub props: Vec<(u32, u64)>,
}

impl Connector {
    /// The kernel's name for the connector, e.g. `HDMI-A-1`.
    pub(crate) fn name(&self) -> String {
        format!("{}-{}", connector_type_name(self.kind), self.kind_id)
    }
}

/// `DRM_MODE_CONNECTOR_*` names as the kernel prints them.
pub(crate) fn connector_type_name(kind: u32) -> &'static str {
    const NAMES: [&str; 21] = [
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
    NAMES.get(kind as usize).copied().unwrap_or("Unknown")
}

/// `DRM_MODE_CONNECTOR_WRITEBACK`.
pub(crate) const CONNECTOR_WRITEBACK: u32 = 18;
/// `DRM_MODE_CONNECTOR_eDP`.
pub(crate) const CONNECTOR_EDP: u32 = 14;
/// `DRM_MODE_CONNECTOR_LVDS`.
pub(crate) const CONNECTOR_LVDS: u32 = 7;
/// `DRM_MODE_CONNECTOR_DSI`.
pub(crate) const CONNECTOR_DSI: u32 = 16;

/// A CRTC's state.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Crtc {
    pub x: u32,
    pub y: u32,
    pub mode: Option<ModeInfo>,
}

/// A plane's state.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Plane {
    pub crtc_id: u32,
    pub fb_id: u32,
    pub possible_crtcs: u32,
}

/// The mode objects of a card.
#[derive(Debug, Clone, Default)]
pub(crate) struct Resources {
    pub crtcs: Vec<u32>,
    pub connectors: Vec<u32>,
}

/// An open DRM device node.
pub(crate) struct Card {
    fd: OwnedFd,
}

fn ioctl<T>(fd: RawFd, request: u64, arg: &mut T) -> io::Result<()> {
    loop {
        // SAFETY: `arg` is the struct the request number encodes.
        let r = unsafe {
            libc::ioctl(
                fd,
                request as libc::c_ulong,
                std::ptr::from_mut(arg).cast::<c_void>(),
            )
        };
        if r == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::EINTR) | Some(libc::EAGAIN) => continue,
            _ => return Err(e),
        }
    }
}

fn ptr<T>(v: &mut [T]) -> u64 {
    v.as_mut_ptr() as u64
}

impl Card {
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        let cpath = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        // SAFETY: plain open(2) on a NUL-terminated path.
        let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` was just opened and is owned here.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let card = Self { fd };
        let mut cap = SetClientCap {
            capability: CLIENT_CAP_UNIVERSAL_PLANES,
            value: 1,
        };
        let _ = ioctl(card.raw(), IOCTL_SET_CLIENT_CAP, &mut cap);
        Ok(card)
    }

    pub(crate) fn raw(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// The kernel driver's name, e.g. `i915`, `amdgpu`, `nvidia-drm`.
    pub(crate) fn driver(&self) -> io::Result<String> {
        let mut v = DrmVersion::default();
        ioctl(self.raw(), IOCTL_VERSION, &mut v)?;
        let mut name = vec![0u8; v.name_len];
        let mut v = DrmVersion {
            name_len: name.len(),
            name: name.as_mut_ptr(),
            ..Default::default()
        };
        ioctl(self.raw(), IOCTL_VERSION, &mut v)?;
        name.truncate(v.name_len.min(name.len()));
        Ok(String::from_utf8_lossy(&name).into_owned())
    }

    pub(crate) fn resources(&self) -> io::Result<Resources> {
        for _ in 0..8 {
            let mut res = CardRes::default();
            ioctl(self.raw(), IOCTL_MODE_GETRESOURCES, &mut res)?;
            let mut crtcs = vec![0u32; res.count_crtcs as usize];
            let mut connectors = vec![0u32; res.count_connectors as usize];
            let (nc, nn) = (res.count_crtcs, res.count_connectors);
            let mut res = CardRes {
                crtc_id_ptr: ptr(&mut crtcs),
                connector_id_ptr: ptr(&mut connectors),
                count_crtcs: nc,
                count_connectors: nn,
                ..Default::default()
            };
            ioctl(self.raw(), IOCTL_MODE_GETRESOURCES, &mut res)?;
            if res.count_crtcs != nc || res.count_connectors != nn {
                continue;
            }
            return Ok(Resources { crtcs, connectors });
        }
        Err(io::Error::other("mode resources kept changing"))
    }

    pub(crate) fn connector(&self, id: u32) -> io::Result<Connector> {
        for _ in 0..8 {
            let mut c = GetConnector {
                connector_id: id,
                ..Default::default()
            };
            ioctl(self.raw(), IOCTL_MODE_GETCONNECTOR, &mut c)?;
            let (nm, np, ne) = (c.count_modes, c.count_props, c.count_encoders);
            let mut encoders = vec![0u32; ne as usize];
            let mut modes = vec![ModeInfo::default(); nm as usize];
            let mut props = vec![0u32; np as usize];
            let mut values = vec![0u64; np as usize];
            let mut c = GetConnector {
                connector_id: id,
                encoders_ptr: ptr(&mut encoders),
                modes_ptr: ptr(&mut modes),
                props_ptr: ptr(&mut props),
                prop_values_ptr: ptr(&mut values),
                count_modes: nm,
                count_props: np,
                count_encoders: ne,
                ..Default::default()
            };
            ioctl(self.raw(), IOCTL_MODE_GETCONNECTOR, &mut c)?;
            if c.count_modes != nm || c.count_props != np || c.count_encoders != ne {
                continue;
            }
            return Ok(Connector {
                kind: c.connector_type,
                kind_id: c.connector_type_id,
                connected: c.connection == 1,
                encoder_id: c.encoder_id,
                encoders,
                modes,
                props: props.into_iter().zip(values).collect(),
            });
        }
        Err(io::Error::other("connector kept changing"))
    }

    /// The CRTC an encoder drives, 0 if none.
    pub(crate) fn encoder_crtc(&self, id: u32) -> io::Result<u32> {
        Ok(self.encoder(id)?.0)
    }

    /// An encoder's current CRTC and the CRTCs it can drive, as a bit mask over the card's CRTCs.
    pub(crate) fn encoder(&self, id: u32) -> io::Result<(u32, u32)> {
        let mut e = GetEncoder {
            encoder_id: id,
            ..Default::default()
        };
        ioctl(self.raw(), IOCTL_MODE_GETENCODER, &mut e)?;
        Ok((e.crtc_id, e.possible_crtcs))
    }

    pub(crate) fn crtc(&self, id: u32) -> io::Result<Crtc> {
        let mut c = ModeCrtc {
            crtc_id: id,
            ..Default::default()
        };
        ioctl(self.raw(), IOCTL_MODE_GETCRTC, &mut c)?;
        Ok(Crtc {
            x: c.x,
            y: c.y,
            mode: (c.mode_valid != 0).then_some(c.mode),
        })
    }

    pub(crate) fn planes(&self) -> io::Result<Vec<u32>> {
        for _ in 0..8 {
            let mut r = GetPlaneRes::default();
            ioctl(self.raw(), IOCTL_MODE_GETPLANERESOURCES, &mut r)?;
            let n = r.count_planes;
            let mut ids = vec![0u32; n as usize];
            let mut r = GetPlaneRes {
                plane_id_ptr: ptr(&mut ids),
                count_planes: n,
            };
            ioctl(self.raw(), IOCTL_MODE_GETPLANERESOURCES, &mut r)?;
            if r.count_planes == n {
                return Ok(ids);
            }
        }
        Err(io::Error::other("planes kept changing"))
    }

    pub(crate) fn plane(&self, id: u32) -> io::Result<Plane> {
        let mut p = GetPlane {
            plane_id: id,
            ..Default::default()
        };
        ioctl(self.raw(), IOCTL_MODE_GETPLANE, &mut p)?;
        Ok(Plane {
            crtc_id: p.crtc_id,
            fb_id: p.fb_id,
            possible_crtcs: p.possible_crtcs,
        })
    }

    /// An object's properties as `(property id, value)`.
    pub(crate) fn properties(&self, obj_id: u32, obj_type: u32) -> io::Result<Vec<(u32, u64)>> {
        for _ in 0..8 {
            let mut o = ObjGetProperties {
                obj_id,
                obj_type,
                ..Default::default()
            };
            ioctl(self.raw(), IOCTL_MODE_OBJ_GETPROPERTIES, &mut o)?;
            let n = o.count_props;
            let mut ids = vec![0u32; n as usize];
            let mut values = vec![0u64; n as usize];
            let mut o = ObjGetProperties {
                props_ptr: ptr(&mut ids),
                prop_values_ptr: ptr(&mut values),
                count_props: n,
                obj_id,
                obj_type,
            };
            ioctl(self.raw(), IOCTL_MODE_OBJ_GETPROPERTIES, &mut o)?;
            if o.count_props == n {
                return Ok(ids.into_iter().zip(values).collect());
            }
        }
        Err(io::Error::other("properties kept changing"))
    }

    pub(crate) fn property_name(&self, prop_id: u32) -> io::Result<String> {
        let mut p = GetProperty {
            values_ptr: 0,
            enum_blob_ptr: 0,
            prop_id,
            flags: 0,
            name: [0; 32],
            count_values: 0,
            count_enum_blobs: 0,
        };
        ioctl(self.raw(), IOCTL_MODE_GETPROPERTY, &mut p)?;
        Ok(cstr(&p.name))
    }

    /// The value of the property named `name` among `props`.
    pub(crate) fn find_property(&self, props: &[(u32, u64)], name: &str) -> Option<u64> {
        props
            .iter()
            .find(|(id, _)| self.property_name(*id).is_ok_and(|n| n == name))
            .map(|(_, v)| *v)
    }

    pub(crate) fn blob(&self, blob_id: u32) -> io::Result<Vec<u8>> {
        let mut b = GetBlob {
            blob_id,
            ..Default::default()
        };
        ioctl(self.raw(), IOCTL_MODE_GETPROPBLOB, &mut b)?;
        let mut data = vec![0u8; b.length as usize];
        let mut b = GetBlob {
            blob_id,
            length: data.len() as u32,
            data: ptr(&mut data),
        };
        ioctl(self.raw(), IOCTL_MODE_GETPROPBLOB, &mut b)?;
        data.truncate(b.length as usize);
        Ok(data)
    }

    /// A framebuffer's layout, with GEM handles the caller must close.
    pub(crate) fn framebuffer(&self, fb_id: u32) -> io::Result<FbCmd2> {
        let mut fb = FbCmd2 {
            fb_id,
            ..Default::default()
        };
        ioctl(self.raw(), IOCTL_MODE_GETFB2, &mut fb)?;
        Ok(fb)
    }

    /// Exports a GEM handle as a DMA-BUF.
    pub(crate) fn export(&self, handle: u32) -> io::Result<OwnedFd> {
        let mut p = PrimeHandle {
            handle,
            flags: (libc::O_CLOEXEC | libc::O_RDWR) as u32,
            fd: -1,
        };
        if ioctl(self.raw(), IOCTL_PRIME_HANDLE_TO_FD, &mut p).is_err() {
            p.flags = libc::O_CLOEXEC as u32;
            ioctl(self.raw(), IOCTL_PRIME_HANDLE_TO_FD, &mut p)?;
        }
        // SAFETY: the kernel returned a new descriptor owned by the caller.
        Ok(unsafe { OwnedFd::from_raw_fd(p.fd) })
    }

    pub(crate) fn close_handle(&self, handle: u32) {
        let mut c = GemClose { handle, pad: 0 };
        let _ = ioctl(self.raw(), IOCTL_GEM_CLOSE, &mut c);
    }

    /// Asks for a vblank event on the CRTC at `pipe` (its index in the card's resources).
    pub(crate) fn request_vblank(&self, pipe: u32) -> io::Result<()> {
        let high = if pipe > 1 {
            (pipe << 1) & VBLANK_HIGH_CRTC_MASK
        } else if pipe == 1 {
            VBLANK_SECONDARY
        } else {
            0
        };
        let mut v = WaitVblankRequest {
            kind: VBLANK_RELATIVE | VBLANK_EVENT | high,
            sequence: 1,
            signal: 0,
            reply_pad: 0,
        };
        ioctl(self.raw(), IOCTL_WAIT_VBLANK, &mut v)
    }

    /// Waits up to `timeout_ms` for events and reads them; true if a vblank arrived.
    pub(crate) fn wait_vblank(&self, timeout_ms: i32) -> io::Result<bool> {
        let mut pfd = libc::pollfd {
            fd: self.raw(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd.
        let n = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if n < 0 {
            let e = io::Error::last_os_error();
            return if e.kind() == io::ErrorKind::Interrupted {
                Ok(false)
            } else {
                Err(e)
            };
        }
        if n == 0 || pfd.revents & libc::POLLIN == 0 {
            return Ok(false);
        }
        let mut buf = [0u8; 1024];
        // SAFETY: reads into a local buffer.
        let len = unsafe { libc::read(self.raw(), buf.as_mut_ptr().cast(), buf.len()) };
        if len < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(has_vblank_event(&buf[..len as usize]))
    }
}

/// Driving a display, for tests.
#[cfg(test)]
impl Card {
    pub(crate) fn set_master(&self) -> io::Result<()> {
        let mut unused = 0u32;
        ioctl(self.raw(), IOCTL_SET_MASTER, &mut unused)
    }

    pub(crate) fn drop_master(&self) -> io::Result<()> {
        let mut unused = 0u32;
        ioctl(self.raw(), IOCTL_DROP_MASTER, &mut unused)
    }

    pub(crate) fn create_dumb(&self, width: u32, height: u32) -> io::Result<CreateDumb> {
        let mut d = CreateDumb {
            width,
            height,
            bpp: 32,
            ..Default::default()
        };
        ioctl(self.raw(), IOCTL_MODE_CREATE_DUMB, &mut d)?;
        Ok(d)
    }

    pub(crate) fn map_dumb(&self, handle: u32) -> io::Result<u64> {
        let mut m = MapDumb {
            handle,
            ..Default::default()
        };
        ioctl(self.raw(), IOCTL_MODE_MAP_DUMB, &mut m)?;
        Ok(m.offset)
    }

    pub(crate) fn destroy_dumb(&self, handle: u32) {
        let mut h = handle;
        let _ = ioctl(self.raw(), IOCTL_MODE_DESTROY_DUMB, &mut h);
    }

    pub(crate) fn add_framebuffer(&self, fb: &mut FbCmd2) -> io::Result<u32> {
        ioctl(self.raw(), IOCTL_MODE_ADDFB2, fb)?;
        Ok(fb.fb_id)
    }

    pub(crate) fn remove_framebuffer(&self, fb_id: u32) {
        let mut id = fb_id;
        let _ = ioctl(self.raw(), IOCTL_MODE_RMFB, &mut id);
    }

    /// Turns `crtc_id` off.
    pub(crate) fn disable_crtc(&self, crtc_id: u32) -> io::Result<()> {
        let mut c = ModeCrtc {
            crtc_id,
            ..Default::default()
        };
        ioctl(self.raw(), IOCTL_MODE_SETCRTC, &mut c)
    }

    pub(crate) fn set_property(
        &self,
        obj_id: u32,
        obj_type: u32,
        prop_id: u32,
        value: u64,
    ) -> io::Result<()> {
        let mut p = ObjSetProperty {
            value,
            prop_id,
            obj_id,
            obj_type,
        };
        ioctl(self.raw(), IOCTL_MODE_OBJ_SETPROPERTY, &mut p)
    }

    /// Shows `fb_id` on `crtc_id` through `connector_id` in `mode`.
    pub(crate) fn set_crtc(
        &self,
        crtc_id: u32,
        fb_id: u32,
        connector_id: u32,
        mode: &ModeInfo,
    ) -> io::Result<()> {
        let mut connectors = [connector_id];
        let mut c = ModeCrtc {
            set_connectors_ptr: ptr(&mut connectors),
            count_connectors: 1,
            crtc_id,
            fb_id,
            mode_valid: 1,
            mode: *mode,
            ..Default::default()
        };
        ioctl(self.raw(), IOCTL_MODE_SETCRTC, &mut c)
    }
}

fn cstr(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// Whether a buffer read from a DRM fd holds a vblank event.
fn has_vblank_event(mut buf: &[u8]) -> bool {
    let mut found = false;
    while buf.len() >= 8 {
        let kind = u32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let len = u32::from_ne_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
        if len < 8 || len > buf.len() {
            break;
        }
        found |= kind == EVENT_VBLANK;
        buf = &buf[len..];
    }
    found
}

/// The `/dev/dri/card*` nodes, in index order.
pub(crate) fn cards() -> Vec<PathBuf> {
    let Ok(dir) = std::fs::read_dir("/dev/dri") else {
        return Vec::new();
    };
    let mut cards: Vec<(u32, PathBuf)> = dir
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let index = name.strip_prefix("card")?.parse().ok()?;
            Some((index, e.path()))
        })
        .collect();
    cards.sort();
    cards.into_iter().map(|(_, p)| p).collect()
}

/// The render node of the same GPU as `card`, if it has one.
pub(crate) fn render_node(card: &Path) -> Option<PathBuf> {
    let name = card.file_name()?.to_str()?;
    let dir = std::fs::read_dir(format!("/sys/class/drm/{name}/device/drm")).ok()?;
    dir.filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .find(|n| n.starts_with("renderD"))
        .map(|n| PathBuf::from("/dev/dri").join(n))
}

/// A name for the GPU behind `card` that stays the same across boots: its sysfs device path.
pub(crate) fn device_key(card: &Path) -> String {
    card.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| std::fs::canonicalize(format!("/sys/class/drm/{n}/device")).ok())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| card.to_string_lossy().into_owned())
}

/// The GPU's PCI vendor id, 0 if unknown.
pub(crate) fn vendor(card: &Path) -> u32 {
    card.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| std::fs::read_to_string(format!("/sys/class/drm/{n}/device/vendor")).ok())
        .and_then(|s| u32::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_the_kernel_headers() {
        assert_eq!(IOCTL_VERSION, 0xc040_6400);
        assert_eq!(IOCTL_GEM_CLOSE, 0x4008_6409);
        assert_eq!(IOCTL_SET_CLIENT_CAP, 0x4010_640d);
        assert_eq!(IOCTL_PRIME_HANDLE_TO_FD, 0xc00c_642d);
        assert_eq!(IOCTL_WAIT_VBLANK, 0xc018_643a);
        assert_eq!(IOCTL_SET_MASTER, 0x641e);
        assert_eq!(IOCTL_DROP_MASTER, 0x641f);
        assert_eq!(IOCTL_MODE_GETRESOURCES, 0xc040_64a0);
        assert_eq!(IOCTL_MODE_GETCRTC, 0xc068_64a1);
        assert_eq!(IOCTL_MODE_SETCRTC, 0xc068_64a2);
        assert_eq!(IOCTL_MODE_GETENCODER, 0xc014_64a6);
        assert_eq!(IOCTL_MODE_GETCONNECTOR, 0xc050_64a7);
        assert_eq!(IOCTL_MODE_GETPROPERTY, 0xc040_64aa);
        assert_eq!(IOCTL_MODE_GETPROPBLOB, 0xc010_64ac);
        assert_eq!(IOCTL_MODE_RMFB, 0xc004_64af);
        assert_eq!(IOCTL_MODE_CREATE_DUMB, 0xc020_64b2);
        assert_eq!(IOCTL_MODE_MAP_DUMB, 0xc010_64b3);
        assert_eq!(IOCTL_MODE_DESTROY_DUMB, 0xc004_64b4);
        assert_eq!(IOCTL_MODE_GETPLANERESOURCES, 0xc010_64b5);
        assert_eq!(IOCTL_MODE_GETPLANE, 0xc020_64b6);
        assert_eq!(IOCTL_MODE_ADDFB2, 0xc068_64b8);
        assert_eq!(IOCTL_MODE_OBJ_GETPROPERTIES, 0xc020_64b9);
        assert_eq!(IOCTL_MODE_OBJ_SETPROPERTY, 0xc018_64ba);
        assert_eq!(IOCTL_MODE_GETFB2, 0xc068_64ce);
        assert_eq!(size_of::<ModeInfo>(), 0x44);
    }

    #[test]
    fn refresh_from_timings() {
        let mode = |clock, htotal, vtotal, flags| ModeInfo {
            clock,
            htotal,
            vtotal,
            flags,
            vrefresh: 60,
            ..Default::default()
        };
        // CEA 1080p60 and 1080p59.94.
        assert_eq!(mode(148_500, 2200, 1125, 0).refresh_mhz(), 60_000);
        assert_eq!(mode(148_352, 2200, 1125, 0).refresh_mhz(), 59_940);
        // 1080i60: fields per second.
        assert_eq!(
            mode(74_250, 2200, 1125, MODE_FLAG_INTERLACE).refresh_mhz(),
            60_000
        );
        assert_eq!(mode(0, 0, 0, 0).refresh_mhz(), 60_000);
    }

    #[test]
    fn vblank_events_are_found() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&2u32.to_ne_bytes());
        buf.extend_from_slice(&8u32.to_ne_bytes());
        assert!(!has_vblank_event(&buf));
        buf.extend_from_slice(&EVENT_VBLANK.to_ne_bytes());
        buf.extend_from_slice(&32u32.to_ne_bytes());
        buf.extend_from_slice(&[0; 24]);
        assert!(has_vblank_event(&buf));
        assert!(!has_vblank_event(&buf[..12]));
    }

    #[test]
    fn connector_names() {
        assert_eq!(connector_type_name(11), "HDMI-A");
        assert_eq!(connector_type_name(14), "eDP");
        assert_eq!(connector_type_name(99), "Unknown");
    }
}
