//! Display capture and hardware video encode for the host.

#[cfg_attr(not(windows), allow(dead_code))]
mod bitstream;
pub mod caps;
#[cfg(any(windows, target_os = "linux"))]
mod encode;
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
mod layout;
#[cfg(target_os = "linux")]
mod linux;
#[cfg_attr(not(windows), allow(dead_code))]
mod modes;
#[cfg(any(windows, target_os = "linux"))]
mod nvenc;
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
mod pacing;
#[cfg(any(windows, target_os = "linux"))]
mod session;
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
mod tuning;
#[cfg(windows)]
mod win;

#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(windows)]
use win as platform;

use thiserror::Error;
use tokio::sync::mpsc;
use ws_proto::{DisplayId, DisplayInfo, EncodedFrame, SendBacklog, VideoCodec, VideoConfig};

/// SDP `a=fmtp` parameters describing what the encoder produces, for the transport's offer.
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

/// The hardware encoders on this PC, probed once per process. Empty when none were found or the
/// platform has no probe.
pub fn encoders() -> &'static [caps::EncoderInfo] {
    static PROBED: std::sync::OnceLock<Vec<caps::EncoderInfo>> = std::sync::OnceLock::new();
    PROBED.get_or_init(|| {
        #[cfg(any(windows, target_os = "linux"))]
        {
            platform::probe_encoders()
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        {
            Vec::new()
        }
    })
}

/// Lists every display the host can capture.
pub fn list_displays() -> Result<Vec<DisplayInfo>> {
    #[cfg(any(windows, target_os = "linux"))]
    {
        platform::list_displays()
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        Err(CaptureError::Unsupported)
    }
}

/// Switches `display`'s desktop to the largest resolution it supports that fits in `width` x
/// `height`, keeping the refresh rate where it can, and returns the size chosen and whether the
/// mode actually changed.
pub fn set_display_mode(display: DisplayId, width: u32, height: u32) -> Result<ModeChange> {
    #[cfg(any(windows, target_os = "linux"))]
    {
        platform::set_display_mode(display, width, height)
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = (display, width, height);
        Err(CaptureError::Unsupported)
    }
}

/// Puts `display` back to its saved resolution.
pub fn restore_display_mode(display: DisplayId) -> Result<()> {
    #[cfg(any(windows, target_os = "linux"))]
    {
        platform::restore_display_mode(display)
    }
    #[cfg(not(any(windows, target_os = "linux")))]
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

/// Handle to a running capture + encode pipeline.
pub struct VideoPipeline {
    #[cfg(any(windows, target_os = "linux"))]
    inner: session::PipelineHandle,
}

impl VideoPipeline {
    /// Starts capturing `display` and encoding at `config`'s size, rate and bitrate.
    pub fn start(
        display: DisplayId,
        config: VideoConfig,
        max: (u32, u32),
        frames: mpsc::Sender<EncodedFrame>,
        backlog: SendBacklog,
    ) -> Result<Self> {
        validate(&config)?;
        #[cfg(any(windows, target_os = "linux"))]
        {
            Ok(Self {
                inner: platform::start(display, config, max, frames, backlog)?,
            })
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        {
            let _ = (display, max, frames, backlog);
            Err(CaptureError::Unsupported)
        }
    }

    /// Changes the stream size, in place where the encoder can, else by opening a new encoder on
    /// the pipeline's thread.
    pub fn resize(&self, width: u32, height: u32) -> Result<()> {
        validate(&VideoConfig {
            width,
            height,
            ..Default::default()
        })?;
        #[cfg(any(windows, target_os = "linux"))]
        {
            self.inner.resize(width, height)
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        {
            Err(CaptureError::Unsupported)
        }
    }

    /// A handle for keyframe and repair requests that any thread can hold.
    pub fn control(&self) -> VideoControl {
        VideoControl {
            #[cfg(any(windows, target_os = "linux"))]
            inner: self.inner.control(),
        }
    }

    /// Switches capture to another display without changing the stream's size or codec.
    pub fn select_display(&self, display: DisplayId) -> Result<()> {
        #[cfg(any(windows, target_os = "linux"))]
        {
            self.inner.select_display(display)
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        {
            let _ = display;
            Err(CaptureError::Unsupported)
        }
    }

    /// Forces the next frame to be an IDR.
    pub fn request_keyframe(&self) {
        #[cfg(any(windows, target_os = "linux"))]
        self.inner.request_keyframe();
    }

    /// Frames from `index` on never reached the browser: the next frame must build only on frames
    /// before it.
    pub fn recover_from(&self, index: u64) {
        #[cfg(any(windows, target_os = "linux"))]
        self.inner.recover_from(index);
        #[cfg(not(any(windows, target_os = "linux")))]
        let _ = index;
    }

    /// Changes the target bitrate without restarting the encoder.
    pub fn set_bitrate(&self, kbps: u32) {
        #[cfg(any(windows, target_os = "linux"))]
        self.inner.set_bitrate(kbps.clamp(100, 500_000));
        #[cfg(not(any(windows, target_os = "linux")))]
        let _ = kbps;
    }
}

/// See [`VideoPipeline::control`].
#[derive(Clone)]
pub struct VideoControl {
    #[cfg(any(windows, target_os = "linux"))]
    inner: session::Control,
}

impl VideoControl {
    /// Forces the next frame to be an IDR.
    pub fn request_keyframe(&self) {
        #[cfg(any(windows, target_os = "linux"))]
        self.inner.request_keyframe();
    }

    /// See [`VideoPipeline::recover_from`].
    pub fn recover_from(&self, index: u64) {
        #[cfg(any(windows, target_os = "linux"))]
        self.inner.recover_from(index);
        #[cfg(not(any(windows, target_os = "linux")))]
        let _ = index;
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
