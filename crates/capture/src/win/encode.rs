//! What every hardware encoder backend shares, and the encoder the pipeline holds.

use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Device,
    ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use ws_proto::VideoConfig;

use super::Gpu;
use super::mf::MfEncoder;
use super::nvenc::{D3d, NvEncoder};
use crate::caps::EncoderBackend;
use crate::{CaptureError, Result};

pub(crate) use crate::encode::{EncodeTiming, Encoded, Finished};
pub(crate) use crate::nvenc::{FAILED_SLOT, FRAME_TIMEOUT, SLOTS, source_qp};

/// An NV12 texture the encoder can read.
pub(crate) fn nv12_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .map_err(|e| CaptureError::Platform(format!("CreateTexture2D NV12: {e}")))?;
    texture.ok_or(CaptureError::Platform("no NV12 texture".into()))
}

/// A hardware encoder of any backend, behind the calls the capture pipeline makes.
pub(crate) enum Encoder<M: Send + 'static> {
    Nvenc(NvEncoder<M>),
    /// With the context the frames are copied in on.
    MediaFoundation(MfEncoder<M>, ID3D11DeviceContext),
}

macro_rules! each {
    ($self:expr, $e:ident => $body:expr) => {
        match $self {
            Encoder::Nvenc($e) => $body,
            Encoder::MediaFoundation($e, _) => $body,
        }
    };
}

impl<M: Send + 'static> Encoder<M> {
    /// Opens `backend` on `gpu` for `cfg`'s stream size.
    pub(crate) fn open(
        backend: EncoderBackend,
        gpu: &Gpu,
        cfg: &VideoConfig,
        max: (u32, u32),
        on_output: impl FnMut(Finished<M>) + Send + 'static,
    ) -> Result<Self> {
        match backend {
            EncoderBackend::Nvenc => {
                NvEncoder::new(D3d::new(gpu), cfg, max, on_output).map(Self::Nvenc)
            }
            EncoderBackend::MediaFoundation => MfEncoder::new(gpu, cfg, on_output)
                .map(|e| Self::MediaFoundation(e, gpu.context.clone())),
            EncoderBackend::Vaapi => Err(CaptureError::NoEncoder(
                "VA-API is not available on Windows".into(),
            )),
        }
    }

    /// Queues `src` for encoding and returns; the frame comes out on the output thread with `meta`.
    pub(crate) fn submit(
        &mut self,
        src: &ID3D11Texture2D,
        timestamp: u64,
        force_idr: bool,
        intra: bool,
        kbps: Option<u32>,
        meta: M,
    ) -> Result<()> {
        match self {
            Encoder::Nvenc(e) => e.submit(src, timestamp, force_idr, intra, kbps, meta),
            Encoder::MediaFoundation(e, context) => {
                e.submit(context, src, timestamp, force_idr, intra, kbps, meta)
            }
        }
    }

    /// Changes the encoded size in place; the next frame is an IDR.
    pub(crate) fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        each!(self, e => e.resize(width, height))
    }

    /// Stops the encoder referencing the frame encoded with `timestamp`.
    pub(crate) fn invalidate(&mut self, timestamp: u64) -> Result<()> {
        each!(self, e => e.invalidate(timestamp))
    }

    pub(crate) fn can_invalidate(&self) -> bool {
        each!(self, e => e.can_invalidate())
    }

    /// Waits until every frame in the encoder has come out.
    pub(crate) fn drain(&mut self) -> Result<()> {
        each!(self, e => e.drain())
    }

    /// The bitrate the encoder is running at.
    pub(crate) fn bitrate_kbps(&self) -> u32 {
        each!(self, e => e.bitrate_kbps())
    }
}
