use serde::{Deserialize, Serialize};

use crate::{DisplayId, DisplayInfo, VideoConfig};

/// Messages on the reliable `control` data channel, browser -> host.
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
    /// Ask the host to send a file (file transfer thread owns the details).
    RequestFile {
        path: String,
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
    Error {
        message: String,
    },
}
