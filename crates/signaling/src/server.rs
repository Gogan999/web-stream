use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use axum::Router;
use axum::extract::Request;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::json;
use tokio::sync::{Mutex, mpsc, oneshot};
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::timeout::TimeoutLayer;

use crate::config::{SignalingConfig, TlsSource};
use crate::pairing::{CODE_TTL, ConfirmError, DEFAULT_WINDOW, Pairing, StartError};
use crate::store::DeviceStore;
use crate::{Error, tls};
use ws_proto::{ByeReason, ClientSignal, HostSignal, PairConfirm, PairStart};

/// `__Host-` makes the browser refuse the cookie unless it is Secure, has
/// `Path=/` and no `Domain`, so a sibling subdomain cannot plant one.
const COOKIE: &str = "__Host-ws_device";
const COOKIE_MAX_AGE_SECS: u64 = 365 * 24 * 3600;
const MAX_WS_MESSAGE: usize = 64 * 1024;
const MAX_API_BODY: usize = 4 * 1024;
/// Every HTTP request, including the `/ws` upgrade, must finish in this time.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Server pings keep NATs open; a socket silent for `WS_IDLE` is dropped.
const WS_PING_EVERY: Duration = Duration::from_secs(15);
const WS_IDLE: Duration = Duration::from_secs(45);
/// A browser that stops reading must not stall the session loop.
const WS_SEND_TIMEOUT: Duration = Duration::from_secs(5);
const CSP: &str = "default-src 'self'; connect-src 'self'; img-src 'self' data: blob:; \
                   media-src 'self' blob:; style-src 'self' 'unsafe-inline'; object-src 'none'; \
                   base-uri 'none'; frame-ancestors 'none'; form-action 'self'";
/// The page asks only for the microphone; nothing it embeds or a script
/// injected into it may reach the rest.
const PERMISSIONS_POLICY: &str = "microphone=(self), camera=(), geolocation=(), payment=(), \
                                  usb=(), serial=(), hid=(), display-capture=()";

/// Things the host should show its user.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    /// Too many wrong codes. Pairing stays locked, across restarts, until
    /// the host user opens it again with [`SignalingHandle::open_pairing`].
    PairingLocked,
    SessionStarted {
        device_name: String,
    },
    SessionEnded {
        device_name: String,
    },
    /// A certificate from Let's Encrypt is in use from now on.
    Certificate {
        domain: String,
        fingerprint: String,
    },
    /// Something the host user should know about (a certificate that could
    /// not be issued, say); the host keeps running.
    Warning(String),
}

/// An authenticated browser on the signaling socket. Dropping it closes the
/// socket.
pub struct SignalingSession {
    pub device_name: String,
    /// Which paired device this is: its token's hash. Unlike the name,
    /// which the browser chooses, it tells two devices apart.
    pub device_id: String,
    /// The host name or address the browser used to reach us (its URL's
    /// host, from the `Host` header, without the port). From outside the
    /// LAN this is the WAN address or DDNS name, which the media
    /// candidates must include.
    pub host: Option<String>,
    incoming: mpsc::Receiver<SessionEvent>,
    outgoing: mpsc::Sender<HostSignal>,
}

/// What [`SignalingSession::recv`] reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    Signal(ClientSignal),
    /// The browser sent [`ClientSignal::Handoff`] and the socket is closed.
    /// The session lives on as long as the peer connection does: the host
    /// ends it by dropping this, and nothing more can be sent here.
    HandedOff,
    /// A handed-off session was ended from this side (replaced by a new
    /// connection, revoked, or dropped). Tell the browser over `control`
    /// ([`ws_proto::HostControl::Bye`]) and close the peer connection.
    Ended(ByeReason),
}

