//! The settings page on the host's own HTTPS server: status, pairing, paired devices and settings,
//! for this PC (with the key `web-stream settings` prints) or a paired device that confirms.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use axum::Router;
use axum::extract::{ConnectInfo, DefaultBodyLimit, State};
use axum::http::{Extensions, HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;
use ws_signaling::{SignalingHandle, SignalingServer};

use super::settings::Settings;
use crate::options::{ServeOptions, TUNNEL_TOKEN_FILE};
use crate::serve::{Event, Extension, Notice, PAIRING_WINDOW};

/// The settings page's key, in the network side's data dir.
pub const TOKEN_FILE: &str = "admin-token";
const COOKIE: &str = "__Host-ws_admin";
const LOCAL_SESSION: Duration = Duration::from_secs(12 * 3600);
const DEVICE_SESSION: Duration = Duration::from_secs(3600);
const MAX_BODY: usize = 64 * 1024;
/// The longest line on the link to the service.
pub const MAX_LINE: u64 = 256 * 1024;
/// Pairing addresses given a QR code.
const MAX_CODES: usize = 4;

const PAGE: &str = include_str!("../../admin/index.html");
const SCRIPT: &str = include_str!("../../admin/admin.js");
const STYLE: &str = include_str!("../../admin/admin.css");

/// From the settings page to whoever keeps the settings.
#[derive(Debug, Serialize, Deserialize)]
pub enum AdminRequest {
    Get,
    Save(Box<Settings>),
}

#[derive(Debug, Serialize, Deserialize)]
pub enum AdminReply {
    Settings {
        settings: Box<Settings>,
        audio_devices: Vec<String>,
    },
    Saved,
    Error(String),
}

/// Answers `req` with the settings in `dir`; a save asks for a restart on `restart`.
pub fn handle(dir: &Path, req: AdminRequest, restart: &mpsc::UnboundedSender<()>) -> AdminReply {
    match req {
        AdminRequest::Get => match Settings::load(dir) {
            Ok(settings) => AdminReply::Settings {
                settings: Box::new(settings),
                audio_devices: crate::audio_devices().unwrap_or_default(),
            },
            Err(e) => AdminReply::Error(format!("{e:#}")),
        },
        AdminRequest::Save(settings) => match settings.save(dir) {
            Ok(()) => {
                tracing::info!("settings saved; restarting the host");
                let restart = restart.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    let _ = restart.send(());
                });
                AdminReply::Saved
            }
            Err(e) => AdminReply::Error(format!("{e:#}")),
        },
    }
}

/// Where the settings live.
pub enum Backend {
    /// In this process's data dir; a save restarts the host through the sender.
    Local {
        dir: PathBuf,
        restart: mpsc::UnboundedSender<()>,
    },
    /// With the service, at the other end of a socket.
    Remote(tokio::sync::Mutex<RemoteLink>),
}

pub struct RemoteLink {
    read: BufReader<OwnedReadHalf>,
    write: OwnedWriteHalf,
}

impl Backend {
    pub fn remote(link: UnixStream) -> Self {
        let (read, write) = link.into_split();
        Self::Remote(tokio::sync::Mutex::new(RemoteLink {
            read: BufReader::new(read),
            write,
        }))
    }

    async fn request(&self, req: AdminRequest) -> AdminReply {
        match self {
            Backend::Local { dir, restart } => handle(dir, req, restart),
            Backend::Remote(link) => {
                let mut link = link.lock().await;
                match link.call(&req).await {
                    Ok(reply) => reply,
                    Err(e) => AdminReply::Error(format!("the service did not answer: {e:#}")),
                }
            }
        }
    }
}

impl RemoteLink {
    async fn call(&mut self, req: &AdminRequest) -> anyhow::Result<AdminReply> {
        let mut line = serde_json::to_vec(req)?;
        line.push(b'\n');
        self.write.write_all(&line).await?;
        let line = read_line(&mut self.read).await?.context("link closed")?;
        Ok(serde_json::from_str(&line)?)
    }
}

/// One line of at most [`MAX_LINE`] bytes; `None` at the end.
pub async fn read_line(
    read: &mut (impl tokio::io::AsyncBufRead + Unpin),
) -> anyhow::Result<Option<String>> {
    let mut line = String::new();
    let n = read.take(MAX_LINE).read_line(&mut line).await?;
    if n == 0 {
        return Ok(None);
    }
    anyhow::ensure!(line.ends_with('\n'), "line too long");
    Ok(Some(line))
}

