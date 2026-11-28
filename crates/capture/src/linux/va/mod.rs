//! VA-API, loaded at run time from `libva.so.2` and `libva-drm.so.2`.

mod av1;
mod bits;
pub(crate) mod encoder;
mod h264;
mod hevc;
#[allow(clippy::all, clippy::pedantic)]
pub(crate) mod sys;
mod vpp;

use std::ffi::{CStr, c_char, c_int, c_uint, c_void};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::Path;
use std::sync::{Arc, OnceLock};

use sys::*;

use crate::caps::{CodecSupport, EncoderBackend, EncoderInfo};
use crate::{CaptureError, Result};
use ws_proto::VideoCodec;

/// The libva entry points used here, with signatures from `va.h` and `va_drm.h`.
#[allow(non_snake_case)]
pub(crate) struct Lib {
    vaGetDisplayDRM: unsafe extern "C" fn(fd: c_int) -> VADisplay,
    vaInitialize: unsafe extern "C" fn(VADisplay, *mut c_int, *mut c_int) -> VAStatus,
    vaTerminate: unsafe extern "C" fn(VADisplay) -> VAStatus,
    vaErrorStr: unsafe extern "C" fn(VAStatus) -> *const c_char,
    vaQueryVendorString: unsafe extern "C" fn(VADisplay) -> *const c_char,
    vaSetErrorCallback:
        unsafe extern "C" fn(VADisplay, VAMessageCallback, *mut c_void) -> VAMessageCallback,
    vaSetInfoCallback:
        unsafe extern "C" fn(VADisplay, VAMessageCallback, *mut c_void) -> VAMessageCallback,
    vaMaxNumProfiles: unsafe extern "C" fn(VADisplay) -> c_int,
    vaMaxNumEntrypoints: unsafe extern "C" fn(VADisplay) -> c_int,
    vaQueryConfigProfiles: unsafe extern "C" fn(VADisplay, *mut VAProfile, *mut c_int) -> VAStatus,
    vaQueryConfigEntrypoints:
        unsafe extern "C" fn(VADisplay, VAProfile, *mut VAEntrypoint, *mut c_int) -> VAStatus,
    vaGetConfigAttributes: unsafe extern "C" fn(
        VADisplay,
        VAProfile,
        VAEntrypoint,
        *mut VAConfigAttrib,
        c_int,
    ) -> VAStatus,
    vaCreateConfig: unsafe extern "C" fn(
        VADisplay,
        VAProfile,
        VAEntrypoint,
        *mut VAConfigAttrib,
        c_int,
        *mut VAConfigID,
    ) -> VAStatus,
    vaDestroyConfig: unsafe extern "C" fn(VADisplay, VAConfigID) -> VAStatus,
    vaCreateSurfaces: unsafe extern "C" fn(
        VADisplay,
        c_uint,
        c_uint,
        c_uint,
        *mut VASurfaceID,
        c_uint,
        *mut VASurfaceAttrib,
        c_uint,
    ) -> VAStatus,
    vaDestroySurfaces: unsafe extern "C" fn(VADisplay, *mut VASurfaceID, c_int) -> VAStatus,
    vaCreateContext: unsafe extern "C" fn(
        VADisplay,
        VAConfigID,
        c_int,
        c_int,
        c_int,
        *mut VASurfaceID,
        c_int,
        *mut VAContextID,
    ) -> VAStatus,
    vaDestroyContext: unsafe extern "C" fn(VADisplay, VAContextID) -> VAStatus,
    vaCreateBuffer: unsafe extern "C" fn(
        VADisplay,
        VAContextID,
        VABufferType,
        c_uint,
        c_uint,
        *mut c_void,
        *mut VABufferID,
    ) -> VAStatus,
    vaDestroyBuffer: unsafe extern "C" fn(VADisplay, VABufferID) -> VAStatus,
    vaMapBuffer: unsafe extern "C" fn(VADisplay, VABufferID, *mut *mut c_void) -> VAStatus,
    vaUnmapBuffer: unsafe extern "C" fn(VADisplay, VABufferID) -> VAStatus,
    vaBeginPicture: unsafe extern "C" fn(VADisplay, VAContextID, VASurfaceID) -> VAStatus,
    vaRenderPicture:
        unsafe extern "C" fn(VADisplay, VAContextID, *mut VABufferID, c_int) -> VAStatus,
    vaEndPicture: unsafe extern "C" fn(VADisplay, VAContextID) -> VAStatus,
    vaSyncSurface: unsafe extern "C" fn(VADisplay, VASurfaceID) -> VAStatus,
    /// libva 2.9 and later.
    vaSyncBuffer: Option<unsafe extern "C" fn(VADisplay, VABufferID, u64) -> VAStatus>,
}

