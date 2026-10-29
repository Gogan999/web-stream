//! Pairing and signaling over a real socket (plain HTTP; TLS is the same
//! router behind rustls).

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use ws_proto::{ByeReason, ClientSignal, HostSignal};
use ws_signaling::{Notice, SessionEvent, SignalingConfig, SignalingServer};

struct Http {
    status: u16,
    headers: String,
    body: String,
}

async fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    origin: Option<&str>,
    cookie: Option<&str>,
    body: &str,
) -> Http {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(o) = origin {
        req += &format!("Origin: {o}\r\n");
    }
    if let Some(c) = cookie {
        req += &format!("Cookie: {c}\r\n");
    }
    req += "\r\n";
    req += body;
    s.write_all(req.as_bytes()).await.unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).await.unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap();
    let status = head[9..12].parse().unwrap();
    Http {
        status,
        headers: head.to_string(),
        body: body.to_string(),
    }
}

fn body_json(h: &Http) -> serde_json::Value {
    serde_json::from_str(&h.body).unwrap_or_else(|_| panic!("not json: {}", h.body))
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

async fn ws_connect(addr: SocketAddr, cookie: &str) -> Result<Ws, u16> {
    ws_connect_with(addr, cookie, &[]).await
}

async fn ws_connect_with(
    addr: SocketAddr,
    cookie: &str,
    headers: &[(&'static str, &str)],
) -> Result<Ws, u16> {
    let mut req = format!("ws://{addr}/ws").into_client_request().unwrap();
    req.headers_mut()
        .insert("Origin", format!("https://{addr}").parse().unwrap());
    req.headers_mut().insert("Cookie", cookie.parse().unwrap());
    for (name, value) in headers {
        req.headers_mut().insert(*name, value.parse().unwrap());
    }
    match tokio_tungstenite::connect_async(req).await {
        Ok((ws, _)) => Ok(ws),
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => Err(resp.status().as_u16()),
        Err(e) => panic!("{e}"),
    }
}

async fn next_host_msg<
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
>(
    ws: &mut S,
) -> HostSignal {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if let Message::Text(t) = msg {
            return serde_json::from_str(&t).unwrap();
        }
    }
}

/// Pairs a browser and returns its cookie.
async fn pair(
    addr: SocketAddr,
    origin: Option<&str>,
    handle: &ws_signaling::SignalingHandle,
    events: &mut ws_signaling::SignalingEvents,
) -> String {
    handle.open_pairing().await.unwrap();
    let r = request(
        addr,
        "POST",
        "/api/pair/start",
        origin,
        None,
        r#"{"device_name":"Test browser"}"#,
    )
    .await;
    let pairing_id = body_json(&r)["pairing_id"].as_str().unwrap().to_string();
    let Some(Notice::PairingCode { code, .. }) = events.notices.recv().await else {
        panic!("no pairing code");
    };
    let r = request(
        addr,
        "POST",
        "/api/pair/confirm",
        origin,
        None,
        &format!(r#"{{"pairing_id":"{pairing_id}","code":"{code}"}}"#),
    )
    .await;
    let token = r
        .headers
        .split("__Host-ws_device=")
        .nth(1)
        .unwrap()
        .split(';')
        .next()
        .unwrap();
    format!("__Host-ws_device={token}")
}

#[tokio::test]
async fn pair_then_signal() {
    let dir = tempfile::tempdir().unwrap();
    let config = SignalingConfig::new("127.0.0.1:0".parse().unwrap(), dir.path().to_path_buf());
    let (server, mut events) = SignalingServer::new(config).unwrap();
    let router = server.router();
    let handle = server.handle();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let origin = format!("https://{addr}");
    let origin = Some(origin.as_str());

    // Not paired yet.
    let r = request(addr, "GET", "/api/session", None, None, "").await;
    assert_eq!(r.status, 401);
    assert!(r.headers.to_lowercase().contains("content-security-policy"));
    assert_eq!(
        ws_connect(addr, "__Host-ws_device=nope").await.err(),
        Some(401)
    );

    // Cross-origin pairing is refused.
    let r = request(
        addr,
        "POST",
        "/api/pair/start",
        Some("https://evil.example"),
        None,
        r#"{"device_name":"x"}"#,
    )
    .await;
    assert_eq!(r.status, 403);

    // Pairing is closed until the host user opens it.
    let r = request(
        addr,
        "POST",
        "/api/pair/start",
        origin,
        None,
        r#"{"device_name":"x"}"#,
    )
    .await;
    assert_eq!(r.status, 403);
    assert_eq!(body_json(&r)["error"], "pairing_closed");
    handle.open_pairing().await.unwrap();

    let r = request(
        addr,
        "POST",
        "/api/pair/start",
        origin,
        None,
        r#"{"device_name":"Test browser"}"#,
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let pairing_id = body_json(&r)["pairing_id"].as_str().unwrap().to_string();
    let code = match events.notices.recv().await.unwrap() {
        Notice::PairingCode {
            code, device_name, ..
        } => {
            assert_eq!(device_name, "Test browser");
            code
        }
        other => panic!("{other:?}"),
    };

    let wrong = if code == "000000" { "111111" } else { "000000" };
    let r = request(
        addr,
        "POST",
        "/api/pair/confirm",
        origin,
        None,
        &format!(r#"{{"pairing_id":"{pairing_id}","code":"{wrong}"}}"#),
    )
    .await;
    assert_eq!(r.status, 403);
    assert_eq!(body_json(&r)["attempts_left"], 2);

    let r = request(
        addr,
        "POST",
        "/api/pair/confirm",
        origin,
        None,
        &format!(r#"{{"pairing_id":"{pairing_id}","code":"{code}"}}"#),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let set_cookie = r
        .headers
        .lines()
        .find(|l| l.to_lowercase().starts_with("set-cookie:"))
        .unwrap();
    assert!(
        set_cookie.contains("HttpOnly")
            && set_cookie.contains("SameSite=Strict")
            && set_cookie.contains("Secure")
    );
    let token = set_cookie
        .split("__Host-ws_device=")
        .nth(1)
        .unwrap()
        .split(';')
        .next()
        .unwrap();
    let cookie = format!("__Host-ws_device={token}");
    let r2 = request(addr, "GET", "/api/session", None, Some(&cookie), "").await;
    assert_eq!(r2.status, 200);
    assert_eq!(body_json(&r2)["device"], "Test browser");

    // Signaling.
    let mut ws = ws_connect(addr, &cookie).await.unwrap();
    assert_eq!(
        next_host_msg(&mut ws).await,
        HostSignal::Welcome {
            ice_servers: vec![]
        }
    );
    let mut session = events.sessions.recv().await.unwrap();
    assert_eq!(session.device_name, "Test browser");
    assert_eq!(session.host.as_deref(), Some("127.0.0.1"));
    assert!(session.send(HostSignal::Offer { sdp: "v=0".into() }).await);
    assert_eq!(
        next_host_msg(&mut ws).await,
        HostSignal::Offer { sdp: "v=0".into() }
    );
    ws.send(Message::text(r#"{"type":"answer","sdp":"v=0 answer"}"#))
        .await
        .unwrap();
    assert_eq!(
        session.recv().await,
        Some(SessionEvent::Signal(ClientSignal::Answer {
            sdp: "v=0 answer".into()
        }))
    );
    ws.send(Message::text(r#"{"type":"nonsense"}"#))
        .await
        .unwrap();
    assert!(matches!(
        next_host_msg(&mut ws).await,
        HostSignal::Error { .. }
    ));

    // A second connection replaces the first.
    let mut ws2 = ws_connect(addr, &cookie).await.unwrap();
    assert_eq!(
        next_host_msg(&mut ws).await,
        HostSignal::Bye {
            reason: ByeReason::Replaced
        }
    );
    assert_eq!(
        next_host_msg(&mut ws2).await,
        HostSignal::Welcome {
            ice_servers: vec![]
        }
    );
    assert_eq!(session.recv().await, None);
    let mut session2 = events.sessions.recv().await.unwrap();
    assert!(session2.host.is_some());

    // A handoff closes the socket but keeps the session live.
    ws2.send(Message::text(r#"{"type":"handoff"}"#))
        .await
        .unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(5), ws2.next())
        .await
        .unwrap();
    assert!(
        matches!(closed, Some(Ok(Message::Close(Some(ref f)))) if f.reason == "handoff"),
        "{closed:?}"
    );
    assert_eq!(session2.recv().await, Some(SessionEvent::HandedOff));
    assert!(
        tokio::time::timeout(Duration::from_millis(200), session2.recv())
            .await
            .is_err()
    );

    // A new connection still replaces it; the host hears why, to tell the
    // browser over the peer connection. (This one came through a Cloudflare
    // Tunnel, whose name is no place to send media.)
    let mut ws3 = ws_connect_with(addr, &cookie, &[("cf-connecting-ip", "198.51.100.9")])
        .await
        .unwrap();
    assert_eq!(
        next_host_msg(&mut ws3).await,
        HostSignal::Welcome {
            ice_servers: vec![]
        }
    );
    assert_eq!(
        session2.recv().await,
        Some(SessionEvent::Ended(ByeReason::Replaced))
    );
    assert_eq!(session2.recv().await, None);
    let mut session3 = events.sessions.recv().await.unwrap();
    assert_eq!(session3.host, None);

    // Unpairing a handed-off session ends it too.
    ws3.send(Message::text(r#"{"type":"handoff"}"#))
        .await
        .unwrap();
    assert_eq!(session3.recv().await, Some(SessionEvent::HandedOff));
    let r = request(addr, "POST", "/api/unpair", origin, Some(&cookie), "{}").await;
    assert_eq!(r.status, 200);
    assert_eq!(
        session3.recv().await,
        Some(SessionEvent::Ended(ByeReason::Revoked))
    );
    assert_eq!(ws_connect(addr, &cookie).await.err(), Some(401));
}

#[tokio::test]
async fn unpair_kicks_a_live_socket() {
    let dir = tempfile::tempdir().unwrap();
    let config = SignalingConfig::new("127.0.0.1:0".parse().unwrap(), dir.path().to_path_buf());
    let (server, mut events) = SignalingServer::new(config).unwrap();
    let router = server.router();
    let handle = server.handle();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let origin = format!("https://{addr}");
    let origin = Some(origin.as_str());
    let cookie = pair(addr, origin, &handle, &mut events).await;
    let mut ws2 = ws_connect(addr, &cookie).await.unwrap();
    next_host_msg(&mut ws2).await;
    let _session = events.sessions.recv().await.unwrap();

    // Unpairing kicks the live session and kills the token.
    let r = request(addr, "POST", "/api/unpair", origin, Some(&cookie), "{}").await;
    assert_eq!(r.status, 200);
    assert_eq!(
        next_host_msg(&mut ws2).await,
        HostSignal::Bye {
            reason: ByeReason::Revoked
        }
    );
    assert_eq!(ws_connect(addr, &cookie).await.err(), Some(401));
}

#[tokio::test]
async fn serves_the_built_in_web_client() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = SignalingConfig::new("127.0.0.1:0".parse().unwrap(), dir.path().to_path_buf());
    config.embedded_web = &[
        ("index.html", b"<!doctype html>hi"),
        ("js/main.js", b"export {};"),
    ];
    let (server, _events) = SignalingServer::new(config).unwrap();
    let router = server.router();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let r = request(addr, "GET", "/", None, None, "").await;
    assert_eq!((r.status, r.body.as_str()), (200, "<!doctype html>hi"));
    let headers = r.headers.to_lowercase();
    assert!(headers.contains("content-type: text/html"));
    assert!(headers.contains("content-security-policy"));
    assert!(headers.contains("cross-origin-opener-policy: same-origin"));
    assert!(headers.contains("permissions-policy: microphone=(self), camera=()"));
    let r = request(addr, "GET", "/js/main.js", None, None, "").await;
    assert_eq!(r.status, 200);
    assert!(r.headers.to_lowercase().contains("text/javascript"));
    assert_eq!(
        request(addr, "GET", "/js/other.js", None, None, "")
            .await
            .status,
        404
    );
    assert_eq!(
        request(addr, "GET", "/../Cargo.toml", None, None, "")
            .await
            .status,
        404
    );
}

#[tokio::test]
async fn pairing_link_pairs_once() {
    let dir = tempfile::tempdir().unwrap();
    let config = SignalingConfig::new("127.0.0.1:0".parse().unwrap(), dir.path().to_path_buf());
    let (server, mut events) = SignalingServer::new(config).unwrap();
    let router = server.router();
    let handle = server.handle();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let origin = format!("https://{addr}");
    let origin = Some(origin.as_str());
    let body = |link: &str| format!(r#"{{"link":"{link}","device_name":"Phone"}}"#);

    // Closed: nothing to pair with.
    let r = request(addr, "POST", "/api/pair/link", origin, None, &body("x")).await;
    assert_eq!(
        (r.status, &*body_json(&r)["error"].to_string()),
        (403, "\"pairing_closed\"")
    );

    let link = handle.open_pairing().await.unwrap();
    // Cross-origin is refused even with the right link.
    let r = request(
        addr,
        "POST",
        "/api/pair/link",
        Some("https://evil.example"),
        None,
        &body(&link),
    )
    .await;
    assert_eq!(r.status, 403);
    let r = request(addr, "POST", "/api/pair/link", origin, None, &body("wrong")).await;
    assert_eq!(r.status, 403);

    let r = request(addr, "POST", "/api/pair/link", origin, None, &body(&link)).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.headers.contains("__Host-ws_device="));
    assert_eq!(
        events.notices.recv().await,
        Some(Notice::Paired {
            device_name: "Phone".into()
        })
    );
    // Once only.
    let r = request(addr, "POST", "/api/pair/link", origin, None, &body(&link)).await;
    assert_eq!(r.status, 403);
}
