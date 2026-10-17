use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::{DisplayId, MicrosMono};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VideoCodec {
    H264,
    H265,
    Av1,
}

/// Encoder settings requested by the client or chosen by the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoConfig {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            codec: VideoCodec::H264,
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: 20_000,
        }
    }
}

/// One encoded access unit, produced by the capture/encode crate and consumed
/// by the transport crate.
///
/// `data` is Annex-B for H.264/H.265 and a temporal unit (OBU stream) for AV1.
/// The transport is expected to drop frames rather than queue them when it
/// falls behind; `keyframe` lets it resume cleanly after a drop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedFrame {
    pub display: DisplayId,
    pub codec: VideoCodec,
    /// Monotonic, gap-free per encoder session; a gap on the receive side
    /// means a frame was dropped.
    pub frame_index: u64,
    pub keyframe: bool,
    /// When the desktop image was captured.
    pub capture_time: MicrosMono,
    /// When the encoder returned the bitstream.
    pub encode_done_time: MicrosMono,
    pub width: u32,
    pub height: u32,
    pub data: Bytes,
}