pub(crate) type VAConfigID = u32;
pub(crate) type VAContextID = u32;

/// libva and libva-drm, loaded once per process.
pub(crate) fn lib() -> Result<&'static Lib> {
    static LIB: OnceLock<std::result::Result<Lib, String>> = OnceLock::new();
    LIB.get_or_init(load)
        .as_ref()
        .map_err(|e| CaptureError::NoEncoder(e.clone()))
}

fn load() -> std::result::Result<Lib, String> {
    // SAFETY: libva's initializers have no preconditions.
    let va = unsafe { libloading::Library::new("libva.so.2") }
        .map_err(|e| format!("VA-API not installed (libva.so.2): {e}"))?;
    let drm = unsafe { libloading::Library::new("libva-drm.so.2") }
        .map_err(|e| format!("VA-API DRM support not installed (libva-drm.so.2): {e}"))?;
    macro_rules! sym {
        ($lib:expr, $name:ident) => {
            // SAFETY: the field's type is the function's signature in va.h / va_drm.h.
            *unsafe { $lib.get(concat!(stringify!($name), "\0").as_bytes()) }
                .map_err(|e| format!("{}: {e}", stringify!($name)))?
        };
    }
    let lib = Lib {
        vaGetDisplayDRM: sym!(drm, vaGetDisplayDRM),
        vaInitialize: sym!(va, vaInitialize),
        vaTerminate: sym!(va, vaTerminate),
        vaErrorStr: sym!(va, vaErrorStr),
        vaQueryVendorString: sym!(va, vaQueryVendorString),
        vaSetErrorCallback: sym!(va, vaSetErrorCallback),
        vaSetInfoCallback: sym!(va, vaSetInfoCallback),
        vaMaxNumProfiles: sym!(va, vaMaxNumProfiles),
        vaMaxNumEntrypoints: sym!(va, vaMaxNumEntrypoints),
        vaQueryConfigProfiles: sym!(va, vaQueryConfigProfiles),
        vaQueryConfigEntrypoints: sym!(va, vaQueryConfigEntrypoints),
        vaGetConfigAttributes: sym!(va, vaGetConfigAttributes),
        vaCreateConfig: sym!(va, vaCreateConfig),
        vaDestroyConfig: sym!(va, vaDestroyConfig),
        vaCreateSurfaces: sym!(va, vaCreateSurfaces),
        vaDestroySurfaces: sym!(va, vaDestroySurfaces),
        vaCreateContext: sym!(va, vaCreateContext),
        vaDestroyContext: sym!(va, vaDestroyContext),
        vaCreateBuffer: sym!(va, vaCreateBuffer),
        vaDestroyBuffer: sym!(va, vaDestroyBuffer),
        vaMapBuffer: sym!(va, vaMapBuffer),
        vaUnmapBuffer: sym!(va, vaUnmapBuffer),
        vaBeginPicture: sym!(va, vaBeginPicture),
        vaRenderPicture: sym!(va, vaRenderPicture),
        vaEndPicture: sym!(va, vaEndPicture),
        vaSyncSurface: sym!(va, vaSyncSurface),
        // SAFETY: as above.
        vaSyncBuffer: unsafe { va.get(b"vaSyncBuffer\0") }.ok().map(|s| *s),
    };
    // Both stay loaded for the life of the process.
    std::mem::forget(va);
    std::mem::forget(drm);
    Ok(lib)
}

