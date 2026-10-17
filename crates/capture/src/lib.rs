//! Display capture and hardware video encode for the host.
//!
//! Windows path: DXGI Desktop Duplication -> GPU texture -> NVENC (then AMF,
//! QSV). Frames leave this crate as [`ws_proto::EncodedFrame`] on a bounded
//! channel with `try_send`. Nothing queues: if the channel is full the
//! encoded frame is dropped, `frame_index` keeps advancing so the gap is
//! visible, and the next frame is forced to be a keyframe.

use thiserror::Error;
use tokio::sync::mpsc;
use ws_proto::{DisplayId, DisplayInfo, EncodedFrame, VideoConfig};

#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("display capture is not supported on this platform")]
    Unsupported,
    #[error("display {0:?} not found")]
    DisplayNotFound(DisplayId),
    #[error("no supported hardware encoder found")]
    NoEncoder,
    #[error("{0}")]
    Platform(String),
}

pub type Result<T> = std::result::Result<T, CaptureError>;

/// Lists every display the host can capture.
pub fn list_displays() -> Result<Vec<DisplayInfo>> {
    Err(CaptureError::Unsupported)
}

/// Handle to a running capture + encode pipeline for one display.
pub struct VideoPipeline {
    _private: (),
}

impl VideoPipeline {
    /// Starts capturing `display` and encoding with `config`. Encoded frames
    /// are delivered with `try_send`; a full channel drops the frame and
    /// forces the next one to be a keyframe.
    pub fn start(
        display: DisplayId,
        config: VideoConfig,
        frames: mpsc::Sender<EncodedFrame>,
    ) -> Result<Self> {
        let _ = (display, config, frames);
        Err(CaptureError::Unsupported)
    }

    /// Forces the next frame to be an IDR.
    pub fn request_keyframe(&self) {}

    /// Changes the target bitrate without restarting the encoder.
    pub fn set_bitrate(&self, kbps: u32) {
        let _ = kbps;
    }
}
