use serde::{Deserialize, Serialize};

use crate::{CursorShape, DisplayId, DisplayInfo, VideoConfig};

/// Identifies a file the host has offered this session. The host only ever
/// serves ids it issued, never paths from the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FileId(pub u64);

/// Messages on the reliable `control` data channel, browser -> host.
///
/// Everything here is untrusted input: the host must validate or clamp every
/// value (see [`VideoConfig::clamped`]) before acting on it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ClientControl {
    ListDisplays,
    SelectDisplay {
        id: DisplayId,
    },
    /// Ask for an IDR, e.g. after the decoder saw a gap.
    RequestKeyframe,
    SetVideoConfig {
        config: VideoConfig,
    },
    SetMicEnabled {
        enabled: bool,
    },
    /// Change the streamed display's desktop resolution to the largest it
    /// supports within `width` x `height` (the browser's screen). Only
    /// honoured when the host user started the host with
    /// `--allow-resolution-change`; undone on disconnect.
    SetHostResolution {
        width: u32,
        height: u32,
    },
    /// Put every display this session changed back to its own resolution.
    RestoreHostResolution,
    /// Ask for a file the host offered with [`HostControl::FileOffer`]; any
    /// other id is rejected. The file transfer work owns the transfer itself.
    RequestFile {
        id: FileId,
    },
    /// Whether to send [`HostControl::CursorPosition`]: the browser draws
    /// the host pointer itself in relative mouse mode, while in absolute
    /// mode its own pointer is already where the host's is. Off until asked.
    SetCursorPositions {
        enabled: bool,
    },
    Ping {
        client_time_ms: f64,
    },
}

/// Messages on the reliable `control` data channel, host -> browser.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum HostControl {
    Displays {
        displays: Vec<DisplayInfo>,
        active: Option<DisplayId>,
    },
    DisplayChanged {
        id: DisplayId,
        config: VideoConfig,
    },
    Pong {
        client_time_ms: f64,
    },
    /// A file the host user chose to share; the client may request it by id.
    FileOffer {
        id: FileId,
        name: String,
        size: u64,
    },
    /// Gamepad rumble, motor strengths 0.0..=1.0.
    Rumble {
        index: u8,
        strong: f32,
        weak: f32,
    },
    /// Sent about once a second while video runs, for the browser's stats
    /// overlay and auto-tune.
    VideoStats(VideoStats),
    /// The host pointer changed shape, or was hidden or shown. Sent when the
    /// stream starts and on every change.
    CursorShape(CursorShape),
    /// Where the host pointer's hotspot is, in desktop pixels from the
    /// streamed display's top-left corner (outside the display when the
    /// pointer is on another one). Sent on change while the pointer shows and
    /// the browser asked with [`ClientControl::SetCursorPositions`]; only the
    /// newest matters.
    CursorPosition {
        x: i32,
        y: i32,
    },
    Error {
        message: String,
    },
}

/// The host's side of the stream over the last stats interval.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoStats {
    /// Bitrate the encoder is set to now.
    pub encoder_kbps: u32,
    /// The browser's bitrate setting: the most the encoder may use.
    pub max_kbps: u32,
    /// What the network is estimated to carry, if known yet.
    pub bwe_kbps: Option<u32>,
    /// Encoded frames handed to the network per second.
    pub fps: f32,
    /// Bitrate of those frames.
    pub sent_kbps: u32,
    /// Average capture-to-encoded time.
    pub encode_ms: f32,
    /// Keyframes asked for (by the browser or after a dropped frame) in
    /// the interval: each is a stall or a soft picture, so fewer is better.
    pub keyframe_requests: u32,
    /// Fraction of packets the browser reports lost, 0..=1.
    pub loss: Option<f32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_request_is_by_id() {
        let json = r#"{"type":"requestFile","id":7}"#;
        let msg: ClientControl = serde_json::from_str(json).unwrap();
        assert_eq!(msg, ClientControl::RequestFile { id: FileId(7) });
        assert!(
            serde_json::from_str::<ClientControl>(r#"{"type":"requestFile","path":"C:/x"}"#)
                .is_err()
        );
    }

    #[test]
    fn rumble_shape() {
        let msg = HostControl::Rumble {
            index: 0,
            strong: 1.0,
            weak: 0.5,
        };
        assert_eq!(
            serde_json::to_string(&msg).unwrap(),
            r#"{"type":"rumble","index":0,"strong":1.0,"weak":0.5}"#
        );
    }
}