unsafe extern "C" fn quiet(_: *mut c_void, _: *const c_char) {}

/// An initialized VA display on a DRM node.
pub(crate) struct Display {
    lib: &'static Lib,
    dpy: VADisplay,
    _fd: OwnedFd,
    pub vendor: String,
}

// SAFETY: libva displays may be used from several threads; drivers lock internally.
unsafe impl Send for Display {}
unsafe impl Sync for Display {}

impl Display {
    pub(crate) fn open(node: &Path) -> Result<Arc<Self>> {
        let lib = lib()?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(node)
            .map_err(|e| CaptureError::NoEncoder(format!("open {}: {e}", node.display())))?;
        let fd: OwnedFd = file.into();
        // SAFETY: `fd` outlives the display (dropped after vaTerminate).
        let dpy = unsafe { (lib.vaGetDisplayDRM)(fd.as_raw_fd()) };
        if dpy.is_null() {
            return Err(CaptureError::NoEncoder(format!(
                "no VA display on {}",
                node.display()
            )));
        }
        unsafe {
            (lib.vaSetErrorCallback)(dpy, Some(quiet), std::ptr::null_mut());
            (lib.vaSetInfoCallback)(dpy, Some(quiet), std::ptr::null_mut());
        }
        let (mut major, mut minor) = (0, 0);
        let status = unsafe { (lib.vaInitialize)(dpy, &mut major, &mut minor) };
        if status != VA_STATUS_SUCCESS as VAStatus {
            unsafe { (lib.vaTerminate)(dpy) };
            return Err(CaptureError::NoEncoder(format!(
                "VA-API driver failed to initialize on {}: {}",
                node.display(),
                error_str(lib, status)
            )));
        }
        let vendor = unsafe { (lib.vaQueryVendorString)(dpy) };
        let vendor = if vendor.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(vendor) }
                .to_string_lossy()
                .into_owned()
        };
        Ok(Arc::new(Self {
            lib,
            dpy,
            _fd: fd,
            vendor,
        }))
    }

    pub(crate) fn lib(&self) -> &'static Lib {
        self.lib
    }

    pub(crate) fn raw(&self) -> VADisplay {
        self.dpy
    }

    pub(crate) fn check(&self, status: VAStatus, what: &str) -> Result<()> {
        if status == VA_STATUS_SUCCESS as VAStatus {
            Ok(())
        } else {
            Err(CaptureError::Platform(format!(
                "{what}: {}",
                error_str(self.lib, status)
            )))
        }
    }

    pub(crate) fn profiles(&self) -> Vec<VAProfile> {
        let max = unsafe { (self.lib.vaMaxNumProfiles)(self.dpy) }.max(0);
        let mut list = vec![0; max as usize];
        let mut n = 0;
        let status =
            unsafe { (self.lib.vaQueryConfigProfiles)(self.dpy, list.as_mut_ptr(), &mut n) };
        if status != VA_STATUS_SUCCESS as VAStatus {
            return Vec::new();
        }
        list.truncate(n.clamp(0, max) as usize);
        list
    }

    pub(crate) fn entrypoints(&self, profile: VAProfile) -> Vec<VAEntrypoint> {
        let max = unsafe { (self.lib.vaMaxNumEntrypoints)(self.dpy) }.max(0);
        let mut list = vec![0; max as usize];
        let mut n = 0;
        let status = unsafe {
            (self.lib.vaQueryConfigEntrypoints)(self.dpy, profile, list.as_mut_ptr(), &mut n)
        };
        if status != VA_STATUS_SUCCESS as VAStatus {
            return Vec::new();
        }
        list.truncate(n.clamp(0, max) as usize);
        list
    }

    /// The value of one config attribute, `None` when the driver does not report it.
    pub(crate) fn attribute(
        &self,
        profile: VAProfile,
        entrypoint: VAEntrypoint,
        kind: VAConfigAttribType,
    ) -> Option<u32> {
        let mut attrib = VAConfigAttrib {
            type_: kind,
            value: 0,
        };
        let status = unsafe {
            (self.lib.vaGetConfigAttributes)(self.dpy, profile, entrypoint, &mut attrib, 1)
        };
        (status == VA_STATUS_SUCCESS as VAStatus && attrib.value != VA_ATTRIB_NOT_SUPPORTED)
            .then_some(attrib.value)
    }

    pub(crate) fn create_config(
        &self,
        profile: VAProfile,
        entrypoint: VAEntrypoint,
        attribs: &mut [VAConfigAttrib],
    ) -> Result<Config> {
        let mut id = 0;
        let status = unsafe {
            (self.lib.vaCreateConfig)(
                self.dpy,
                profile,
                entrypoint,
                attribs.as_mut_ptr(),
                attribs.len() as c_int,
                &mut id,
            )
        };
        self.check(status, "vaCreateConfig")?;
        Ok(Config { id })
    }

    pub(crate) fn destroy_config(&self, config: &Config) {
        unsafe { (self.lib.vaDestroyConfig)(self.dpy, config.id) };
    }

    /// Surfaces of `format` (`VA_RT_FORMAT_*`) and `fourcc` (`VA_FOURCC_*`, 0 for the driver's choice).
    pub(crate) fn create_surfaces(
        &self,
        format: u32,
        fourcc: u32,
        width: u32,
        height: u32,
        count: usize,
    ) -> Result<Vec<VASurfaceID>> {
        let mut ids = vec![VA_INVALID_SURFACE; count];
        let mut attrib = int_attrib(VASurfaceAttribPixelFormat, fourcc as i32);
        let (attribs, n) = if fourcc != 0 {
            (&mut attrib as *mut VASurfaceAttrib, 1)
        } else {
            (std::ptr::null_mut(), 0)
        };
        let status = unsafe {
            (self.lib.vaCreateSurfaces)(
                self.dpy,
                format,
                width,
                height,
                ids.as_mut_ptr(),
                count as c_uint,
                attribs,
                n,
            )
        };
        self.check(status, "vaCreateSurfaces")?;
        Ok(ids)
    }

    /// Wraps a DMA-BUF described by `desc` as a surface.
    pub(crate) fn import_surface(
        &self,
        format: u32,
        desc: &mut VADRMPRIMESurfaceDescriptor,
    ) -> Result<VASurfaceID> {
        let mut attribs = [
            int_attrib(
                VASurfaceAttribMemoryType,
                VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2 as i32,
            ),
            VASurfaceAttrib {
                type_: VASurfaceAttribExternalBufferDescriptor,
                flags: VA_SURFACE_ATTRIB_SETTABLE,
                value: VAGenericValue {
                    type_: VAGenericValueTypePointer,
                    value: _VAGenericValue__bindgen_ty_1 {
                        p: std::ptr::from_mut(desc).cast(),
                    },
                },
            },
        ];
        let mut id = VA_INVALID_SURFACE;
        let status = unsafe {
            (self.lib.vaCreateSurfaces)(
                self.dpy,
                format,
                desc.width,
                desc.height,
                &mut id,
                1,
                attribs.as_mut_ptr(),
                attribs.len() as c_uint,
            )
        };
        self.check(status, "vaCreateSurfaces (DMA-BUF import)")?;
        Ok(id)
    }

    pub(crate) fn destroy_surfaces(&self, ids: &mut [VASurfaceID]) {
        if !ids.is_empty() {
            unsafe { (self.lib.vaDestroySurfaces)(self.dpy, ids.as_mut_ptr(), ids.len() as c_int) };
        }
    }

    pub(crate) fn create_context(
        &self,
        config: &Config,
        width: u32,
        height: u32,
        targets: &mut [VASurfaceID],
    ) -> Result<VAContextID> {
        let mut id = 0;
        let status = unsafe {
            (self.lib.vaCreateContext)(
                self.dpy,
                config.id,
                width as c_int,
                height as c_int,
                VA_PROGRESSIVE as c_int,
                targets.as_mut_ptr(),
                targets.len() as c_int,
                &mut id,
            )
        };
        self.check(status, "vaCreateContext")?;
        Ok(id)
    }

    pub(crate) fn destroy_context(&self, id: VAContextID) {
        unsafe { (self.lib.vaDestroyContext)(self.dpy, id) };
    }

    /// A parameter buffer holding a copy of `data`.
    pub(crate) fn buffer<T>(
        &self,
        context: VAContextID,
        kind: VABufferType,
        data: &T,
    ) -> Result<Buffer<'_>> {
        self.raw_buffer(
            context,
            kind,
            std::ptr::from_ref(data).cast_mut().cast(),
            size_of::<T>(),
        )
    }

    /// A buffer of `size` bytes, holding a copy of `data` unless it is null.
    pub(crate) fn raw_buffer(
        &self,
        context: VAContextID,
        kind: VABufferType,
        data: *mut c_void,
        size: usize,
    ) -> Result<Buffer<'_>> {
        let mut id = VA_INVALID_ID;
        let status = unsafe {
            (self.lib.vaCreateBuffer)(self.dpy, context, kind, size as c_uint, 1, data, &mut id)
        };
        self.check(status, "vaCreateBuffer")?;
        Ok(Buffer { display: self, id })
    }

    /// A misc parameter buffer: the type word followed by `data`.
    pub(crate) fn misc_buffer<T: Copy>(
        &self,
        context: VAContextID,
        kind: VAEncMiscParameterType,
        data: &T,
    ) -> Result<Buffer<'_>> {
        let mut bytes = vec![0u32; (4 + size_of::<T>()).div_ceil(4)];
        bytes[0] = kind;
        // SAFETY: copies a plain C struct into the words after the type.
        unsafe {
            std::ptr::copy_nonoverlapping(
                std::ptr::from_ref(data).cast::<u8>(),
                bytes.as_mut_ptr().add(1).cast::<u8>(),
                size_of::<T>(),
            )
        };
        self.raw_buffer(
            context,
            VAEncMiscParameterBufferType,
            bytes.as_mut_ptr().cast(),
            4 + size_of::<T>(),
        )
    }

    /// Runs one picture on `context` into `target` with `buffers`.
    pub(crate) fn render(
        &self,
        context: VAContextID,
        target: VASurfaceID,
        buffers: &[Buffer<'_>],
    ) -> Result<()> {
        let mut ids: Vec<VABufferID> = buffers.iter().map(|b| b.id).collect();
        let l = self.lib;
        self.check(
            unsafe { (l.vaBeginPicture)(self.dpy, context, target) },
            "vaBeginPicture",
        )?;
        let rendered = self.check(
            unsafe { (l.vaRenderPicture)(self.dpy, context, ids.as_mut_ptr(), ids.len() as c_int) },
            "vaRenderPicture",
        );
        let ended = self.check(
            unsafe { (l.vaEndPicture)(self.dpy, context) },
            "vaEndPicture",
        );
        rendered.and(ended)
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        unsafe { (self.lib.vaTerminate)(self.dpy) };
    }
}

