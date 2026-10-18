//! Short-lived TURN credentials (draft-uberti-behave-turn-rest), as coturn
//! checks them with `use-auth-secret`.

use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use hmac::{Hmac, Mac};
use sha1::Sha1;

use crate::config::TurnServer;
use ws_proto::IceServer;

pub fn ice_servers(servers: &[TurnServer], now: SystemTime) -> Vec<IceServer> {
    servers.iter().map(|s| credentials(s, now)).collect()
}

fn credentials(server: &TurnServer, now: SystemTime) -> IceServer {
    let expiry = now.duration_since(UNIX_EPOCH).unwrap_or_default() + server.ttl;
    let username = format!("{}:web-stream", expiry.as_secs());
    let mut mac = Hmac::<Sha1>::new_from_slice(server.shared_secret.as_bytes())
        .expect("HMAC accepts any key length");
    mac.update(username.as_bytes());
    IceServer {
        urls: server.urls.clone(),
        username: Some(username),
        credential: Some(STANDARD.encode(mac.finalize().into_bytes())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn matches_coturn_scheme() {
        let server = TurnServer {
            urls: vec!["turns:turn.example:443?transport=tcp".into()],
            shared_secret: "secret".into(),
            ttl: Duration::from_secs(3600),
        };
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        let ice = credentials(&server, now);
        assert_eq!(ice.username.as_deref(), Some("1003600:web-stream"));
        // printf '1003600:web-stream' | openssl dgst -sha1 -hmac secret -binary | base64
        assert_eq!(
            ice.credential.as_deref(),
            Some("Fc6VduirVhPFTBq3lqlH9U1thOo=")
        );
    }
}
