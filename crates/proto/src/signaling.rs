use serde::{Deserialize, Serialize};

/// Which side of a session a signaling peer is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PeerRole {
    Host,
    Client,
}

/// JSON messages exchanged over the signaling WebSocket. The server only
/// routes these between the host and client of a session; it never sees
/// media.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum SignalMessage {
    /// First message from each peer. `token` authenticates the peer for
    /// `session`; the auth scheme is owned by the signaling thread.
    Join {
        session: String,
        role: PeerRole,
        token: String,
    },
    /// Server -> peer: the other side has joined.
    PeerJoined {
        role: PeerRole,
    },
    PeerLeft {
        role: PeerRole,
    },
    Offer {
        sdp: String,
    },
    Answer {
        sdp: String,
    },
    IceCandidate {
        candidate: String,
        sdp_mid: Option<String>,
        /// Spelled `sdpMLineIndex` to match `RTCIceCandidateInit`.
        #[serde(rename = "sdpMLineIndex")]
        sdp_mline_index: Option<u16>,
    },
    /// Server -> peer: short-lived TURN credentials.
    IceServers {
        servers: Vec<IceServer>,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
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
    fn signal_json_matches_browser_shape() {
        let msg = SignalMessage::IceCandidate {
            candidate: "candidate:1".into(),
            sdp_mid: Some("0".into()),
            sdp_mline_index: Some(0),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            json,
            r#"{"type":"iceCandidate","candidate":"candidate:1","sdpMid":"0","sdpMLineIndex":0}"#
        );
        assert_eq!(serde_json::from_str::<SignalMessage>(&json).unwrap(), msg);
    }
}