pub(crate) struct Config {
    pub id: VAConfigID,
}

/// A VA buffer, destroyed on drop.
pub(crate) struct Buffer<'a> {
    display: &'a Display,
    pub id: VABufferID,
}

impl Drop for Buffer<'_> {
    fn drop(&mut self) {
        unsafe { (self.display.lib.vaDestroyBuffer)(self.display.dpy, self.id) };
    }
}

fn int_attrib(kind: VASurfaceAttribType, value: i32) -> VASurfaceAttrib {
    VASurfaceAttrib {
        type_: kind,
        flags: VA_SURFACE_ATTRIB_SETTABLE,
        value: VAGenericValue {
            type_: VAGenericValueTypeInteger,
            value: _VAGenericValue__bindgen_ty_1 { i: value },
        },
    }
}

fn error_str(lib: &Lib, status: VAStatus) -> String {
    let s = unsafe { (lib.vaErrorStr)(status) };
    let text = if s.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned()
    };
    format!("{text} (VAStatus {status:#x})")
}

/// The profile and entrypoint used for `codec`, preferring the low-power encoder.
pub(crate) fn encode_entry(
    display: &Display,
    codec: VideoCodec,
) -> Option<(VAProfile, VAEntrypoint)> {
    let profiles = display.profiles();
    let wanted: &[VAProfile] = match codec {
        VideoCodec::H264 => &[
            VAProfileH264High,
            VAProfileH264Main,
            VAProfileH264ConstrainedBaseline,
        ],
        VideoCodec::H265 => &[VAProfileHEVCMain],
        VideoCodec::Av1 => &[VAProfileAV1Profile0],
    };
    wanted
        .iter()
        .filter(|p| profiles.contains(p))
        .find_map(|&p| {
            let entries = display.entrypoints(p);
            [VAEntrypointEncSliceLP, VAEntrypointEncSlice]
                .into_iter()
                .find(|e| entries.contains(e))
                .filter(|&e| {
                    display
                        .attribute(p, e, VAConfigAttribRTFormat)
                        .is_some_and(|f| f & VA_RT_FORMAT_YUV420 != 0)
                        && display
                            .attribute(p, e, VAConfigAttribRateControl)
                            .is_some_and(|rc| rc & VA_RC_CBR != 0)
                })
                .map(|e| (p, e))
        })
}

