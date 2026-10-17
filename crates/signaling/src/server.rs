use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime};

use axum::Router;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{Mutex, mpsc, oneshot};
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;

use crate::config::SignalingConfig;
use crate::pairing::{CODE_TTL, ConfirmError, Pairing};
use crate::store::DeviceStore;
use crate::{Error, tls};
use ws_proto::{ByeReason, ClientSignal, HostSignal};

const COOKIE: &str = "ws_device";
const COOKIE_MAX_AGE_SECS: u64 = 365 * 24 * 3600;
const MAX_WS_MESSAGE: usize = 64 * 1024;
const MAX_API_BODY: usize = 4 * 1024;
const CSP: &str = "default-src 'self'; connect-src 'self'; img-src 'self' data: blob:; \
                   media-src 'self' blob:; style-src 'self' 'unsafe-inline'; object-src 'none'; \
                   base-uri 'none'; frame-ancestors 'none'; form-action 'self'";

/// Things the host should show its user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// Show this code until it expires or `Paired`/`PairingFailed` arrives.
    PairingCode {
        code: String,
        device_name: String,
        expires_in_secs: u64,
    },
    Paired {
        device_name: String,
    },
    PairingFailed {
        attempts_left: u8,
    },
    SessionStarted {
        device_name: String,
    },
    SessionEnded {
        device_name: String,
    },
}

/// An authenticated browser on the signaling socket. Dropping it closes the
/// socket.
pub struct SignalingSession {
    pub device_name: String,
    incoming: mpsc::Receiver<ClientSignal>,
    outgoing: mpsc::Sender<HostSignal>,
}

impl SignalingSession {
    /// Next message from the browser; `None` once the socket is gone or the
    /// session was replaced.
    pub async fn recv(&mut self) -> Option<ClientSignal> {
        self.incoming.recv().await
    }

    /// Returns false if the socket is already gone.
    pub async fn send(&self, msg: HostSignal) -> bool {
        self.outgoing.send(msg).await.is_ok()
    }

    pub fn sender(&self) -> mpsc::Sender<HostSignal> {
        self.outgoing.clone()
    }
}

/// Receivers the host's main loop reads.
pub struct SignalingEvents {
    pub sessions: mpsc::Receiver<SignalingSession>,
    pub notices: mpsc::UnboundedReceiver<Notice>,
}

struct Current {
    id: u64,
    token: String,
    kick: oneshot::Sender<ByeReason>,
}

struct Shared {
    config: SignalingConfig,
    store: Mutex<DeviceStore>,
    pairing: Mutex<Pairing>,
    current: Mutex<Option<Current>>,
    next_id: AtomicU64,
    sessions: mpsc::Sender<SignalingSession>,
    notices: mpsc::UnboundedSender<Notice>,
}

impl Shared {
    fn notify(&self, notice: Notice) {
        let _ = self.notices.send(notice);
    }

    async fn kick_current(&self, reason: ByeReason) {
        if let Some(cur) = self.current.lock().await.take() {
            let _ = cur.kick.send(reason);
        }
    }
}

/// Lets the rest of the host act on sessions, for example from a tray menu
/// or the "drop all sessions" hotkey.
#[derive(Clone)]
pub struct SignalingHandle {
    shared: Arc<Shared>,
}

impl SignalingHandle {
    /// End the live session, if any. The device stays paired.
    pub async fn drop_session(&self) {
        self.shared.kick_current(ByeReason::HostShutdown).await;
    }

    /// Forget every paired device and end the live session.
    pub async fn revoke_all(&self) -> Result<(), Error> {
        self.shared.store.lock().await.revoke_all()?;
        self.shared.kick_current(ByeReason::Revoked).await;
        Ok(())
    }

    pub async fn cancel_pairing(&self) {
        self.shared.pairing.lock().await.cancel();
    }
}

pub struct SignalingServer {
    shared: Arc<Shared>,
    tls: tls::LoadedTls,
}