impl SignalingSession {
    /// Next event; `None` once the socket is gone (unless it was handed off)
    /// or the session ended.
    pub async fn recv(&mut self) -> Option<SessionEvent> {
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

    /// Host-side action (tray or console): accept one pairing for the next
    /// two minutes. Pairing is otherwise closed, and this is also the only
    /// way to clear a lockout after repeated wrong codes.
    pub async fn open_pairing(&self) -> Result<(), Error> {
        self.open_pairing_for(DEFAULT_WINDOW).await
    }

    pub async fn open_pairing_for(&self, window: Duration) -> Result<(), Error> {
        self.shared
            .pairing
            .lock()
            .await
            .open(Instant::now(), window)?;
        tracing::info!(secs = window.as_secs(), "pairing open");
        Ok(())
    }

    pub async fn close_pairing(&self) {
        self.shared.pairing.lock().await.close();
    }
}

pub struct SignalingServer {
    shared: Arc<Shared>,
    tls: tls::LoadedTls,
}

impl SignalingServer {
    pub fn new(config: SignalingConfig) -> Result<(Self, SignalingEvents), Error> {
        std::fs::create_dir_all(&config.data_dir)?;
        // Also tighten files left by an older build or copied in.
        crate::acl::restrict_tree(&config.data_dir)?;
        let pairing = Pairing::load(&config.data_dir)?;
        if pairing.is_locked() {
            tracing::warn!(
                "pairing is locked after repeated wrong codes; open pairing on the host to unlock"
            );
        }
        let tls = tls::load(&config.tls, &config.data_dir)?;
        let store = DeviceStore::open(&config.data_dir, config.device_idle_expiry)?;
        let (sessions_tx, sessions) = mpsc::channel(4);
        let (notices_tx, notices) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            config,
            store: Mutex::new(store),
            pairing: Mutex::new(pairing),
            current: Mutex::new(None),
            next_id: AtomicU64::new(1),
            sessions: sessions_tx,
            notices: notices_tx,
        });
        Ok((Self { shared, tls }, SignalingEvents { sessions, notices }))
    }

    /// The TLS certificate in use: its SHA-256 fingerprint is for the user
    /// to compare with what the browser shows.
    pub fn certificate(&self) -> crate::CertInfo {
        self.tls.certs.info()
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
        tracing::info!(%addr, fingerprint = %self.tls.certs.info().fingerprint, "signaling listening");
        // Issues and renews the certificate while the server runs; ends
        // with it.
        let _acme = match &self.shared.config.tls {
            TlsSource::DuckDns(duck) => Some(AbortOnDrop(tokio::spawn(crate::acme::maintain(
                self.tls.certs.clone(),
                duck.clone(),
                self.shared.config.data_dir.clone(),
                self.shared.notices.clone(),
            )))),
            _ => None,
        };
        crate::listen::serve(addr, self.tls.config.clone(), app).await
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
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

    let embedded = shared.config.embedded_web;
    let app = match &shared.config.web_root {
        Some(root) => api.fallback_service(ServeDir::new(root)),
        None if !embedded.is_empty() => {
            api.fallback(move |uri: axum::http::Uri| async move { embedded_file(embedded, &uri) })
        }
        None => api.fallback(|| async { Html(PLACEHOLDER) }),
    };

    app.layer(axum::middleware::from_fn(no_cross_site_autoconnect))
        .layer(SetResponseHeaderLayer::overriding(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(CSP),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        // A page another site opens gets no handle on this one, and nothing
        // here loads into another site's page.
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("cross-origin-opener-policy"),
            HeaderValue::from_static("same-origin"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("cross-origin-resource-policy"),
            HeaderValue::from_static("same-origin"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("permissions-policy"),
            HeaderValue::from_static(PERMISSIONS_POLICY),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        ))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            REQUEST_TIMEOUT,
        ))
}

/// `?autoconnect` connects as soon as the page loads, which replaces the
/// live session. A link or redirect from another site must not do that, so
/// it lands on the plain page instead.
async fn no_cross_site_autoconnect(req: Request, next: Next) -> Response {
    if is_cross_site_autoconnect(req.headers(), req.uri()) {
        return (StatusCode::SEE_OTHER, [(header::LOCATION, "/")]).into_response();
    }
    next.run(req).await
}

fn is_cross_site_autoconnect(headers: &HeaderMap, uri: &axum::http::Uri) -> bool {
    let autoconnect = uri.query().is_some_and(|q| {
        q.split('&')
            .any(|kv| kv.split('=').next() == Some("autoconnect"))
    });
    // `none` is a typed URL or a bookmark, `same-origin` the page itself.
    // Browsers too old to send the header can't be told apart; let them be.
    let foreign = headers
        .get("sec-fetch-site")
        .is_some_and(|v| v != "same-origin" && v != "none");
    autoconnect && foreign
}