/// The VA-API encoders on the GPU behind `node`.
pub(crate) fn probe(node: &Path, adapter: &str) -> Option<EncoderInfo> {
    let display = Display::open(node).ok()?;
    let processes = display
        .entrypoints(VAProfileNone)
        .contains(&VAEntrypointVideoProc);
    if !processes {
        return None;
    }
    let codecs: Vec<CodecSupport> = [VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1]
        .into_iter()
        .filter_map(|codec| {
            let (p, e) = encode_entry(&display, codec)?;
            let max_width = display
                .attribute(p, e, VAConfigAttribMaxPictureWidth)
                .unwrap_or(4096);
            let max_height = display
                .attribute(p, e, VAConfigAttribMaxPictureHeight)
                .unwrap_or(4096);
            if codec == VideoCodec::Av1 && !av1::supported(&display, p, e) {
                return None;
            }
            Some(CodecSupport {
                codec,
                max_width,
                max_height,
                repair: false,
            })
        })
        .collect();
    (!codecs.is_empty()).then(|| EncoderInfo {
        backend: EncoderBackend::Vaapi,
        adapter: if display.vendor.is_empty() {
            adapter.to_owned()
        } else {
            format!("{adapter} ({})", display.vendor)
        },
        codecs,
    })
}

#[cfg(test)]
mod tests {
    use super::av1::tests::av1;
    use super::h264::H264;
    use super::h264::tests::stream;
    use super::hevc::{Hevc, Tools};
    use super::*;