impl SignalingServer {
    pub fn new(config: SignalingConfig) -> Result<(Self, SignalingEvents), Error> {
        std::fs::create_dir_all(&config.data_dir)?;
        let tls = tls::load(&config.tls, &config.data_dir)?;
        let store = DeviceStore::open(&config.data_dir, config.device_idle_expiry)?;
        let (sessions_tx, sessions) = mpsc::channel(4);
        let (notices_tx, notices) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            config,
            store: Mutex::new(store),
            pairing: Mutex::new(Pairing::default()),
            current: Mutex::new(None),
            next_id: AtomicU64::new(1),
            sessions: sessions_tx,
            notices: notices_tx,
        });
        Ok((Self { shared, tls }, SignalingEvents { sessions, notices }))
    }

    /// SHA-256 fingerprint of the TLS certificate, for the user to compare
    /// with what the browser shows.
    pub fn fingerprint(&self) -> &str {
        &self.tls.fingerprint
    }

    pub fn handle(&self) -> SignalingHandle {
        SignalingHandle {
            shared: self.shared.clone(),
        }
    }

    /// The HTTP routes without TLS, for tests or a custom listener.
    pub fn router(&self) -> Router {
        router(self.shared.clone())
    }

    pub async fn serve(self) -> Result<(), Error> {
        let addr = self.shared.config.bind;
        let app = router(self.shared.clone());
        let rustls = axum_server::tls_rustls::RustlsConfig::from_config(self.tls.config.clone());
        tracing::info!(%addr, fingerprint = %self.tls.fingerprint, "signaling listening");
        axum_server::bind_rustls(addr, rustls)
            .serve(app.into_make_service())
            .await?;
        Ok(())
    }
}

fn router(shared: Arc<Shared>) -> Router {
    let api = Router::new()
        .route("/api/session", get(session_status))
        .route("/api/pair/start", post(pair_start))
        .route("/api/pair/confirm", post(pair_confirm))
        .route("/api/unpair", post(unpair))
        .route("/ws", get(ws_upgrade))
        .layer(DefaultBodyLimit::max(MAX_API_BODY))
        .with_state(shared.clone());

    let app = match &shared.config.web_root {
        Some(root) => api.fallback_service(ServeDir::new(root)),
        None => api.fallback(|| async { Html(PLACEHOLDER) }),
    };

    app.layer(SetResponseHeaderLayer::overriding(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    ))
    .layer(SetResponseHeaderLayer::overriding(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    ))
    .layer(SetResponseHeaderLayer::overriding(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    ))
    .layer(SetResponseHeaderLayer::overriding(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    ))
}

const PLACEHOLDER: &str = "<!doctype html><meta charset=utf-8><title>web-stream</title>\
                           <p>The web client is not installed on this host.";

fn err(status: StatusCode, body: serde_json::Value) -> Response {
    (status, axum::Json(body)).into_response()
}

/// Accept only requests from the host's own origin (or configured extras),
/// which blocks cross-site form posts and cross-site WebSocket hijacking.
fn origin_ok(shared: &Shared, headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    if shared.config.extra_origins.iter().any(|o| o == origin) {
        return true;
    }
    let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    origin.strip_prefix("https://") == Some(host)
}

/// POSTs must be same-origin JSON. A JSON content type cannot be sent
/// cross-origin without a CORS preflight, which this server never approves.
fn post_ok(shared: &Shared, headers: &HeaderMap) -> bool {
    let json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(';')
                .next()
                .is_some_and(|t| t.trim().eq_ignore_ascii_case("application/json"))
        });
    json && origin_ok(shared, headers)
}

fn cookie_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|kv| {
            kv.trim()
                .strip_prefix(COOKIE)?
                .strip_prefix('=')
                .map(str::to_string)
        })
        .filter(|t| !t.is_empty())
}

fn set_cookie(token: &str, max_age: u64) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{COOKIE}={token}; Path=/; Max-Age={max_age}; HttpOnly; Secure; SameSite=Strict"
    ))
    .expect("token is base64url")
}

