//! BGRA desktop image -> NV12 at the stream size, on the GPU, using the
//! D3D11 video processor (fixed-function hardware on all three vendors).
//! Output is BT.709 limited range, which is what the encoder signals.

use std::mem::ManuallyDrop;

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_VIDEO_COLOR, D3D11_VIDEO_COLOR_0,
    D3D11_VIDEO_COLOR_RGBA, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_OPTIMAL_SPEED, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11Texture2D, ID3D11VideoContext1, ID3D11VideoDevice,
    ID3D11VideoProcessor, ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
    DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::core::Interface;

use super::{Gpu, win_err};
use crate::layout::letterbox;
use crate::{CaptureError, Result};

pub(crate) struct Converter {
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext1,
    out_w: u32,
    out_h: u32,
    fps: u32,
    /// NV12 texture the encoder reads from.
    output: ID3D11Texture2D,
    input: Option<Input>,
}

/// Everything that depends on the captured image's size and format.
struct Input {
    desc: D3D11_TEXTURE2D_DESC,
    /// Our own copy of the desktop image so the duplication frame can be
    /// released immediately.
    copy: ID3D11Texture2D,
    processor: ID3D11VideoProcessor,
    in_view: ID3D11VideoProcessorInputView,
    out_view: ID3D11VideoProcessorOutputView,
}

impl Converter {
    pub(crate) fn new(gpu: &Gpu, out_w: u32, out_h: u32, fps: u32) -> Result<Self> {
        let video_device: ID3D11VideoDevice =
            gpu.device.cast().map_err(win_err("ID3D11VideoDevice"))?;
        let video_context: ID3D11VideoContext1 = gpu
            .context
            .cast()
            .map_err(win_err("ID3D11VideoContext1 (needs Windows 10)"))?;
        let desc = D3D11_TEXTURE2D_DESC {
            Width: out_w,
            Height: out_h,
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
        let mut output = None;
        unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut output)) }
            .map_err(win_err("CreateTexture2D NV12"))?;
        Ok(Self {
            video_device,
            video_context,
            out_w,
            out_h,
            fps,
            output: output.ok_or(CaptureError::Platform("no NV12 texture".into()))?,
            input: None,
        })
    }

    /// The NV12 texture written by [`Self::convert`].
    pub(crate) fn output(&self) -> &ID3D11Texture2D {
        &self.output
    }

    /// Copies `frame` and converts it into [`Self::output`].
    pub(crate) fn convert(&mut self, gpu: &Gpu, frame: &ID3D11Texture2D) -> Result<()> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { frame.GetDesc(&mut desc) };
        let stale = self.input.as_ref().is_none_or(|i| {
            i.desc.Width != desc.Width
                || i.desc.Height != desc.Height
                || i.desc.Format != desc.Format
        });
        if stale {
            self.input = Some(self.make_input(gpu, desc)?);
        }
        let input = self.input.as_ref().expect("input just created");

        unsafe {
            gpu.context.CopyResource(&input.copy, frame);
            let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: ManuallyDrop::new(Some(input.in_view.clone())),
                ..Default::default()
            };
            let streams = [stream];
            let result = self.video_context.VideoProcessorBlt(
                &input.processor,
                &input.out_view,
                0,
                &streams,
            );
            // The stream struct holds an extra reference we must release.
            let [mut stream] = streams;
            ManuallyDrop::drop(&mut stream.pInputSurface);
            result.map_err(win_err("VideoProcessorBlt"))
        }
    }

    fn make_input(&self, gpu: &Gpu, frame_desc: D3D11_TEXTURE2D_DESC) -> Result<Input> {
        let (in_w, in_h) = (frame_desc.Width, frame_desc.Height);
        let copy_desc = D3D11_TEXTURE2D_DESC {
            MipLevels: 1,
            ArraySize: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
            ..frame_desc
        };
        let mut copy = None;
        unsafe {
            gpu.device
                .CreateTexture2D(&copy_desc, None, Some(&mut copy))
        }
        .map_err(win_err("CreateTexture2D desktop copy"))?;
        let copy = copy.ok_or(CaptureError::Platform("no copy texture".into()))?;

        let rate = DXGI_RATIONAL {
            Numerator: self.fps,
            Denominator: 1,
        };
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: in_w,
            InputHeight: in_h,
            OutputFrameRate: rate,
            OutputWidth: self.out_w,
            OutputHeight: self.out_h,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        let vd = &self.video_device;
        let enumerator = unsafe { vd.CreateVideoProcessorEnumerator(&content) }
            .map_err(win_err("CreateVideoProcessorEnumerator"))?;
        let processor = unsafe { vd.CreateVideoProcessor(&enumerator, 0) }
            .map_err(win_err("CreateVideoProcessor"))?;

        let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };
        let mut in_view = None;
        unsafe {
            vd.CreateVideoProcessorInputView(&copy, &enumerator, &in_desc, Some(&mut in_view))
        }
        .map_err(win_err("CreateVideoProcessorInputView"))?;
        let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut out_view = None;
        unsafe {
            vd.CreateVideoProcessorOutputView(
                &self.output,
                &enumerator,
                &out_desc,
                Some(&mut out_view),
            )
        }
        .map_err(win_err("CreateVideoProcessorOutputView"))?;

        let dest = letterbox(in_w, in_h, self.out_w, self.out_h);
        let to_rect = |l: u32, t: u32, r: u32, b: u32| RECT {
            left: l as i32,
            top: t as i32,
            right: r as i32,
            bottom: b as i32,
        };
        let vc = &self.video_context;
        unsafe {
            vc.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            vc.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            vc.VideoProcessorSetStreamSourceRect(
                &processor,
                0,
                true,
                Some(&to_rect(0, 0, in_w, in_h)),
            );
            vc.VideoProcessorSetStreamDestRect(
                &processor,
                0,
                true,
                Some(&to_rect(dest.left, dest.top, dest.right, dest.bottom)),
            );
            vc.VideoProcessorSetOutputTargetRect(
                &processor,
                true,
                Some(&to_rect(0, 0, self.out_w, self.out_h)),
            );
            let black = D3D11_VIDEO_COLOR {
                Anonymous: D3D11_VIDEO_COLOR_0 {
                    RGBA: D3D11_VIDEO_COLOR_RGBA {
                        R: 0.0,
                        G: 0.0,
                        B: 0.0,
                        A: 1.0,
                    },
                },
            };
            vc.VideoProcessorSetOutputBackgroundColor(&processor, false, &black);
            vc.VideoProcessorSetStreamColorSpace1(
                &processor,
                0,
                DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
            );
            vc.VideoProcessorSetOutputColorSpace1(
                &processor,
                DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
            );
        }
        tracing::debug!(
            in_w,
            in_h,
            out_w = self.out_w,
            out_h = self.out_h,
            ?dest,
            "video processor ready"
        );

        Ok(Input {
            desc: frame_desc,
            copy,
            processor,
            in_view: in_view.ok_or(CaptureError::Platform("no input view".into()))?,
            out_view: out_view.ok_or(CaptureError::Platform("no output view".into()))?,
        })
    }
}