    /// Runs ffmpeg's `trace_headers` over `data` and returns its log, or `None` without ffmpeg.
    fn trace(format: &str, name: &str, data: &[u8]) -> Option<String> {
        let dir =
            std::env::temp_dir().join(format!("ws-capture-headers-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join(name);
        std::fs::write(&path, data).ok()?;
        let out = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "trace", "-f", format, "-i"])
            .arg(&path)
            .args(["-c", "copy", "-bsf:v", "trace_headers", "-f", "null", "-"])
            .output()
            .ok()?;
        if let Some(keep) = std::env::var_os("WS_HEADER_DUMP") {
            let _ = std::fs::copy(&path, std::path::Path::new(&keep).join(name));
        }
        let _ = std::fs::remove_dir_all(&dir);
        Some(String::from_utf8_lossy(&out.stderr).into_owned())
    }

    fn assert_clean(log: &str, expect: &[&str]) {
        for line in log.lines().filter(|l| l.starts_with("[trace_headers")) {
            let l = line.to_ascii_lowercase();
            assert!(
                l.contains(" = ")
                    || !(l.contains("invalid")
                        || l.contains("error")
                        || l.contains("out of range")
                        || l.contains("failed")),
                "ffmpeg: {line}\n{log}"
            );
        }
        for e in expect {
            assert!(log.contains(e), "missing {e:?} in\n{log}");
        }
    }