async fn session_status(State(shared): State<Arc<Shared>>, headers: HeaderMap) -> Response {
    let store = shared.store.lock().await;
    match cookie_token(&headers).and_then(|t| store.check(&t, SystemTime::now()).cloned()) {
        Some(dev) => axum::Json(json!({"paired": true, "device": dev.name})).into_response(),
        None => err(StatusCode::UNAUTHORIZED, json!({"paired": false})),
    }
}

#[derive(Deserialize)]
struct PairStart {
    device_name: String,
}

async fn pair_start(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if !post_ok(&shared, &headers) {
        return err(StatusCode::FORBIDDEN, json!({"error": "bad_origin"}));
    }
    let Ok(req) = serde_json::from_str::<PairStart>(&body) else {
        return err(StatusCode::BAD_REQUEST, json!({"error": "bad_request"}));
    };
    let started = shared
        .pairing
        .lock()
        .await
        .start(&req.device_name, Instant::now());
    match started {
        Ok(Some(s)) => {
            tracing::warn!(device = %s.device_name, code = %s.code, "pairing requested; show this code to the host user");
            shared.notify(Notice::PairingCode {
                code: s.code,
                device_name: s.device_name,
                expires_in_secs: CODE_TTL.as_secs(),
            });
            axum::Json(json!({"pairing_id": s.pairing_id, "expires_in": CODE_TTL.as_secs()}))
                .into_response()
        }
        Ok(None) => err(
            StatusCode::TOO_MANY_REQUESTS,
            json!({"error": "rate_limited"}),
        ),
        Err(e) => {
            tracing::error!("pairing start failed: {e}");
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error": "internal"}),
            )
        }
    }
}

#[derive(Deserialize)]
struct PairConfirm {
    pairing_id: String,
    code: String,
}

async fn pair_confirm(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if !post_ok(&shared, &headers) {
        return err(StatusCode::FORBIDDEN, json!({"error": "bad_origin"}));
    }
    let Ok(req) = serde_json::from_str::<PairConfirm>(&body) else {
        return err(StatusCode::BAD_REQUEST, json!({"error": "bad_request"}));
    };
    let result = shared
        .pairing
        .lock()
        .await
        .confirm(&req.pairing_id, &req.code, Instant::now());
    match result {
        Ok(device_name) => {
            let token = match shared
                .store
                .lock()
                .await
                .add(&device_name, SystemTime::now())
            {
                Ok(t) => t,
                Err(e) => {
                    tracing::error!("saving paired device failed: {e}");
                    return err(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        json!({"error": "internal"}),
                    );
                }
            };
            tracing::info!(device = %device_name, "device paired");
            shared.notify(Notice::Paired { device_name });
            (
                [(header::SET_COOKIE, set_cookie(&token, COOKIE_MAX_AGE_SECS))],
                axum::Json(json!({"paired": true})),
            )
                .into_response()
        }
        Err(ConfirmError::BadCode { attempts_left }) => {
            tracing::warn!(attempts_left, "wrong pairing code");
            shared.notify(Notice::PairingFailed { attempts_left });
            err(
                StatusCode::FORBIDDEN,
                json!({"error": "bad_code", "attempts_left": attempts_left}),
            )
        }
        Err(ConfirmError::Expired) => err(StatusCode::GONE, json!({"error": "expired"})),
        Err(ConfirmError::RateLimited) => {
            tracing::warn!("pairing locked after repeated wrong codes");
            err(
                StatusCode::TOO_MANY_REQUESTS,
                json!({"error": "rate_limited"}),
            )
        }
    }
}