/// The settings page key in `dir`, made on first use.
pub fn load_or_create_token(dir: &Path) -> anyhow::Result<String> {
    let path = dir.join(TOKEN_FILE);
    if let Ok(t) = std::fs::read_to_string(&path) {
        let t = t.trim();
        if t.len() >= 32 {
            return Ok(t.to_string());
        }
    }
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("random key: {e}"))?;
    let token = URL_SAFE_NO_PAD.encode(bytes);
    ws_signaling::write_private(&path, token.as_bytes())
        .with_context(|| format!("{}", path.display()))?;
    Ok(token)
}

/// The settings page, ready to attach to the host's HTTPS server.
pub struct Admin {
    backend: Arc<Backend>,
    data_dir: PathBuf,
    hostnames: Vec<String>,
    remote_admin: bool,
    upnp: Option<Arc<Mutex<String>>>,
    service: bool,
    token: String,
}

impl Admin {
    pub fn new(
        backend: Backend,
        opts: &ServeOptions,
        remote_admin: bool,
        upnp: Option<Arc<Mutex<String>>>,
        service: bool,
    ) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&opts.data_dir)?;
        Ok(Self {
            backend: Arc::new(backend),
            data_dir: opts.data_dir.clone(),
            hostnames: opts.hostnames.clone(),
            remote_admin,
            upnp,
            service,
            token: load_or_create_token(&opts.data_dir)?,
        })
    }
}

impl Extension for Admin {
    fn attach(
        self: Box<Self>,
        server: &mut SignalingServer,
        started: &Event,
        mut notices: mpsc::UnboundedReceiver<Notice>,
    ) -> mpsc::UnboundedReceiver<Notice> {
        let (port, ips, fingerprint, self_signed) = match started {
            Event::Started {
                port,
                ips,
                fingerprint,
                self_signed,
                ..
            } => (*port, ips.clone(), fingerprint.clone(), *self_signed),
            _ => (0, Vec::new(), String::new(), true),
        };
        let key = Sha256::new()
            .chain_update(b"web-stream admin cookie\0")
            .chain_update(self.token.as_bytes())
            .finalize()
            .into();
        let state = Arc::new(AdminState {
            signaling: server.handle(),
            backend: self.backend,
            data_dir: self.data_dir,
            hostnames: self.hostnames,
            remote_admin: self.remote_admin,
            upnp: self.upnp,
            service: self.service,
            token: self.token,
            key,
            port,
            ips,
            fingerprint,
            self_signed,
            live: Mutex::new(Live::default()),
        });
        server.add_routes(routes(state.clone()));
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(n) = notices.recv().await {
                state.live.lock().unwrap().observe(&n);
                if tx.send(n).is_err() {
                    break;
                }
            }
        });
        rx
    }
}

struct AdminState {
    signaling: SignalingHandle,
    backend: Arc<Backend>,
    data_dir: PathBuf,
    hostnames: Vec<String>,
    remote_admin: bool,
    upnp: Option<Arc<Mutex<String>>>,
    service: bool,
    token: String,
    key: [u8; 32],
    port: u16,
    ips: Vec<IpAddr>,
    fingerprint: String,
    self_signed: bool,
    live: Mutex<Live>,
}

/// What the host's notices said last.
#[derive(Default)]
struct Live {
    tunnel_url: Option<String>,
    connected: Option<String>,
    link: Option<(String, Instant)>,
    code: Option<(String, String, Instant)>,
    locked: bool,
}

impl Live {
    fn observe(&mut self, n: &Notice) {
        match n {
            Notice::Tunnel { url } => self.tunnel_url = url.clone(),
            Notice::SessionStarted { device_name } => self.connected = Some(device_name.clone()),
            Notice::SessionEnded { device_name }
                if self.connected.as_ref() == Some(device_name) =>
            {
                self.connected = None
            }
            Notice::PairingCode {
                code,
                device_name,
                expires_in_secs,
            } => {
                let until = Instant::now() + Duration::from_secs(*expires_in_secs);
                self.code = Some((code.clone(), device_name.clone(), until));
            }
            Notice::Paired { .. } => {
                self.link = None;
                self.code = None;
            }
            Notice::PairingLocked => {
                self.code = None;
                self.locked = true;
            }
            _ => {}
        }
    }
}

