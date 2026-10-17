//! Signaling and pairing messages, as specified in `docs/signaling.md`
//! (owned by the transport work). The host serves both itself: pairing over
//! same-origin HTTPS under `/api`, signaling over the `/ws` WebSocket. The
//! host is always the offerer and never trickles candidates.

use serde::{Deserialize, Serialize};

/// Host -> browser on `/ws`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostSignal {
    /// First message after the upgrade; `ice_servers` feeds
    /// `RTCConfiguration.iceServers` and is usually empty.
    Welcome {
        ice_servers: Vec<IceServer>,
    },
    /// Sent right after `welcome` and after every `restart`.
    Offer {
        sdp: String,
    },
    Error {
        code: String,
        message: String,
    },
    Bye {
        reason: ByeReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ByeReason {
    /// A newer connection took over; only one session is live.
    Replaced,
    HostShutdown,
    /// This device's pairing was revoked.
    Revoked,
}

/// Browser -> host on `/ws`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientSignal {
    Answer {
        sdp: String,
    },
    /// Optional; spelled like `RTCIceCandidateInit`.
    Candidate {
        candidate: String,
        #[serde(rename = "sdpMid", default)]
        sdp_mid: Option<String>,
        #[serde(rename = "sdpMLineIndex", default)]
        sdp_mline_index: Option<u16>,
    },
    /// Ask for a fresh offer, e.g. after ICE failure.
    Restart,
}

/// One `RTCIceServer`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IceServer {
    pub urls: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

/// `GET /api/session` response (200 or 401).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStatus {
    pub paired: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
}

/// `POST /api/pair/start` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairStart {
    pub device_name: String,
}

/// `POST /api/pair/start` 200 response. The host shows a 6-digit code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairStarted {
    pub pairing_id: String,
    /// Seconds.
    pub expires_in: u32,
}

/// `POST /api/pair/confirm` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairConfirm {
    pub pairing_id: String,
    pub code: String,
}

/// `POST /api/pair/confirm` 200 response; the `ws_device` cookie is set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairConfirmed {
    pub paired: bool,
}

/// Error body on 403/410/429 from the pairing API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    /// `bad_code`, `expired` or `rate_limited`.
    pub error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempts_left: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_signals_match_doc() {
        let welcome = HostSignal::Welcome {
            ice_servers: vec![],
        };
        assert_eq!(
            serde_json::to_string(&welcome).unwrap(),
            r#"{"type":"welcome","ice_servers":[]}"#
        );
        let bye = HostSignal::Bye {
            reason: ByeReason::HostShutdown,
        };
        assert_eq!(
            serde_json::to_string(&bye).unwrap(),
            r#"{"type":"bye","reason":"host_shutdown"}"#
        );
    }

    #[test]
    fn browser_candidate_parses() {
        let json =
            r#"{"type":"candidate","candidate":"candidate:1","sdpMid":"0","sdpMLineIndex":0}"#;
        let msg: ClientSignal = serde_json::from_str(json).unwrap();
        assert_eq!(
            msg,
            ClientSignal::Candidate {
                candidate: "candidate:1".into(),
                sdp_mid: Some("0".into()),
                sdp_mline_index: Some(0),
            }
        );
        assert_eq!(serde_json::to_string(&msg).unwrap(), json);
        let restart: ClientSignal = serde_json::from_str(r#"{"type":"restart"}"#).unwrap();
        assert_eq!(restart, ClientSignal::Restart);
    }

    #[test]
    fn pairing_error_shape() {
        let e: ApiError =
            serde_json::from_str(r#"{"error":"bad_code","attempts_left":2}"#).unwrap();
        assert_eq!(e.attempts_left, Some(2));
    }
}