async fn unpair(State(shared): State<Arc<Shared>>, headers: HeaderMap) -> Response {
    if !post_ok(&shared, &headers) {
        return err(StatusCode::FORBIDDEN, json!({"error": "bad_origin"}));
    }
    if let Some(token) = cookie_token(&headers) {
        if let Err(e) = shared.store.lock().await.revoke(&token) {
            tracing::error!("revoking device failed: {e}");
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error": "internal"}),
            );
        }
        let mut current = shared.current.lock().await;
        if current.as_ref().is_some_and(|c| c.token == token) {
            let _ = current
                .take()
                .expect("checked")
                .kick
                .send(ByeReason::Revoked);
        }
    }
    (
        [(header::SET_COOKIE, set_cookie("", 0))],
        axum::Json(json!({"paired": false})),
    )
        .into_response()
}

async fn ws_upgrade(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if !origin_ok(&shared, &headers) {
        return err(StatusCode::FORBIDDEN, json!({"error": "bad_origin"}));
    }
    let Some(token) = cookie_token(&headers) else {
        return err(StatusCode::UNAUTHORIZED, json!({"error": "not_paired"}));
    };
    let device = match shared
        .store
        .lock()
        .await
        .authenticate(&token, SystemTime::now())
    {
        Ok(Some(dev)) => dev,
        Ok(None) => return err(StatusCode::UNAUTHORIZED, json!({"error": "not_paired"})),
        Err(e) => {
            tracing::error!("device lookup failed: {e}");
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error": "internal"}),
            );
        }
    };
    ws.max_message_size(MAX_WS_MESSAGE)
        .on_upgrade(move |socket| run_socket(shared, socket, token, device.name))
}

async fn run_socket(
    shared: Arc<Shared>,
    mut socket: WebSocket,
    token: String,
    device_name: String,
) {
    let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
    let (kick_tx, mut kick_rx) = oneshot::channel();
    if let Some(old) = shared.current.lock().await.replace(Current {
        id,
        token,
        kick: kick_tx,
    }) {
        let _ = old.kick.send(ByeReason::Replaced);
    }

    let (in_tx, in_rx) = mpsc::channel(32);
    let (out_tx, mut out_rx) = mpsc::channel(32);
    let welcome = HostSignal::Welcome {
        ice_servers: crate::turn::ice_servers(&shared.config.turn, SystemTime::now()),
    };
    if send_json(&mut socket, &welcome).await.is_err() {
        clear_current(&shared, id).await;
        return;
    }
    let session = SignalingSession {
        device_name: device_name.clone(),
        incoming: in_rx,
        outgoing: out_tx,
    };
    if shared.sessions.send(session).await.is_err() {
        clear_current(&shared, id).await;
        return;
    }
    tracing::info!(device = %device_name, "session started");
    shared.notify(Notice::SessionStarted {
        device_name: device_name.clone(),
    });

    loop {
        tokio::select! {
            reason = &mut kick_rx => {
                if let Ok(reason) = reason {
                    let _ = send_json(&mut socket, &HostSignal::Bye { reason }).await;
                }
                let _ = socket.send(Message::Close(Some(CloseFrame { code: 1000, reason: "bye".into() }))).await;
                break;
            }
            out = out_rx.recv() => {
                // `None`: the host dropped the session.
                let Some(msg) = out else { break };
                if send_json(&mut socket, &msg).await.is_err() {
                    break;
                }
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => match serde_json::from_str::<ClientSignal>(&text) {
                        Ok(msg) => {
                            if in_tx.send(msg).await.is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            let msg = HostSignal::Error { code: "bad_message".into(), message: e.to_string() };
                            if send_json(&mut socket, &msg).await.is_err() {
                                break;
                            }
                        }
                    },
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(_)) => {}
                }
            }
        }
    }

    clear_current(&shared, id).await;
    tracing::info!(device = %device_name, "session ended");
    shared.notify(Notice::SessionEnded { device_name });
}

async fn clear_current(shared: &Shared, id: u64) {
    let mut current = shared.current.lock().await;
    if current.as_ref().is_some_and(|c| c.id == id) {
        *current = None;
    }
}

async fn send_json(socket: &mut WebSocket, msg: &HostSignal) -> Result<(), axum::Error> {
    let text = serde_json::to_string(msg).expect("host messages serialize");
    socket.send(Message::Text(text.into())).await
}
