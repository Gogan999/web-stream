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
//! is dropped (`frame_index` still advances) and the next frame skips over
//! it (see [`ws_proto::EncodedFrame::recovers_from`]), or is a keyframe
//! where the encoder can't, so the decoder can resume.
//!
//! The mouse cursor is not drawn into the video. DXGI excludes it; the host
//! sends its shape and position over the control channel instead
//! (`ws_input::cursor`), and the browser draws it locally with no encode
//! latency.
//!
//! [`VideoPipeline::start`] and [`VideoPipeline::select_display`] block
//! while the GPU objects are created (tens of milliseconds). From async code
//! call them inside `tokio::task::spawn_blocking`.

#[cfg_attr(not(windows), allow(dead_code))]
mod layout;
#[cfg_attr(not(windows), allow(dead_code))]
mod modes;
#[cfg_attr(not(windows), allow(dead_code))]
mod pacing;
#[cfg(feature = "synthetic")]
pub mod synthetic;
#[cfg_attr(not(windows), allow(dead_code))]
mod tuning;
#[cfg(windows)]
mod win;

use thiserror::Error;
use tokio::sync::mpsc;
use ws_proto::{DisplayId, DisplayInfo, EncodedFrame, SendBacklog, VideoCodec, VideoConfig};

/// SDP `a=fmtp` parameters describing what the encoder produces, for the
/// transport's offer. H.264 is Constrained High (no B-frames) at level 5.2,
/// HEVC Main at level 5.1 main tier, AV1 Main at level 5.1 main tier.
pub fn sdp_fmtp(codec: VideoCodec) -> &'static str {
    match codec {
        VideoCodec::H264 => {
            "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=640c34"
        }
        VideoCodec::H265 => "profile-id=1;tier-flag=0;level-id=153",
        VideoCodec::Av1 => "profile=0;level-idx=13;tier=0",
    }
}

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
    #[cfg(feature = "synthetic")]
    if synthetic::enabled() {
        return synthetic::list_displays();
    }
    #[cfg(windows)]
    {
        win::list_displays()
    }
    #[cfg(not(windows))]
    {
        Err(CaptureError::Unsupported)
    }
}

/// Switches `display`'s desktop to the largest resolution it supports that
/// fits in `width` x `height`, keeping the refresh rate where it can, and
/// returns the size chosen and whether the mode actually changed. The change is temporary: it is not saved as the
/// display's setting, [`restore_display_mode`] undoes it, and Windows is
/// expected to undo it when the host process ends (not documented by
/// Microsoft, so callers restore explicitly).
///
/// This changes the host user's own screen, so callers only do it when the
/// host user has opted in.
pub fn set_display_mode(display: DisplayId, width: u32, height: u32) -> Result<ModeChange> {
    #[cfg(feature = "synthetic")]
    if synthetic::enabled() {
        return synthetic::set_display_mode(display, width, height);
    }
    #[cfg(windows)]
    {
        win::set_display_mode(display, width, height)
    }
    #[cfg(not(windows))]
    {
        let _ = (display, width, height);
        Err(CaptureError::Unsupported)
    }
}

/// Puts `display` back to its saved resolution.
pub fn restore_display_mode(display: DisplayId) -> Result<()> {
    #[cfg(feature = "synthetic")]
    if synthetic::enabled() {
        return synthetic::restore_display_mode(display);
    }
    #[cfg(windows)]
    {
        win::restore_display_mode(display)
    }
    #[cfg(not(windows))]
    {
        let _ = display;
        Err(CaptureError::Unsupported)
    }
}

/// What [`set_display_mode`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeChange {
    pub width: u32,
    pub height: u32,
    /// False when the display was already in that mode.
    pub changed: bool,
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

/// The running pipeline: the platform's, or the synthetic source's. With
/// neither (another OS, without the feature) it can't be built.
enum Pipeline {
    #[cfg(windows)]
    Win(win::PipelineHandle),
    #[cfg(feature = "synthetic")]
    Synthetic(synthetic::PipelineHandle),
}

#[derive(Clone)]
enum Control {
    #[cfg(windows)]
    Win(win::Control),
    #[cfg(feature = "synthetic")]
    Synthetic(synthetic::Control),
}

/// Handle to a running capture + encode pipeline.
///
/// The pipeline runs on its own thread. It ends when the handle is dropped
/// or on an unrecoverable error; either way the frame channel's sender is
/// dropped, so the receiver sees the stream close.
pub struct VideoPipeline {
    inner: Pipeline,
}