/// A file of the built-in web client by its URL path, `/` being `index.html`.
fn embedded_file(
    files: &'static [(&'static str, &'static [u8])],
    uri: &axum::http::Uri,
) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    let Some((name, body)) = files.iter().find(|(name, _)| *name == path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mime = match name.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("webmanifest") => "application/manifest+json",
        _ => "application/octet-stream",
    };
    ([(header::CONTENT_TYPE, mime)], *body).into_response()
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
        Ok(Ok(s)) => {
            // The code goes only to the host UI, never to the log.
            tracing::info!(device = %s.device_name, "pairing requested");
            shared.notify(Notice::PairingCode {
                code: s.code,
                device_name: s.device_name,
                expires_in_secs: CODE_TTL.as_secs(),
            });
            axum::Json(json!({"pairing_id": s.pairing_id, "expires_in": CODE_TTL.as_secs()}))
                .into_response()
        }
        Ok(Err(StartError::Closed)) => {
            err(StatusCode::FORBIDDEN, json!({"error": "pairing_closed"}))
        }
        Ok(Err(StartError::Locked)) => err(StatusCode::LOCKED, json!({"error": "locked"})),
        Ok(Err(StartError::Busy)) => err(StatusCode::CONFLICT, json!({"error": "busy"})),
        Ok(Err(StartError::RateLimited)) => err(
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
    let result = match result {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("pairing state: {e}");
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error": "internal"}),
            );
        }
    };
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
        Err(ConfirmError::Locked) => {
            tracing::warn!("pairing locked after repeated wrong codes");
            shared.notify(Notice::PairingLocked);
            err(StatusCode::LOCKED, json!({"error": "locked"}))
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
    let host = request_host(&headers);
    ws.max_message_size(MAX_WS_MESSAGE)
        .max_frame_size(MAX_WS_MESSAGE)
        .on_upgrade(move |socket| run_socket(shared, socket, token, device.name, host))
}

/// The host part of a `Host` header: `example.com:8443` → `example.com`,
/// `[::1]:8443` → `::1`.
fn request_host(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::HOST)?.to_str().ok()?.trim();
    let host = match value.strip_prefix('[') {
        Some(rest) => rest.split_once(']')?.0,
        None => value.split(':').next()?,
    };
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

