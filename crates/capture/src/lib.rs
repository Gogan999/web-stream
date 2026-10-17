//! Display capture and hardware video encode for the host.
//!
//! Windows path: DXGI Desktop Duplication -> GPU copy -> D3D11 video
//! processor (BGRA to NV12, scale/letterbox to the stream size) -> NVENC
//! (AMF and QSV to follow). Everything stays on the GPU until the encoded
//! bitstream is read back.
//!
//! Frames leave this crate as [`ws_proto::EncodedFrame`] on a bounded
//! channel. The pipeline never queues: it encodes the newest desktop image
//! when a frame slot comes up, and if the channel is full the encoded frame
//! is dropped and the next one is forced to be a keyframe so the decoder can
//! resume.

#[cfg_attr(not(windows), allow(dead_code))]
mod layout;
#[cfg_attr(not(windows), allow(dead_code))]
mod pacing;
#[cfg(windows)]
mod win;

use thiserror::Error;
use tokio::sync::mpsc;
use ws_proto::{DisplayId, DisplayInfo, EncodedFrame, VideoConfig};

#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("display capture is not supported on this platform")]
    Unsupported,
    #[error("display {0:?} not found")]
    DisplayNotFound(DisplayId),
    #[error("no supported hardware encoder found: {0}")]
    NoEncoder(String),
    #[error("invalid video config: {0}")]
    InvalidConfig(String),
    #[error("capture pipeline stopped")]
    Stopped,
    #[error("{0}")]
    Platform(String),
}

pub type Result<T> = std::result::Result<T, CaptureError>;

/// Lists every display the host can capture.
pub fn list_displays() -> Result<Vec<DisplayInfo>> {
    #[cfg(windows)]
    {
        win::list_displays()
    }
    #[cfg(not(windows))]
    {
        Err(CaptureError::Unsupported)
    }
}

fn validate(config: &VideoConfig) -> Result<()> {
    let bad = |m: &str| Err(CaptureError::InvalidConfig(m.into()));
    if config.width == 0 || config.height == 0 || config.width % 2 != 0 || config.height % 2 != 0 {
        return bad("width and height must be even and non-zero");
    }
    if config.width > 8192 || config.height > 8192 {
        return bad("width and height must be at most 8192");
    }
    if !(1..=240).contains(&config.fps) {
        return bad("fps must be 1..=240");
    }
    if !(100..=500_000).contains(&config.bitrate_kbps) {
        return bad("bitrate must be 100..=500000 kbps");
    }
    Ok(())
}

/// Handle to a running capture + encode pipeline.
///
/// The pipeline runs on its own thread. It ends when the handle is dropped
/// or on an unrecoverable error; either way the frame channel's sender is
/// dropped, so the receiver sees the stream close.
pub struct VideoPipeline {
    #[cfg(windows)]
    inner: win::PipelineHandle,
}

impl VideoPipeline {
    /// Starts capturing `display` and encoding at `config`'s size, rate and
    /// bitrate. Returns once the encoder is initialized. The output size
    /// stays fixed for the life of the pipeline; displays with a different
    /// aspect ratio are letterboxed.
    pub fn start(
        display: DisplayId,
        config: VideoConfig,
        frames: mpsc::Sender<EncodedFrame>,
    ) -> Result<Self> {
        validate(&config)?;
        #[cfg(windows)]
        {
            Ok(Self {
                inner: win::PipelineHandle::start(display, config, frames)?,
            })
        }
        #[cfg(not(windows))]
        {
            let _ = (display, frames);
            Err(CaptureError::Unsupported)
        }
    }

    /// Switches capture to another display without changing the stream's
    /// size or codec, so the browser keeps decoding the same track. The
    /// next frame is a keyframe.
    pub fn select_display(&self, display: DisplayId) -> Result<()> {
        #[cfg(windows)]
        {
            self.inner.select_display(display)
        }
        #[cfg(not(windows))]
        {
            let _ = display;
            Err(CaptureError::Unsupported)
        }
    }

    /// Forces the next frame to be an IDR.
    pub fn request_keyframe(&self) {
        #[cfg(windows)]
        self.inner.request_keyframe();
    }

    /// Changes the target bitrate without restarting the encoder.
    pub fn set_bitrate(&self, kbps: u32) {
        #[cfg(windows)]
        self.inner.set_bitrate(kbps.clamp(100, 500_000));
        #[cfg(not(windows))]
        let _ = kbps;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_odd_sizes() {
        let mut c = VideoConfig::default();
        assert!(validate(&c).is_ok());
        c.width = 1921;
        assert!(validate(&c).is_err());
    }
}
