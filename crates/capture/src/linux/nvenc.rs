//! NVENC on Linux: the scanout is imported into OpenGL through EGL, converted to NV12 by a shader,
//! and copied on the GPU into CUDA buffers the encoder reads.

use std::ffi::c_void;
use std::sync::Arc;
use std::time::Duration;

use ws_nvenc_sys as nv;
use ws_proto::VideoConfig;

use super::cuda::{Context, DeviceBuffer, GlResource};
use super::display::Output;
use super::egl::{Converter, Egl};
use super::kms::Frame;
use crate::encode::Finished;
use crate::nvenc::{Device, NvEncoder};
use crate::{CaptureError, Result};

/// The converter's NV12 planes, registered with CUDA.
pub(crate) struct Planes {
    y: GlResource,
    uv: GlResource,
    width: u32,
    height: u32,
}

/// NVENC's view of a CUDA context.
pub(crate) struct Cuda(Arc<Context>);

/// A raw context pointer for the output thread.
struct SendContext(Arc<Context>);

// SAFETY: CUDA contexts may be made current on any thread.
unsafe impl Send for SendContext {}

impl Device for Cuda {
    type Source = Planes;
    type Buffer = DeviceBuffer;
    const DEVICE_TYPE: nv::NV_ENC_DEVICE_TYPE = nv::NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_CUDA;
    const RESOURCE_TYPE: nv::NV_ENC_INPUT_RESOURCE_TYPE =
        nv::NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR;
    const ASYNC: bool = false;

    fn handle(&self) -> *mut c_void {
        self.0.raw()
    }

    fn buffer(&self, width: u32, height: u32) -> Result<DeviceBuffer> {
        self.0.alloc_pitch(width as usize, height as usize * 3 / 2)
    }

    fn resource(buffer: &DeviceBuffer) -> (*mut c_void, u32) {
        (buffer.ptr as *mut c_void, buffer.pitch as u32)
    }

    fn copy(&self, buffer: &DeviceBuffer, src: &Planes) -> Result<()> {
        let (w, h) = (src.width as usize, src.height as usize);
        self.0
            .copy_textures(&mut [(&src.y, w, h), (&src.uv, w, h / 2)], buffer)
    }

    fn output_thread_init(&self) -> Box<dyn FnOnce() + Send> {
        let ctx = SendContext(self.0.clone());
        Box::new(move || {
            super::pipeline::raise_priority();
            ctx.0.push_forever();
        })
    }

    fn create_event() -> Result<*mut c_void> {
        Err(CaptureError::Platform(
            "NVENC completion events are Windows only".into(),
        ))
    }

    fn close_event(_: *mut c_void) {}

    fn wait_event(_: *mut c_void, _: Duration) -> bool {
        false
    }
}

/// Converter and NVENC encoder for one GPU, driven from the capture thread.
pub(crate) struct GlNvenc<M: Send + 'static> {
    encoder: NvEncoder<Cuda, M>,
    planes: Planes,
    ctx: Arc<Context>,
    converter: Converter,
    egl: Egl,
    has_image: bool,
}

impl<M: Send + 'static> GlNvenc<M> {
    pub(crate) fn new(
        output: &Output,
        cfg: &VideoConfig,
        max: (u32, u32),
        on_output: impl FnMut(Finished<M>) + Send + 'static,
    ) -> Result<Self> {
        let egl = Egl::open(&output.card)?;
        let converter = Converter::new(&egl, cfg.width, cfg.height)?;
        let ctx = Context::for_current_gl()?;
        let planes = register(&ctx, &converter)?;
        let encoder = {
            let _cur = ctx.push()?;
            NvEncoder::new(Cuda(ctx.clone()), cfg, max, on_output)?
        };
        Ok(Self {
            encoder,
            planes,
            ctx,
            converter,
            egl,
            has_image: false,
        })
    }

    pub(crate) fn convert(&mut self, frame: &Frame) -> Result<()> {
        self.egl.make_current()?;
        self.converter.convert(frame)?;
        self.has_image = true;
        Ok(())
    }

    pub(crate) fn submit(
        &mut self,
        index: u64,
        idr: bool,
        intra: bool,
        kbps: Option<u32>,
        meta: M,
    ) -> Result<()> {
        if !self.has_image {
            return Err(CaptureError::Platform("no image to encode".into()));
        }
        self.egl.make_current()?;
        let _cur = self.ctx.push()?;
        self.encoder
            .submit(&self.planes, index, idr, intra, kbps, meta)
    }

    pub(crate) fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        let ctx = self.ctx.clone();
        let _cur = ctx.push()?;
        self.encoder.resize(width, height)?;
        self.egl.make_current()?;
        self.converter.resize(width, height)?;
        self.planes = register(&self.ctx, &self.converter)?;
        self.has_image = false;
        Ok(())
    }

    pub(crate) fn can_invalidate(&self) -> bool {
        self.encoder.can_invalidate()
    }

    pub(crate) fn invalidate(&mut self, index: u64) -> Result<()> {
        let _cur = self.ctx.push()?;
        self.encoder.invalidate(index)
    }

    pub(crate) fn drain(&mut self) -> Result<()> {
        let _cur = self.ctx.push()?;
        self.encoder.drain()
    }

    pub(crate) fn bitrate_kbps(&self) -> u32 {
        self.encoder.bitrate_kbps()
    }
}

impl<M: Send + 'static> Drop for GlNvenc<M> {
    fn drop(&mut self) {
        let _ = self.egl.make_current();
        // Current until the context is destroyed.
        self.ctx.push_forever();
    }
}

fn register(ctx: &Arc<Context>, converter: &Converter) -> Result<Planes> {
    Ok(Planes {
        y: ctx.register_texture(converter.planes[0])?,
        uv: ctx.register_texture(converter.planes[1])?,
        width: converter.width,
        height: converter.height,
    })
}

/// The NVENC encoders on the NVIDIA GPU behind `card`.
pub(crate) fn probe(card: &std::path::Path) -> Option<crate::caps::EncoderInfo> {
    let device = std::fs::canonicalize(format!(
        "/sys/class/drm/{}/device",
        card.file_name()?.to_str()?
    ))
    .ok()?;
    let bus_id = device.file_name()?.to_str()?.to_owned();
    let ctx = match Context::for_pci(&bus_id) {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!("NVENC probe on {bus_id}: {e}");
            return None;
        }
    };
    let probed = match ctx.push() {
        Ok(_cur) => crate::nvenc::probe(&Cuda(ctx.clone())),
        Err(e) => Err(e),
    };
    match probed {
        Ok(codecs) if !codecs.is_empty() => Some(crate::caps::EncoderInfo {
            backend: crate::caps::EncoderBackend::Nvenc,
            adapter: format!("NVIDIA GPU {bus_id}"),
            codecs,
        }),
        Ok(_) => None,
        Err(e) => {
            tracing::debug!("NVENC probe on {bus_id}: {e}");
            None
        }
    }
}
