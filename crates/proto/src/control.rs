use serde::{Deserialize, Serialize};

use crate::{DisplayId, DisplayInfo, VideoConfig};

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
    /// Ask for a file the host offered with [`HostControl::FileOffer`]; any
    /// other id is rejected. The file transfer work owns the transfer itself.
    RequestFile {
        id: FileId,
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
    Error {
        message: String,
    },
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