fn routes(state: Arc<AdminState>) -> Router {
    Router::new()
        .route("/admin", get(page))
        .route("/admin/admin.js", get(script))
        .route("/admin/admin.css", get(style))
        .route("/api/admin/whoami", get(whoami))
        .route("/api/admin/login", post(login))
        .route("/api/admin/confirm", post(confirm))
        .route("/api/admin/logout", post(logout))
        .route("/api/admin/status", get(status))
        .route("/api/admin/pair", post(pair))
        .route("/api/admin/unpair", post(unpair))
        .route("/api/admin/unpair-all", post(unpair_all))
        .route("/api/admin/settings", get(settings).post(save_settings))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(state)
}

async fn page() -> Response {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], PAGE).into_response()
}

async fn script() -> Response {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        SCRIPT,
    )
        .into_response()
}

async fn style() -> Response {
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], STYLE).into_response()
}

fn err(status: StatusCode, message: &str) -> Response {
    (status, axum::Json(json!({"error": message}))).into_response()
}

/// Who may use the page.
#[derive(Debug, PartialEq)]
enum Who {
    /// Someone on this PC with the key.
    Local,
    /// The paired device with this id, which confirmed.
    Device(String),
}

/// A request from this PC itself, not through a tunnel.
fn is_local(headers: &HeaderMap, ext: &Extensions) -> bool {
    !headers.contains_key("cf-connecting-ip")
        && ext
            .get::<ConnectInfo<SocketAddr>>()
            .is_some_and(|c| match c.0.ip() {
                IpAddr::V4(ip) => ip.is_loopback(),
                IpAddr::V6(ip) => {
                    ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
                }
            })
}

/// A JSON POST from the page's own origin.
fn same_origin_json(headers: &HeaderMap) -> bool {
    let json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(';')
                .next()
                .is_some_and(|t| t.trim().eq_ignore_ascii_case("application/json"))
        });
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    json && matches!((origin, host), (Some(o), Some(h)) if o.strip_prefix("https://") == Some(h))
}

fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|kv| kv.trim().strip_prefix(name)?.strip_prefix('='))
        .filter(|v| !v.is_empty())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn mac(key: &[u8; 32], payload: &str) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("any key length");
    mac.update(payload.as_bytes());
    mac
}

/// A signed session: `who`, valid until `expires` (Unix seconds).
fn sign(key: &[u8; 32], who: &Who, expires: u64) -> String {
    let payload = match who {
        Who::Local => format!("l.{expires}.-"),
        Who::Device(id) => format!("d.{expires}.{id}"),
    };
    let tag = mac(key, &payload).finalize().into_bytes();
    format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(payload),
        URL_SAFE_NO_PAD.encode(tag)
    )
}

/// The session in a cookie, if it is ours and still valid at `now`.
fn verify(key: &[u8; 32], value: &str, now: u64) -> Option<Who> {
    let (payload, tag) = value.split_once('.')?;
    let payload = String::from_utf8(URL_SAFE_NO_PAD.decode(payload).ok()?).ok()?;
    let tag = URL_SAFE_NO_PAD.decode(tag).ok()?;
    mac(key, &payload).verify_slice(&tag).ok()?;
    let mut parts = payload.splitn(3, '.');
    let (kind, expires, id) = (parts.next()?, parts.next()?, parts.next()?);
    if expires.parse::<u64>().ok()? < now {
        return None;
    }
    match kind {
        "l" => Some(Who::Local),
        "d" => Some(Who::Device(id.to_string())),
        _ => None,
    }
}

fn session_cookie(value: &str, max_age: u64) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{COOKIE}={value}; Path=/; Max-Age={max_age}; HttpOnly; Secure; SameSite=Strict"
    ))
    .expect("base64url")
}

impl AdminState {
    /// Who is asking, if they may use the page.
    async fn who(&self, headers: &HeaderMap, ext: &Extensions) -> Option<Who> {
        let who = verify(&self.key, cookie_value(headers, COOKIE)?, now_secs())?;
        match &who {
            Who::Local => is_local(headers, ext).then_some(who),
            Who::Device(id) => {
                if !self.remote_admin {
                    return None;
                }
                let device = self.signaling.paired_device(headers).await?;
                (device.id == *id).then_some(who)
            }
        }
    }

