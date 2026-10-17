//! Host audio: WASAPI loopback capture and Opus encode for playback in the
//! browser, and decode of the browser microphone into a virtual capture
//! device. Packets cross crate boundaries as [`ws_proto::AudioPacket`].
//!
//! Game audio: [`LoopbackCapture::start`] captures the default output (or a
//! named one), converts to 48 kHz stereo and sends one 10 ms Opus packet per
//! [`ws_proto::AudioPacket`] with `try_send`; a full channel drops the packet
//! rather than queueing old audio.
//!
//! Mic: [`MicPlayback::start`] opens a render device such as VB-Audio's
//! "CABLE Input", which games then see as the "CABLE Output" microphone.
//! Feed it every Opus payload of the browser's mic track with
//! [`MicReceiver::push`].

pub mod convert;
pub mod encode;
pub mod mic;

#[cfg(windows)]
mod win;

pub use encode::{DEFAULT_BITRATE, FRAME_SAMPLES};
pub use mic::{JitterConfig, JitterStats, MicReceiver};

#[cfg(windows)]
pub use win::{LoopbackCapture, MicPlayback, list_render_devices};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum AudioError {
    #[error("opus: {0}")]
    Opus(#[from] opus::Error),
    #[error("audio device not found: {0}")]
    DeviceNotFound(String),
    #[error("unsupported audio format: {0}")]
    UnsupportedFormat(String),
    #[error("{0}")]
    Platform(String),
}

pub type Result<T> = std::result::Result<T, AudioError>;

/// Settings for capturing what the host is playing.
#[derive(Debug, Clone)]
pub struct LoopbackConfig {
    /// Substring of an output device's name; `None` follows the default
    /// output device, including when the user switches it mid-session.
    pub device: Option<String>,
    pub bitrate: u32,
}

impl Default for LoopbackConfig {
    fn default() -> Self {
        Self {
            device: None,
            bitrate: DEFAULT_BITRATE,
        }
    }
}

/// Settings for playing the browser mic into a virtual cable.
#[derive(Debug, Clone)]
pub struct MicConfig {
    /// Case-insensitive substring of the render device's name.
    pub device: String,
    pub jitter: JitterConfig,
}

impl Default for MicConfig {
    fn default() -> Self {
        Self {
            device: "CABLE Input".into(),
            jitter: JitterConfig::default(),
        }
    }
}