async fn run_socket(
    shared: Arc<Shared>,
    mut socket: WebSocket,
    token: String,
    device_name: String,
    host: Option<String>,
) {
    let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
    let device_id = crate::store::hash(&token);
    let (kick_tx, mut kick_rx) = oneshot::channel();
    {
        // Re-check the token while holding `current`: a revoke that landed
        // after `ws_upgrade` checked it would otherwise miss this socket.
        // Revokers update the store first and then take `current`, so either
        // they see this session or this check sees the revocation.
        let mut current = shared.current.lock().await;
        if shared
            .store
            .lock()
            .await
            .check(&token, SystemTime::now())
            .is_none()
        {
            drop(current);
            let _ = send_json(
                &mut socket,
                &HostSignal::Bye {
                    reason: ByeReason::Revoked,
                },
            )
            .await;
            return;
        }
        if let Some(old) = current.replace(Current {
            id,
            token,
            kick: kick_tx,
        }) {
            let _ = old.kick.send(ByeReason::Replaced);
        }
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
        device_id,
        host,
        incoming: in_rx,
        outgoing: out_tx,
    };
    if shared.sessions.try_send(session).is_err() {
        tracing::warn!("host is not accepting sessions");
        clear_current(&shared, id).await;
        return;
    }
    tracing::info!(device = %device_name, "session started");
    shared.notify(Notice::SessionStarted {
        device_name: device_name.clone(),
    });

    let mut ping = tokio::time::interval(WS_PING_EVERY);
    ping.tick().await;
    let mut last_heard = Instant::now();
    let mut handed_off = false;
    loop {
        tokio::select! {
            _ = ping.tick() => {
                if last_heard.elapsed() >= WS_IDLE {
                    tracing::info!(device = %device_name, "signaling socket idle");
                    break;
                }
                if send(&mut socket, Message::Ping(Default::default())).await.is_err() {
                    break;
                }
            }
            reason = &mut kick_rx => {
                if let Ok(reason) = reason {
                    let _ = send_json(&mut socket, &HostSignal::Bye { reason }).await;
                }
                let _ = send(&mut socket, Message::Close(Some(CloseFrame { code: 1000, reason: "bye".into() }))).await;
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
                last_heard = Instant::now();
                match incoming {
                    Some(Ok(Message::Text(text))) => match serde_json::from_str::<ClientSignal>(&text) {
                        Ok(ClientSignal::Handoff) => {
                            handed_off = true;
                            break;
                        }
                        Ok(msg) => {
                            // A host that stops reading must not block kicks
                            // and pings; a browser flooding it gets dropped.
                            if in_tx.try_send(SessionEvent::Signal(msg)).is_err() {
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

    if handed_off {
        let _ = send(
            &mut socket,
            Message::Close(Some(CloseFrame {
                code: 1000,
                reason: "handoff".into(),
            })),
        )
        .await;
        drop(socket);
        tracing::info!(device = %device_name, "signaling handed off to the peer connection");
        if in_tx.try_send(SessionEvent::HandedOff).is_ok() {
            run_handed_off(kick_rx, &in_tx, &mut out_rx).await;
        }
    }

    clear_current(&shared, id).await;
    tracing::info!(device = %device_name, "session ended");
    shared.notify(Notice::SessionEnded { device_name });
}

/// A handed-off session stays the live one, so a new connection, a revoke
/// or a drop still ends it, until the host drops it.
async fn run_handed_off(
    kick: oneshot::Receiver<ByeReason>,
    events: &mpsc::Sender<SessionEvent>,
    outgoing: &mut mpsc::Receiver<HostSignal>,
) {
    tokio::pin!(kick);
    loop {
        tokio::select! {
            reason = &mut kick => {
                if let Ok(reason) = reason {
                    let _ = events.try_send(SessionEvent::Ended(reason));
                }
                return;
            }
            // There is no socket to deliver to; `None`: the host dropped it.
            out = outgoing.recv() => if out.is_none() {
                return;
            },
        }
    }
}

async fn clear_current(shared: &Shared, id: u64) {
    let mut current = shared.current.lock().await;
    if current.as_ref().is_some_and(|c| c.id == id) {
        *current = None;
    }
}

async fn send_json(socket: &mut WebSocket, msg: &HostSignal) -> Result<(), ()> {
    let text = serde_json::to_string(msg).expect("host messages serialize");
    send(socket, Message::Text(text.into())).await
}

async fn send(socket: &mut WebSocket, msg: Message) -> Result<(), ()> {
    match tokio::time::timeout(WS_SEND_TIMEOUT, socket.send(msg)).await {
        Ok(Ok(())) => Ok(()),
        _ => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(value: &str) -> Option<String> {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_str(value).unwrap());
        request_host(&headers)
    }

    #[test]
    fn autoconnect_only_from_this_site() {
        let check = |site: Option<&str>, uri: &str| {
            let mut headers = HeaderMap::new();
            if let Some(site) = site {
                headers.insert("sec-fetch-site", HeaderValue::from_str(site).unwrap());
            }
            is_cross_site_autoconnect(&headers, &uri.parse().unwrap())
        };
        assert!(check(Some("cross-site"), "/?autoconnect"));
        assert!(check(Some("same-site"), "/?x=1&autoconnect=1"));
        assert!(!check(Some("same-origin"), "/?autoconnect"));
        assert!(!check(Some("none"), "/?autoconnect"));
        assert!(!check(None, "/?autoconnect"));
        assert!(!check(Some("cross-site"), "/"));
        assert!(!check(Some("cross-site"), "/?noautoconnect"));
    }

    #[test]
    fn host_header_without_port() {
        assert_eq!(host("203.0.113.9:8443").as_deref(), Some("203.0.113.9"));
        assert_eq!(
            host("Home.Example.net:8443").as_deref(),
            Some("home.example.net")
        );
        assert_eq!(
            host("home.example.net").as_deref(),
            Some("home.example.net")
        );
        assert_eq!(host("[2001:db8::1]:8443").as_deref(), Some("2001:db8::1"));
        assert_eq!(host(":8443"), None);
        assert_eq!(request_host(&HeaderMap::new()), None);
    }
}
