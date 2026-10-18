use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::{DisplayId, MicrosMono};

static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// Converts an [`Instant`](std::time::Instant) to [`MicrosMono`]. The epoch is
/// fixed the first time this (or [`mono_now`]) is called; earlier instants
/// clamp to 0.
pub fn mono_micros(at: std::time::Instant) -> MicrosMono {
    let epoch = *EPOCH.get_or_init(std::time::Instant::now);
    at.saturating_duration_since(epoch).as_micros() as MicrosMono
}

/// The current time as [`MicrosMono`].
pub fn mono_now() -> MicrosMono {
    mono_micros(std::time::Instant::now())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VideoCodec {
    H264,
    H265,
    Av1,
}

/// How the encoder trades picture quality against encode time. The bitrate
/// is set separately; this picks how hard the encoder works to spend it well.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VideoQuality {
    /// Fastest encode, softest picture.
    Speed,
    #[default]
    Balanced,
    /// Slowest encode the frame rate allows, and a little room for large
    /// frames (fast motion, keyframes) to take longer than one frame time.
    Clarity,
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
    /// Older clients leave this out and get the default.
    #[serde(default)]
    pub quality: VideoQuality,
    /// How many times an ordinary frame's bits a keyframe may use, 1..=4.
    /// A bigger keyframe looks sharper the moment it arrives but takes that
    /// many frame times to send.
    #[serde(default = "default_keyframe_scale")]
    pub keyframe_scale: u8,
    /// Repair a frame the host itself dropped (sent too late, queue full)
    /// by telling the encoder to stop referencing it, instead of sending a
    /// keyframe. Loss on the network still needs a keyframe.
    #[serde(default = "default_true")]
    pub recover_drops: bool,
}

pub const DEFAULT_KEYFRAME_SCALE: u8 = 2;

fn default_keyframe_scale() -> u8 {
    DEFAULT_KEYFRAME_SCALE
}

fn default_true() -> bool {
    true
}

impl VideoConfig {
    /// Clamps a client-supplied config to values the host will run: even
    /// sizes from 320x180 to 7680x4320, 1..=240 fps, 500..=200000 kbps,
    /// keyframes 1..=4 frames' worth of bits.
    pub fn clamped(&self) -> Self {
        let even = |v: u32, lo: u32, hi: u32| v.clamp(lo, hi) & !1;
        Self {
            codec: self.codec,
            width: even(self.width, 320, 7680),
            height: even(self.height, 180, 4320),
            fps: self.fps.clamp(1, 240),
            bitrate_kbps: self.bitrate_kbps.clamp(500, 200_000),
            quality: self.quality,
            keyframe_scale: self.keyframe_scale.clamp(1, 4),
            recover_drops: self.recover_drops,
        }
    }
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            codec: VideoCodec::H264,
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: 20_000,
            quality: VideoQuality::Balanced,
            keyframe_scale: DEFAULT_KEYFRAME_SCALE,
            recover_drops: true,
        }
    }
}

/// One encoded access unit, produced by the capture/encode crate and consumed
/// by the transport crate.
///
/// `data` is Annex-B for H.264/H.265 and a temporal unit (OBU stream) for AV1.
/// Nothing queues frames. If the transport's channel is full when a frame
/// finishes encoding, the capture side drops that frame, keeps
/// `frame_index` advancing (so the drop shows as a gap) and makes the next
/// frame a keyframe, or a frame that references nothing from the gap (see
/// [`EncodedFrame::recovers_from`]). If the transport itself falls behind
/// it skips to the newest frame and asks for one of those.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedFrame {
    pub display: DisplayId,
    pub codec: VideoCodec,
    /// Monotonic, gap-free per encoder session; a gap on the receive side
    /// means a frame was dropped.
    pub frame_index: u64,
    pub keyframe: bool,
    /// Set on the first frame encoded after the encoder was told to stop
    /// referencing frames from this index on: it builds only on frames
    /// before it, so it can follow a gap that starts there without a
    /// keyframe.
    pub recovers_from: Option<u64>,
    /// When the desktop image was captured.
    pub capture_time: MicrosMono,
    /// When the encoder returned the bitstream.
    pub encode_done_time: MicrosMono,
    pub width: u32,
    pub height: u32,
    pub data: Bytes,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_hostile_config() {
        let c = VideoConfig {
            codec: VideoCodec::H264,
            width: 99_999,
            height: 1,
            fps: 0,
            bitrate_kbps: u32::MAX,
            quality: VideoQuality::Speed,
            keyframe_scale: 200,
            recover_drops: false,
        }
        .clamped();
        assert_eq!(
            (c.width, c.height, c.fps, c.bitrate_kbps, c.keyframe_scale),
            (7680, 180, 1, 200_000, 4)
        );
        let odd = VideoConfig {
            width: 1921,
            height: 1081,
            ..Default::default()
        }
        .clamped();
        assert_eq!((odd.width, odd.height), (1920, 1080));
    }

    #[test]
    fn quality_is_optional_on_the_wire() {
        let json = r#"{"codec":"h264","width":1280,"height":720,"fps":60,"bitrateKbps":8000}"#;
        let c: VideoConfig = serde_json::from_str(json).unwrap();
        assert_eq!(c.quality, VideoQuality::Balanced);
        assert_eq!(c.keyframe_scale, DEFAULT_KEYFRAME_SCALE);
        assert!(c.recover_drops);
        let c = VideoConfig {
            quality: VideoQuality::Clarity,
            ..Default::default()
        };
        assert!(
            serde_json::to_string(&c)
                .unwrap()
                .contains(r#""quality":"clarity""#)
        );
    }
}