    /// The addresses browsers reach the host at.
    fn addresses(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut add = |s: String| {
            if !out.contains(&s) {
                out.push(s);
            }
        };
        if let Some(url) = &self.live.lock().unwrap().tunnel_url {
            add(url.trim_end_matches('/').to_string());
        }
        for ip in self.ips.iter().filter(|ip| !ip.is_loopback()) {
            match ip {
                IpAddr::V4(v4) => add(format!("https://{v4}:{}", self.port)),
                IpAddr::V6(v6) => add(format!("https://[{v6}]:{}", self.port)),
            }
        }
        for name in &self.hostnames {
            add(format!("https://{name}:{}", self.port));
        }
        out
    }
}

/// Fails the request unless it comes from someone who may use the page.
macro_rules! require {
    ($state:expr, $headers:expr, $ext:expr) => {
        match $state.who(&$headers, &$ext).await {
            Some(who) => who,
            None => return err(StatusCode::UNAUTHORIZED, "not_admin"),
        }
    };
    ($state:expr, $headers:expr, $ext:expr, post) => {{
        if !same_origin_json(&$headers) {
            return err(StatusCode::FORBIDDEN, "bad_origin");
        }
        require!($state, $headers, $ext)
    }};
}

async fn whoami(State(s): State<Arc<AdminState>>, headers: HeaderMap, ext: Extensions) -> Response {
    let device = s.signaling.paired_device(&headers).await.map(|d| d.name);
    axum::Json(json!({
        "admin": s.who(&headers, &ext).await.is_some(),
        "local": is_local(&headers, &ext),
        "device": device,
        "remote_admin": s.remote_admin,
        "service": s.service,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct Login {
    token: String,
}

async fn login(
    State(s): State<Arc<AdminState>>,
    headers: HeaderMap,
    ext: Extensions,
    body: String,
) -> Response {
    if !same_origin_json(&headers) {
        return err(StatusCode::FORBIDDEN, "bad_origin");
    }
    if !is_local(&headers, &ext) {
        return err(StatusCode::FORBIDDEN, "not_local");
    }
    let Ok(req) = serde_json::from_str::<Login>(&body) else {
        return err(StatusCode::BAD_REQUEST, "bad_request");
    };
    let ok: bool =
        subtle::ConstantTimeEq::ct_eq(req.token.trim().as_bytes(), s.token.as_bytes()).into();
    if !ok {
        tracing::warn!("settings page: wrong key");
        return err(StatusCode::FORBIDDEN, "bad_key");
    }
    let value = sign(&s.key, &Who::Local, now_secs() + LOCAL_SESSION.as_secs());
    (
        [(
            header::SET_COOKIE,
            session_cookie(&value, LOCAL_SESSION.as_secs()),
        )],
        axum::Json(json!({"admin": true})),
    )
        .into_response()
}

async fn confirm(State(s): State<Arc<AdminState>>, headers: HeaderMap) -> Response {
    if !same_origin_json(&headers) {
        return err(StatusCode::FORBIDDEN, "bad_origin");
    }
    if !s.remote_admin {
        return err(StatusCode::FORBIDDEN, "remote_admin_off");
    }
    let Some(device) = s.signaling.paired_device(&headers).await else {
        return err(StatusCode::UNAUTHORIZED, "not_paired");
    };
    tracing::info!(device = %device.name, "settings page opened by a paired device");
    let who = Who::Device(device.id);
    let value = sign(&s.key, &who, now_secs() + DEVICE_SESSION.as_secs());
    (
        [(
            header::SET_COOKIE,
            session_cookie(&value, DEVICE_SESSION.as_secs()),
        )],
        axum::Json(json!({"admin": true})),
    )
        .into_response()
}

async fn logout(headers: HeaderMap) -> Response {
    if !same_origin_json(&headers) {
        return err(StatusCode::FORBIDDEN, "bad_origin");
    }
    (
        [(header::SET_COOKIE, session_cookie("", 0))],
        axum::Json(json!({"admin": false})),
    )
        .into_response()
}

async fn status(State(s): State<Arc<AdminState>>, headers: HeaderMap, ext: Extensions) -> Response {
    require!(s, headers, ext);
    let devices = s.signaling.devices().await;
    let this_device = s.signaling.paired_device(&headers).await.map(|d| d.id);
    let now = Instant::now();
    let left = |until: Instant| until.checked_duration_since(now).map(|d| d.as_secs());
    let live = s.live.lock().unwrap();
    let pairing = live.link.as_ref().and_then(|(_, until)| left(*until));
    let code = live
        .code
        .as_ref()
        .and_then(|(code, device, until)| left(*until).map(|secs| (code, device, secs)));
    let body = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "service": s.service,
        "port": s.port,
        "fingerprint": s.fingerprint,
        "self_signed": s.self_signed,
        "tunnel_url": live.tunnel_url,
        "connected": live.connected,
        "pairing_secs": pairing,
        "code": code.map(|(c, d, secs)| json!({"code": c, "device": d, "secs": secs})),
        "locked": live.locked,
        "upnp": s.upnp.as_ref().map(|u| u.lock().unwrap().clone()),
        "devices": devices,
        "this_device": this_device,
    });
    drop(live);
    let mut body = body;
    body["addresses"] = json!(s.addresses());
    axum::Json(body).into_response()
}

async fn pair(State(s): State<Arc<AdminState>>, headers: HeaderMap, ext: Extensions) -> Response {
    require!(s, headers, ext, post);
    let link = match s.signaling.open_pairing().await {
        Ok(l) => l,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    {
        let mut live = s.live.lock().unwrap();
        live.link = Some((link.clone(), Instant::now() + PAIRING_WINDOW));
        live.locked = false;
    }
    let codes: Vec<_> = s
        .addresses()
        .into_iter()
        .take(MAX_CODES)
        .map(|origin| {
            let url = format!("{origin}/#pair={link}");
            json!({"url": url, "rows": super::qr::rows(&url)})
        })
        .collect();
    axum::Json(json!({"secs": PAIRING_WINDOW.as_secs(), "codes": codes})).into_response()
}

#[derive(Deserialize)]
struct Unpair {
    id: String,
}

async fn unpair(
    State(s): State<Arc<AdminState>>,
    headers: HeaderMap,
    ext: Extensions,
    body: String,
) -> Response {
    require!(s, headers, ext, post);
    let Ok(req) = serde_json::from_str::<Unpair>(&body) else {
        return err(StatusCode::BAD_REQUEST, "bad_request");
    };
    match s.signaling.revoke_device(&req.id).await {
        Ok(removed) => axum::Json(json!({"removed": removed})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

async fn unpair_all(
    State(s): State<Arc<AdminState>>,
    headers: HeaderMap,
    ext: Extensions,
) -> Response {
    require!(s, headers, ext, post);
    match s.signaling.revoke_all().await {
        Ok(()) => axum::Json(json!({"removed": true})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

async fn settings(
    State(s): State<Arc<AdminState>>,
    headers: HeaderMap,
    ext: Extensions,
) -> Response {
    require!(s, headers, ext);
    match s.backend.request(AdminRequest::Get).await {
        AdminReply::Settings {
            settings,
            audio_devices,
        } => axum::Json(json!({
            "settings": settings,
            "audio_devices": audio_devices,
            "tunnel_token_set": s.data_dir.join(TUNNEL_TOKEN_FILE).exists(),
        }))
        .into_response(),
        AdminReply::Error(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e),
        AdminReply::Saved => err(StatusCode::INTERNAL_SERVER_ERROR, "unexpected reply"),
    }
}

#[derive(Deserialize)]
struct Save {
    settings: Box<Settings>,
    #[serde(default)]
    tunnel_token: String,
}

/// A Cloudflare Tunnel token as pasted: printable ASCII, no spaces.
fn valid_tunnel_token(t: &str) -> bool {
    !t.is_empty() && t.len() <= 4096 && t.bytes().all(|b| b.is_ascii_graphic())
}

async fn save_settings(
    State(s): State<Arc<AdminState>>,
    headers: HeaderMap,
    ext: Extensions,
    body: String,
) -> Response {
    let who = require!(s, headers, ext, post);
    let req = match serde_json::from_str::<Save>(&body) {
        Ok(r) => r,
        Err(e) => return err(StatusCode::BAD_REQUEST, &format!("bad settings: {e}")),
    };
    let token = req.tunnel_token.trim();
    if !token.is_empty() {
        if !valid_tunnel_token(token) {
            return err(StatusCode::BAD_REQUEST, "the tunnel token is not a token");
        }
        if let Err(e) = crate::options::save_tunnel_token(&s.data_dir, token) {
            return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:#}"));
        }
    }
    if req.settings.tunnel == "named" && !s.data_dir.join(TUNNEL_TOKEN_FILE).exists() {
        return err(StatusCode::BAD_REQUEST, "a named tunnel needs its token");
    }
    tracing::info!(?who, "settings page: saving settings");
    match s.backend.request(AdminRequest::Save(req.settings)).await {
        AdminReply::Saved => axum::Json(json!({"saved": true})).into_response(),
        AdminReply::Error(e) => err(StatusCode::BAD_REQUEST, &e),
        AdminReply::Settings { .. } => err(StatusCode::INTERNAL_SERVER_ERROR, "unexpected reply"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_are_signed_and_expire() {
        let key = [7u8; 32];
        let local = sign(&key, &Who::Local, 1000);
        assert_eq!(verify(&key, &local, 999), Some(Who::Local));
        assert_eq!(verify(&key, &local, 1001), None);
        assert_eq!(verify(&[8u8; 32], &local, 999), None);
        let device = sign(&key, &Who::Device("ab12".into()), 1000);
        assert_eq!(verify(&key, &device, 10), Some(Who::Device("ab12".into())));
        let (payload, tag) = device.split_once('.').unwrap();
        let forged = URL_SAFE_NO_PAD.encode("l.99999.-");
        assert_eq!(verify(&key, &format!("{forged}.{tag}"), 10), None);
        assert_eq!(verify(&key, payload, 10), None);
        assert_eq!(verify(&key, "garbage", 10), None);
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(*k, v.parse().unwrap());
        }
        h
    }

    #[test]
    fn only_loopback_without_a_tunnel_is_local() {
        let at = |addr: &str| {
            let mut e = Extensions::new();
            e.insert(ConnectInfo::<SocketAddr>(addr.parse().unwrap()));
            e
        };
        assert!(is_local(&HeaderMap::new(), &at("127.0.0.1:5000")));
        assert!(is_local(&HeaderMap::new(), &at("[::ffff:127.0.0.1]:5000")));
        assert!(!is_local(&HeaderMap::new(), &at("192.168.1.5:5000")));
        assert!(!is_local(&HeaderMap::new(), &Extensions::new()));
        let tunneled = headers(&[("cf-connecting-ip", "203.0.113.9")]);
        assert!(!is_local(&tunneled, &at("127.0.0.1:5000")));
    }

    #[test]
    fn posts_must_be_same_origin_json() {
        let ok = headers(&[
            ("content-type", "application/json"),
            ("origin", "https://localhost:8443"),
            ("host", "localhost:8443"),
        ]);
        assert!(same_origin_json(&ok));
        let foreign = headers(&[
            ("content-type", "application/json"),
            ("origin", "https://evil.example"),
            ("host", "localhost:8443"),
        ]);
        assert!(!same_origin_json(&foreign));
        let form = headers(&[
            ("content-type", "text/plain"),
            ("origin", "https://localhost:8443"),
            ("host", "localhost:8443"),
        ]);
        assert!(!same_origin_json(&form));
        let cookie = headers(&[("cookie", "a=b; __Host-ws_admin=xyz; c=d")]);
        assert_eq!(cookie_value(&cookie, COOKIE), Some("xyz"));
        assert_eq!(cookie_value(&cookie, "__Host-ws_device"), None);
    }

    #[test]
    fn the_key_is_made_once() {
        let dir = tempfile::tempdir().unwrap();
        let a = load_or_create_token(dir.path()).unwrap();
        assert_eq!(load_or_create_token(dir.path()).unwrap(), a);
        assert!(a.len() >= 43);
        assert!(valid_tunnel_token("eyJhIjoiMTIzIn0"));
        assert!(!valid_tunnel_token("a b"));
    }

    #[tokio::test]
    async fn saving_asks_for_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let s = Settings {
            port: 9300,
            ..Settings::default()
        };
        assert!(matches!(
            handle(dir.path(), AdminRequest::Save(Box::new(s)), &tx),
            AdminReply::Saved
        ));
        rx.recv().await.unwrap();
        match handle(dir.path(), AdminRequest::Get, &tx) {
            AdminReply::Settings { settings, .. } => assert_eq!(settings.port, 9300),
            r => panic!("{r:?}"),
        }
        let bad = Settings {
            fps: 0,
            ..Settings::default()
        };
        assert!(matches!(
            handle(dir.path(), AdminRequest::Save(Box::new(bad)), &tx),
            AdminReply::Error(_)
        ));
    }
}