#[cfg_attr(
    not(any(windows, feature = "synthetic")),
    allow(unused_variables, unreachable_code)
)]
impl VideoPipeline {
    /// Starts capturing `display` and encoding at `config`'s size, rate and
    /// bitrate. Returns once the encoder is initialized. The output size
    /// stays fixed for the life of the pipeline; displays with a different
    /// aspect ratio are letterboxed.
    /// `backlog` is the transport's send queue age: while it is behind,
    /// frames are skipped before they are encoded (see `pacing`).
    /// `max` (width, height) is the largest size [`Self::resize`] may later
    /// ask for without a new encoder.
    pub fn start(
        display: DisplayId,
        config: VideoConfig,
        max: (u32, u32),
        frames: mpsc::Sender<EncodedFrame>,
        backlog: SendBacklog,
    ) -> Result<Self> {
        validate(&config)?;
        #[cfg(feature = "synthetic")]
        if synthetic::enabled() {
            let _ = max;
            return Ok(Self {
                inner: Pipeline::Synthetic(synthetic::PipelineHandle::start(
                    display, config, frames, backlog,
                )?),
            });
        }
        #[cfg(windows)]
        {
            Ok(Self {
                inner: Pipeline::Win(win::PipelineHandle::start(
                    display, config, max, frames, backlog,
                )?),
            })
        }
        #[cfg(not(windows))]
        {
            let _ = (display, max, frames, backlog);
            Err(CaptureError::Unsupported)
        }
    }

    /// Changes the stream size, in place where the encoder can (no new
    /// device or session, so no stall), else by opening a new encoder on
    /// the pipeline's thread. The next frame is a keyframe; frame numbers
    /// carry on. Blocks until done.
    pub fn resize(&self, width: u32, height: u32) -> Result<()> {
        validate(&VideoConfig {
            width,
            height,
            ..Default::default()
        })?;
        match self.inner {
            #[cfg(windows)]
            Pipeline::Win(ref p) => p.resize(width, height),
            #[cfg(feature = "synthetic")]
            Pipeline::Synthetic(ref p) => p.resize(width, height),
        }
    }

    /// A handle for keyframe and repair requests that any thread can hold
    /// (the transport's run loop, say). It doesn't keep the pipeline alive:
    /// once the pipeline is dropped, requests are ignored.
    pub fn control(&self) -> VideoControl {
        VideoControl {
            inner: match self.inner {
                #[cfg(windows)]
                Pipeline::Win(ref p) => Control::Win(p.control()),
                #[cfg(feature = "synthetic")]
                Pipeline::Synthetic(ref p) => Control::Synthetic(p.control()),
            },
        }
    }

    /// Switches capture to another display without changing the stream's
    /// size or codec, so the browser keeps decoding the same track. The
    /// next frame is a keyframe.
    pub fn select_display(&self, display: DisplayId) -> Result<()> {
        match self.inner {
            #[cfg(windows)]
            Pipeline::Win(ref p) => p.select_display(display),
            #[cfg(feature = "synthetic")]
            Pipeline::Synthetic(ref p) => p.select_display(display),
        }
    }

    /// Forces the next frame to be an IDR.
    pub fn request_keyframe(&self) {
        match self.inner {
            #[cfg(windows)]
            Pipeline::Win(ref p) => p.request_keyframe(),
            #[cfg(feature = "synthetic")]
            Pipeline::Synthetic(ref p) => p.request_keyframe(),
        }
    }

    /// Frames from `index` on never reached the browser: the next frame
    /// must build only on frames before it. Invalidates them in the encoder
    /// where it can ([`ws_proto::VideoConfig::recover_drops`]), and sends a
    /// keyframe otherwise.
    pub fn recover_from(&self, index: u64) {
        match self.inner {
            #[cfg(windows)]
            Pipeline::Win(ref p) => p.recover_from(index),
            #[cfg(feature = "synthetic")]
            Pipeline::Synthetic(ref p) => p.recover_from(index),
        }
    }

    /// Changes the target bitrate without restarting the encoder.
    pub fn set_bitrate(&self, kbps: u32) {
        let kbps = kbps.clamp(100, 500_000);
        match self.inner {
            #[cfg(windows)]
            Pipeline::Win(ref p) => p.set_bitrate(kbps),
            #[cfg(feature = "synthetic")]
            Pipeline::Synthetic(ref p) => p.set_bitrate(kbps),
        }
    }
}

/// See [`VideoPipeline::control`].
#[derive(Clone)]
pub struct VideoControl {
    inner: Control,
}

#[cfg_attr(
    not(any(windows, feature = "synthetic")),
    allow(unused_variables, unreachable_code)
)]
impl VideoControl {
    /// Forces the next frame to be an IDR.
    pub fn request_keyframe(&self) {
        match self.inner {
            #[cfg(windows)]
            Control::Win(ref c) => c.request_keyframe(),
            #[cfg(feature = "synthetic")]
            Control::Synthetic(ref c) => c.request_keyframe(),
        }
    }

    /// See [`VideoPipeline::recover_from`].
    pub fn recover_from(&self, index: u64) {
        match self.inner {
            #[cfg(windows)]
            Control::Win(ref c) => c.recover_from(index),
            #[cfg(feature = "synthetic")]
            Control::Synthetic(ref c) => c.recover_from(index),
        }
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
