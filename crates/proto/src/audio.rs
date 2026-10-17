use bytes::Bytes;

use crate::MicrosMono;

/// Direction of an audio stream, from the host's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AudioDirection {
    /// Host speakers -> browser.
    Playback,
    /// Browser microphone -> host virtual mic.
    Capture,
}

/// One Opus packet (typically 10 ms or 20 ms).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioPacket {
    pub direction: AudioDirection,
    pub sequence: u64,
    pub capture_time: MicrosMono,
    pub sample_rate: u32,
    pub channels: u8,
    pub data: Bytes,
}
