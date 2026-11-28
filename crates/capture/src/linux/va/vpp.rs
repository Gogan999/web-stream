//! Scanout DMA-BUF to NV12 at the stream size with VA video processing: import, crop, rotate,
//! mirror, scale, letterbox and colour conversion on the GPU.

use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::sys::*;
use super::{Config, Display, VAContextID};
use crate::layout::letterbox;
use crate::linux::kms::{Frame, FrameKey, fourcc};
use crate::{CaptureError, Result};
use ws_proto::orientation::Transform;

/// Scanout buffers kept imported: a compositor cycles through two to four.
const IMPORT_CACHE: usize = 6;

/// `VA_ROTATION_*`: quarter turns clockwise.
const VA_ROTATION_NONE: u32 = 0;
/// `VA_MIRROR_*`, applied after the rotation.
const VA_MIRROR_NONE: u32 = 0;
const VA_MIRROR_HORIZONTAL: u32 = 1;
const VA_MIRROR_VERTICAL: u32 = 2;

/// `VAProcPipelineCaps` (`va_vpp.h`) up to `mirror_flags`, with room for the rest.
#[repr(C)]
pub(crate) struct PipelineCaps {
    pipeline_flags: u32,
    filter_flags: u32,
    num_forward_references: u32,
    num_backward_references: u32,
    input_color_standards: *mut VAProcColorStandardType,
    num_input_color_standards: u32,
    output_color_standards: *mut VAProcColorStandardType,
    num_output_color_standards: u32,
    rotation_flags: u32,
    blend_flags: u32,
    mirror_flags: u32,
    rest: [u32; 64],
}

/// Rotations (`1 << VA_ROTATION_*`) and mirrors (`VA_MIRROR_*` bits) the pipeline supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Orientations {
    rotations: u32,
    mirrors: u32,
}

/// The rotation and mirror state that applies `t`, if the pipeline can.
fn va_state(t: Transform, caps: Orientations) -> Option<(u32, u32)> {
    let q = u32::from(t.quarter());
    let options: &[(u32, u32)] = if t.mirror() {
        &[(q, VA_MIRROR_HORIZONTAL), ((q + 2) % 4, VA_MIRROR_VERTICAL)]
    } else {
        &[((4 - q) % 4, VA_MIRROR_NONE)]
    };
    options.iter().copied().find(|&(rotation, mirror)| {
        (rotation == VA_ROTATION_NONE || caps.rotations & (1 << rotation) != 0)
            && (mirror == VA_MIRROR_NONE || caps.mirrors & mirror != 0)
    })
}

/// A transform the pipeline cannot apply was logged.
static UNSUPPORTED_LOGGED: AtomicBool = AtomicBool::new(false);

/// An NV12 surface the converter writes and the encoder reads.
pub(crate) struct Pooled {
    pub surface: VASurfaceID,
    /// Frames in the encoder reading it.
    pub users: u32,
}

pub(crate) struct Vpp {
    display: Arc<Display>,
    config: Config,
    context: VAContextID,
    width: u32,
    height: u32,
    orientations: Orientations,
    imported: Vec<(FrameKey, VASurfaceID)>,
    pub pool: Vec<Pooled>,
    /// The pool surface holding the newest image.
    pub current: Option<usize>,
}

/// The VA fourcc and render target format for a DRM scanout format.
pub(crate) fn va_format(drm: u32) -> Option<(u32, u32)> {
    Some(match drm {
        fourcc::XRGB8888 => (VA_FOURCC_BGRX, VA_RT_FORMAT_RGB32),
        fourcc::ARGB8888 => (VA_FOURCC_BGRA, VA_RT_FORMAT_RGB32),
        fourcc::XBGR8888 => (VA_FOURCC_RGBX, VA_RT_FORMAT_RGB32),
        fourcc::ABGR8888 => (VA_FOURCC_RGBA, VA_RT_FORMAT_RGB32),
        fourcc::XRGB2101010 => (VA_FOURCC_X2R10G10B10, VA_RT_FORMAT_RGB32_10),
        fourcc::ARGB2101010 => (VA_FOURCC_A2R10G10B10, VA_RT_FORMAT_RGB32_10),
        fourcc::XBGR2101010 => (VA_FOURCC_X2B10G10R10, VA_RT_FORMAT_RGB32_10),
        fourcc::ABGR2101010 => (VA_FOURCC_A2B10G10R10, VA_RT_FORMAT_RGB32_10),
        _ => return None,
    })
}