    #[test]
    fn ffmpeg_parses_h264_parameter_sets() {
        for (w, h, profile) in [
            (1920, 1080, VAProfileH264High),
            (1366, 768, VAProfileH264Main),
            (640, 360, VAProfileH264ConstrainedBaseline),
        ] {
            let c = H264::new(profile, &stream(w, h), VA_ENC_PACKED_HEADER_SEQUENCE);
            let mut data = c.sps();
            data.extend(c.pps());
            // An IDR slice header with a few bytes of slice data.
            let mut s = bits::BitWriter::new();
            s.ue(0);
            s.ue(7);
            s.ue(0);
            s.u(16, 0);
            s.ue(0);
            s.flag(false);
            s.flag(false);
            s.se(0);
            s.ue(0);
            s.se(0);
            s.se(0);
            s.trailing();
            let mut slice = s.into_bytes();
            slice.extend([0x55; 16]);
            data.extend(bits::nal(&[0x65], &slice));
            let Some(log) = trace("h264", "s.h264", &data) else {
                eprintln!("skipped: no ffmpeg");
                return;
            };
            assert_clean(&log, &["max_dec_frame_buffering", "slice_qp_delta"]);
        }
    }

    #[test]
    fn ffmpeg_parses_hevc_parameter_sets() {
        let tools = Tools::new(
            Some((1 << 4) | (1 << 6) | (1 << 10) | (1 << 20) | (2 << 22)),
            None,
            None,
            false,
        );
        let gpb = Tools::new(
            Some(1 << 6),
            None,
            Some(VA_PREDICTION_DIRECTION_BI_NOT_EMPTY),
            false,
        );
        for (w, h, tools) in [
            (1920, 1080, tools),
            (2560, 1440, Tools::new(None, None, None, false)),
            (1280, 720, gpb),
        ] {
            let c = Hevc::new(&stream(w, h), tools, VA_ENC_PACKED_HEADER_SEQUENCE);
            let mut data = c.vps();
            data.extend(c.sps());
            data.extend(c.pps());
            for (idr, poc) in [(true, 0), (false, 1), (false, 2)] {
                let (mut slice, _) = c.slice_header(idr, poc);
                slice.extend([0x55; 16]);
                data.extend(slice);
            }
            let Some(log) = trace("hevc", "s.hevc", &data) else {
                eprintln!("skipped: no ffmpeg");
                return;
            };
            assert_clean(
                &log,
                &[
                    "log2_max_mv_length_vertical",
                    "pps_extension_present_flag",
                    "slice_qp_delta",
                ],
            );
        }
    }

    #[test]
    fn ffmpeg_parses_av1_headers() {
        for (w, h) in [(1920, 1080), (7680, 4320)] {
            let a = av1(w, h);
            let mut data = vec![0x12, 0x00];
            data.extend(a.frame_header(true, 0).data);
            for i in 1..3 {
                data.extend([0x12, 0x00]);
                data.extend(a.frame_header(false, i).data);
            }
            let Some(log) = trace("obu", "s.obu", &data) else {
                eprintln!("skipped: no ffmpeg");
                return;
            };
            assert_clean(&log, &["Sequence Header", "Frame Header", "is_global[7]"]);
        }
    }
}
