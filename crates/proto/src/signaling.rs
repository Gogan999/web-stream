use serde::{Deserialize, Serialize};

/// Messages on the signaling WebSocket (`/ws`), host -> browser. The host
/// is always the offerer; see `docs/signaling.md`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostSignal {
    Welcome { ice_servers: Vec<IceServer> },
    Offer { sdp: String },
    Error { code: String, message: String },
    Bye { reason: ByeReason },
}

/// Messages on the signaling WebSocket, browser -> host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientSignal {
    Answer {
        sdp: String,
    },
    Candidate {
        candidate: String,
        #[serde(rename = "sdpMid", default)]
        sdp_mid: Option<String>,
        #[serde(rename = "sdpMLineIndex", default)]
        sdp_mline_index: Option<u16>,
    },
    Restart,
}

/// Why the host closed the signaling session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ByeReason {
    /// Another connection from a paired browser took over.
    Replaced,
    HostShutdown,
    /// The device was unpaired.
    Revoked,
}

/// One entry of `RTCConfiguration.iceServers`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IceServer {
    pub urls: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_messages_use_type_tag() {
        let json = serde_json::to_string(&HostSignal::Offer { sdp: "v=0".into() }).unwrap();
        assert_eq!(json, r#"{"type":"offer","sdp":"v=0"}"#);
        let json = serde_json::to_string(&HostSignal::Bye {
            reason: ByeReason::HostShutdown,
        })
        .unwrap();
        assert_eq!(json, r#"{"type":"bye","reason":"host_shutdown"}"#);
    }

    #[test]
    fn parses_browser_candidate() {
        let msg: ClientSignal = serde_json::from_str(
            r#"{"type":"candidate","candidate":"candidate:1 1 udp 1 1.2.3.4 5 typ host","sdpMid":"0","sdpMLineIndex":0}"#,
        )
        .unwrap();
        assert_eq!(
            msg,
            ClientSignal::Candidate {
                candidate: "candidate:1 1 udp 1 1.2.3.4 5 typ host".into(),
                sdp_mid: Some("0".into()),
                sdp_mline_index: Some(0),
            }
        );
        let msg: ClientSignal = serde_json::from_str(r#"{"type":"restart"}"#).unwrap();
        assert_eq!(msg, ClientSignal::Restart);
    }
}