impl Vpp {
    /// A converter to `width` x `height` NV12 with `pool` output surfaces of `surface_w` x
    /// `surface_h`.
    pub(crate) fn new(
        display: Arc<Display>,
        width: u32,
        height: u32,
        surface_w: u32,
        surface_h: u32,
        pool: usize,
    ) -> Result<Self> {
        let config = display.create_config(VAProfileNone, VAEntrypointVideoProc, &mut [])?;
        let mut surfaces = match display.create_surfaces(
            VA_RT_FORMAT_YUV420,
            VA_FOURCC_NV12,
            surface_w,
            surface_h,
            pool,
        ) {
            Ok(s) => s,
            Err(e) => {
                display.destroy_config(&config);
                return Err(e);
            }
        };
        let context = match display.create_context(&config, surface_w, surface_h, &mut surfaces) {
            Ok(c) => c,
            Err(e) => {
                display.destroy_surfaces(&mut surfaces);
                display.destroy_config(&config);
                return Err(e);
            }
        };
        let orientations = query_orientations(&display, context);
        Ok(Self {
            display,
            config,
            context,
            width,
            height,
            orientations,
            imported: Vec::new(),
            pool: surfaces
                .into_iter()
                .map(|surface| Pooled { surface, users: 0 })
                .collect(),
            current: None,
        })
    }

    /// A pool surface neither in the encoder nor holding the newest image.
    pub(crate) fn free_surface(&self) -> Option<usize> {
        (0..self.pool.len()).find(|&i| self.pool[i].users == 0 && Some(i) != self.current)
    }

    /// Converts `frame` into pool surface `target`, which becomes the current image.
    pub(crate) fn convert(&mut self, frame: &Frame, target: usize) -> Result<()> {
        let input = self.import(frame)?;
        let c = frame.crop;
        let src = VARectangle {
            x: c.x as i16,
            y: c.y as i16,
            width: c.width as u16,
            height: c.height as u16,
        };
        let (transform, (rotation, mirror)) = match va_state(frame.transform, self.orientations) {
            Some(state) => (frame.transform, state),
            None => {
                if !UNSUPPORTED_LOGGED.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        transform = ?frame.transform,
                        "VA-API video processing cannot rotate or mirror this way; streaming unrotated"
                    );
                }
                (Transform::IDENTITY, (VA_ROTATION_NONE, VA_MIRROR_NONE))
            }
        };
        let (upright_w, upright_h) = transform.size(c.width, c.height);
        let r = letterbox(upright_w, upright_h, self.width, self.height);
        let dst = VARectangle {
            x: r.left as i16,
            y: r.top as i16,
            width: (r.right - r.left) as u16,
            height: (r.bottom - r.top) as u16,
        };
        // SAFETY: plain data; zero and null are valid for every field.
        let mut p: VAProcPipelineParameterBuffer = unsafe { std::mem::zeroed() };
        p.surface = input;
        p.surface_region = &src;
        p.surface_color_standard = VAProcColorStandardBT709;
        p.output_region = &dst;
        p.output_background_color = 0xff00_0000;
        p.output_color_standard = VAProcColorStandardBT709;
        p.filter_flags = VA_FILTER_SCALING_DEFAULT;
        p.rotation_state = rotation;
        p.mirror_state = mirror;
        p.input_color_properties.color_range = VA_SOURCE_RANGE_FULL as u8;
        p.output_color_properties = VAProcColorProperties {
            chroma_sample_location: (VA_CHROMA_SITING_HORIZONTAL_LEFT
                | VA_CHROMA_SITING_VERTICAL_CENTER) as u8,
            color_range: VA_SOURCE_RANGE_REDUCED as u8,
            colour_primaries: 1,
            transfer_characteristics: 1,
            matrix_coefficients: 1,
            reserved: [0; 3],
        };
        let d = &*self.display;
        let buffer = d.buffer(self.context, VAProcPipelineParameterBufferType, &p)?;
        d.render(self.context, self.pool[target].surface, &[buffer])?;
        self.current = Some(target);
        Ok(())
    }

    /// The scanout buffer as a VA surface, imported once per buffer.
    fn import(&mut self, frame: &Frame) -> Result<VASurfaceID> {
        let key = frame.key();
        if let Some(i) = self.imported.iter().position(|(k, _)| *k == key) {
            let entry = self.imported.remove(i);
            let id = entry.1;
            self.imported.push(entry);
            return Ok(id);
        }
        let (va_fourcc, rt_format) = va_format(frame.fourcc).ok_or_else(|| {
            CaptureError::Platform(format!(
                "scanout format {:?} is not supported",
                fourcc_str(frame.fourcc)
            ))
        })?;
        let mut desc = VADRMPRIMESurfaceDescriptor {
            fourcc: va_fourcc,
            width: frame.width,
            height: frame.height,
            num_objects: frame.objects.len() as u32,
            num_layers: 1,
            ..Default::default()
        };
        for (i, fd) in frame.objects.iter().enumerate() {
            desc.objects[i].fd = fd.as_raw_fd();
            desc.objects[i].size = dmabuf_size(fd.as_raw_fd());
            desc.objects[i].drm_format_modifier = frame.modifier;
        }
        let layer = &mut desc.layers[0];
        layer.drm_format = frame.fourcc;
        layer.num_planes = frame.planes.len() as u32;
        for (i, p) in frame.planes.iter().enumerate() {
            layer.object_index[i] = p.object as u32;
            layer.offset[i] = p.offset;
            layer.pitch[i] = p.pitch;
        }
        let id = self.display.import_surface(rt_format, &mut desc)?;
        if self.imported.len() >= IMPORT_CACHE {
            let (_, mut old) = self.imported.remove(0);
            self.display
                .destroy_surfaces(std::slice::from_mut(&mut old));
        }
        self.imported.push((key, id));
        Ok(id)
    }
}

impl Drop for Vpp {
    fn drop(&mut self) {
        let d = &self.display;
        d.destroy_context(self.context);
        let mut ids: Vec<VASurfaceID> = self.imported.drain(..).map(|(_, id)| id).collect();
        ids.extend(self.pool.iter().map(|p| p.surface));
        d.destroy_surfaces(&mut ids);
        d.destroy_config(&self.config);
    }
}

/// The rotations and mirrors the video processing pipeline on `context` supports.
fn query_orientations(display: &Display, context: VAContextID) -> Orientations {
    let none = Orientations {
        rotations: 0,
        mirrors: 0,
    };
    let Some(query) = display.lib().vaQueryVideoProcPipelineCaps else {
        return none;
    };
    // SAFETY: plain data; zero and null are valid for every field.
    let mut caps: PipelineCaps = unsafe { std::mem::zeroed() };
    let status = unsafe { query(display.raw(), context, std::ptr::null_mut(), 0, &mut caps) };
    if display
        .check(status, "vaQueryVideoProcPipelineCaps")
        .is_err()
    {
        return none;
    }
    Orientations {
        rotations: caps.rotation_flags,
        mirrors: caps.mirror_flags,
    }
}

fn dmabuf_size(fd: i32) -> u32 {
    // SAFETY: lseek on a DMA-BUF reports its size and leaves nothing to undo for importers.
    let size = unsafe { libc::lseek(fd, 0, libc::SEEK_END) };
    unsafe { libc::lseek(fd, 0, libc::SEEK_SET) };
    u32::try_from(size.max(0)).unwrap_or(u32::MAX)
}

fn fourcc_str(code: u32) -> String {
    code.to_le_bytes().iter().map(|&b| b as char).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ws_proto::orientation::{Affine, compose};

    #[test]
    fn scanout_formats_map_to_va() {
        assert_eq!(
            va_format(fourcc::XRGB8888),
            Some((VA_FOURCC_BGRX, VA_RT_FORMAT_RGB32))
        );
        assert_eq!(
            va_format(fourcc::XRGB2101010),
            Some((VA_FOURCC_X2R10G10B10, VA_RT_FORMAT_RGB32_10))
        );
        assert_eq!(va_format(u32::from_le_bytes(*b"NV12")), None);
        assert_eq!(fourcc_str(fourcc::XRGB8888), "XR24");
    }

    /// VA-API's clockwise rotation then mirror, on the unit square.
    fn va_affine(rotation: u32, mirror: u32) -> Affine {
        let cw: [Affine; 4] = [
            [[1, 0, 0], [0, 1, 0]],
            [[0, -1, 1], [1, 0, 0]],
            [[-1, 0, 1], [0, -1, 1]],
            [[0, 1, 0], [-1, 0, 1]],
        ];
        let flip: Affine = match mirror {
            VA_MIRROR_HORIZONTAL => [[-1, 0, 1], [0, 1, 0]],
            VA_MIRROR_VERTICAL => [[1, 0, 0], [0, -1, 1]],
            _ => [[1, 0, 0], [0, 1, 0]],
        };
        compose(flip, cw[rotation as usize])
    }

    #[test]
    fn va_rotation_and_mirror_apply_every_transform() {
        let all = Orientations {
            rotations: 0b1111,
            mirrors: VA_MIRROR_HORIZONTAL | VA_MIRROR_VERTICAL,
        };
        for t in Transform::all() {
            let (rotation, mirror) = va_state(t, all).expect("supported");
            assert_eq!(va_affine(rotation, mirror), t.affine(), "{t:?}");
            for mirrors in [VA_MIRROR_HORIZONTAL, VA_MIRROR_VERTICAL] {
                let only = Orientations { mirrors, ..all };
                let (rotation, mirror) = va_state(t, only).expect("one mirror is enough");
                assert_eq!(va_affine(rotation, mirror), t.affine(), "{t:?}");
            }
        }
        // VA_ROTATION_90 turns clockwise: the bottom-left corner lands top-left.
        let (rotation, _) = va_state(Transform::new(3, false), all).unwrap();
        assert_eq!(rotation, 1);
    }

    #[test]
    fn unsupported_orientations_are_refused() {
        let none = Orientations {
            rotations: 0,
            mirrors: 0,
        };
        assert_eq!(
            va_state(Transform::IDENTITY, none),
            Some((VA_ROTATION_NONE, VA_MIRROR_NONE))
        );
        for t in Transform::all().filter(|t| !t.is_identity()) {
            assert_eq!(va_state(t, none), None, "{t:?}");
        }
        let half_turn_only = Orientations {
            rotations: 1 << 2,
            mirrors: 0,
        };
        assert_eq!(
            va_state(Transform::new(2, false), half_turn_only),
            Some((2, 0))
        );
        assert_eq!(va_state(Transform::new(1, false), half_turn_only), None);
        assert_eq!(va_state(Transform::new(2, true), half_turn_only), None);
    }
}
