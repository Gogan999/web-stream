//! The host: the signaling server, the transport and the media side wired
//! together. A front end (the tray app in `ws-host-gui`, the service's
//! control pipe, the test host in `examples`) drives it with [`Command`]s
//! and shows its [`Event`]s.

use std::net::IpAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Context;
use tokio::sync::{mpsc, oneshot, watch};
use ws_proto::{ByeReason, ClientSignal, HostControl, HostSignal, VideoCodec, channels};
pub use ws_signaling::Notice;
use ws_signaling::{
    SessionEvent, SignalingConfig, SignalingHandle, SignalingServer, SignalingSession, TlsSource,
};
use ws_transport::{Transport, TransportConfig};

use crate::files::FileCommand;
pub use crate::files::FileEvent;
use crate::media;
use crate::options::ServeOptions;
use crate::tunnel::Tunnel;
pub use ws_proto::FileId;

/// What the host user asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Let a browser pair for two minutes.
    OpenPairing,
    /// End the current session.
    DropSession,
    /// Forget every paired browser.
    UnpairAll,
    /// Offer these files to the connected browser.
    ShareFiles(Vec<PathBuf>),
    /// Stop offering a shared file.
    StopSharing(FileId),
    /// Stop the host.
    Quit,
}

/// What the host user should see. Serialized for the service's control
/// pipe, which carries it to the tray app.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Event {
    /// Everything is listening.
    Started {
        port: u16,
        /// LAN addresses the host listens on.
        ips: Vec<IpAddr>,
        fingerprint: String,
        /// Browsers warn about the certificate until the user accepts it
        /// (no `--cert`). Through a Cloudflare Tunnel they see Cloudflare's.
        self_signed: bool,
        web_root: Option<PathBuf>,
        data_dir: PathBuf,
    },
    /// Pairing is open for two minutes. `link` is the secret of the pairing
    /// link (`#pair=` on the client's URL), which pairs once without a code;
    /// show it only to the host user (as a QR code), never log it.
    PairingOpened {
        link: String,
    },
    /// Every paired browser was removed.
    UnpairedAll,
    Notice(Notice),
    /// Something happened to a shared file.
    File(FileEvent),
    /// A command failed.
    Error(String),
    /// A browser started, or the host finished, an update
    /// (`--allow-remote-update`).
    Update(String),
}

/// Runs the host until [`Command::Quit`], until every command sender is
/// dropped, or until the signaling server fails. Shutting down drops the
/// session and stops capture, which puts back a changed display resolution.
pub async fn serve(
    opts: ServeOptions,
    mut commands: mpsc::Receiver<Command>,
    events: mpsc::UnboundedSender<Event>,
) -> anyhow::Result<()> {
    let mut net = Network::start(&opts).await?;
    let (media_stop, media_stop_rx) = oneshot::channel();
    let (files, files_rx) = mpsc::channel(8);
    let transport_events = net.transport_events.take().expect("events not taken yet");
    let media_task = tokio::spawn(media::run(
        net.transport.clone(),
        transport_events,
        opts.media.clone(),
        media::Signaled {
            codecs: net.codecs.clone(),
            peers: net.peers.clone(),
        },
        files_rx,
        events.clone(),
        media_stop_rx,
    ));
    let notices = net.notices.take().expect("notices not taken yet");
    tokio::spawn(forward_notices(notices, events.clone()));

    let _ = events.send(net.started.clone());
    if opts.pair_at_start {
        open_pairing(&net.signaling, &events).await;
    }

    let result = loop {
        tokio::select! {
            r = &mut net.server_task => break server_stopped(r),
            cmd = commands.recv() => match cmd {
                Some(Command::OpenPairing) => open_pairing(&net.signaling, &events).await,
                Some(Command::DropSession) => net.signaling.drop_session().await,
                Some(Command::UnpairAll) => unpair_all(&net.signaling, &events).await,
                Some(Command::ShareFiles(paths)) => {
                    let _ = files.send(FileCommand::Share(paths)).await;
                }
                Some(Command::StopSharing(id)) => {
                    let _ = files.send(FileCommand::Stop(id)).await;
                }
                Some(Command::Quit) | None => break Ok(()),
            },
        }
    };

    net.close().await;
    let _ = media_stop.send(());
    let _ = media_task.await;
    net.shutdown().await;
    result
}

/// The network-facing half of the host: the signaling server (HTTPS and
/// the signaling socket) and the transport (the peer connection), with
/// signaling driving the transport. The Windows service runs it in a
/// process of its own, with almost no rights ([`crate::split`]).
pub(crate) struct Network {
    pub signaling: SignalingHandle,
    pub server_task: tokio::task::JoinHandle<Result<(), ws_signaling::Error>>,
    pub transport: Transport,
    /// Taken by whoever runs the media side.
    pub transport_events: Option<ws_transport::Events>,
    pub notices: Option<mpsc::UnboundedReceiver<Notice>>,
    pub codecs: watch::Receiver<Option<Vec<VideoCodec>>>,
    pub peers: watch::Receiver<media::Peer>,
    /// [`Event::Started`], for the front end.
    pub started: Event,
    signaling_stop: Option<oneshot::Sender<()>>,
    sessions: tokio::task::JoinHandle<()>,
    /// cloudflared, while the host runs.
    _tunnel: Option<Tunnel>,
}

impl Network {
    pub async fn start(opts: &ServeOptions) -> anyhow::Result<Self> {
        let mut config = SignalingConfig::new(opts.bind(), opts.data_dir.clone());
        config.web_root = opts.web_root.clone();
        config.embedded_web = crate::web::WEB_FILES;
        config.device_idle_expiry = opts.device_idle_expiry;
        config.extra_origins = opts.extra_origins.clone();
        config.tls = match &opts.cert {
            Some((cert, key)) => TlsSource::Files {
                cert: cert.clone(),
                key: key.clone(),
            },
            None => TlsSource::SelfSigned {
                hostnames: opts.hostnames.clone(),
            },
        };
        let (server, signaling_events) = SignalingServer::new(config)
            .map_err(|e| data_dir_error(e, &opts.data_dir))
            .context("signaling server")?;
        let signaling = server.handle();
        let certificate = server.certificate();
        if certificate.self_signed && opts.tunnel.is_none() {
            tracing::info!(
                "self-signed certificate: browsers warn until it is accepted. For use away from \
                 home, a Cloudflare Tunnel (--tunnel) means no warning to click past"
            );
        }
        let tunnel = opts
            .tunnel
            .clone()
            .map(|t| Tunnel::start(t, opts.port, signaling.clone()));
        let public_ip = match &tunnel {
            Some(t) => t.public_ip(),
            None => watch::channel(None).1,
        };

        let (transport, transport_events) = Transport::start(TransportConfig {
            ips: opts.ips.clone(),
            public_ips: opts.public_ips.clone(),
            udp_port: opts.port,
            tcp_port: opts.ice_tcp_port,
            ..Default::default()
        })
        .await
        .context("transport")?;

        let server_task = tokio::spawn(server.serve());
        let (codecs_tx, codecs) = watch::channel(None);
        let (peers_tx, peers) = watch::channel(media::Peer::default());
        let (signaling_stop, signaling_stop_rx) = oneshot::channel();
        let sessions = tokio::spawn(run_signaling(
            signaling_events.sessions,
            transport.clone(),
            codecs_tx,
            peers_tx,
            public_ip,
            signaling_stop_rx,
        ));
        let started = Event::Started {
            port: opts.port,
            ips: if opts.ips.is_empty() {
                ws_transport::lan_ips()
            } else {
                opts.ips.clone()
            },
            fingerprint: certificate.fingerprint,
            self_signed: certificate.self_signed,
            web_root: opts.web_root.clone(),
            data_dir: opts.data_dir.clone(),
        };
        Ok(Self {
            signaling,
            server_task,
            transport,
            transport_events: Some(transport_events),
            notices: Some(signaling_events.notices),
            codecs,
            peers,
            started,
            signaling_stop: Some(signaling_stop),
            sessions,
            _tunnel: tunnel,
        })
    }

    /// Ends the session (a handed-off browser hears why) and stops
    /// listening; the transport keeps its sockets until [`Network::shutdown`].
    pub async fn close(&mut self) {
        self.signaling.drop_session().await;
        // Lets a handed-off browser hear the `bye` over `control`.
        if let Some(stop) = self.signaling_stop.take() {
            let _ = stop.send(());
        }
        if tokio::time::timeout(Duration::from_secs(1), &mut self.sessions)
            .await
            .is_err()
        {
            self.sessions.abort();
        }
        self.server_task.abort();
        self.transport.close();
    }

    /// Closes the sockets, on the transport's own thread: waits for it, so
    /// a host started right after can bind the same ports.
    pub async fn shutdown(self) {
        if tokio::time::timeout(Duration::from_secs(2), self.transport.shutdown())
            .await
            .is_err()
        {
            tracing::warn!("transport did not stop in time");
        }
    }
}

pub(crate) fn server_stopped(
    r: Result<Result<(), ws_signaling::Error>, tokio::task::JoinError>,
) -> anyhow::Result<()> {
    match r {
        Ok(Ok(())) => Err(anyhow::anyhow!("signaling server stopped")),
        Ok(Err(e)) => Err(e).context("signaling server"),
        Err(e) => Err(e).context("signaling server"),
    }
}

pub(crate) async fn unpair_all(signaling: &SignalingHandle, events: &mpsc::UnboundedSender<Event>) {
    let _ = events.send(match signaling.revoke_all().await {
        Ok(()) => Event::UnpairedAll,
        Err(e) => Event::Error(format!("could not remove paired browsers: {e}")),
    });
}

/// Access denied on the data dir usually means the host once ran as
/// administrator, which left files this user can't open; say how to fix it.
fn data_dir_error(e: ws_signaling::Error, data_dir: &std::path::Path) -> anyhow::Error {
    let denied = matches!(&e, ws_signaling::Error::Io(io)
        if io.kind() == std::io::ErrorKind::PermissionDenied);
    if !denied {
        return e.into();
    }
    anyhow::Error::new(e).context(format!(
        "cannot use the data folder {}: it was probably created by the host running as \
         administrator. Run the host as administrator once to repair it, or delete the \
         folder (paired browsers will have to pair again)",
        data_dir.display()
    ))
}

pub(crate) async fn open_pairing(
    signaling: &SignalingHandle,
    events: &mpsc::UnboundedSender<Event>,
) {
    let _ = events.send(match signaling.open_pairing().await {
        Ok(link) => Event::PairingOpened { link },
        Err(e) => Event::Error(format!("could not open pairing: {e}")),
    });
}

async fn forward_notices(
    mut notices: mpsc::UnboundedReceiver<Notice>,
    events: mpsc::UnboundedSender<Event>,
) {
    while let Some(n) = notices.recv().await {
        if events.send(Event::Notice(n)).is_err() {
            break;
        }
    }
}

/// How long a `bye` sent over `control` gets to reach the browser before
/// the peer connection closes.
const BYE_GRACE: Duration = Duration::from_millis(250);
/// A handed-off session whose peer connection has been down this long is
/// over. Without a socket nothing else says the browser left.
const HANDED_OFF_DOWN_LIMIT: Duration = Duration::from_secs(20);

/// Signaling for one browser at a time: offer on connect, apply the answer,
/// pass candidates through, re-offer on restart. A new session replaces the
/// current one (the server has already told the old browser, unless it had
/// handed signaling off: then it hears it over `control`). A handed-off
/// session ends when the server says so, or when its peer connection stays
/// down.
async fn run_signaling(
    mut sessions: mpsc::Receiver<SignalingSession>,
    transport: Transport,
    codecs: watch::Sender<Option<Vec<VideoCodec>>>,
    peers: watch::Sender<media::Peer>,
    public_ip: watch::Receiver<Option<IpAddr>>,
    mut stop: oneshot::Receiver<()>,
) {
    let mut current: Option<SignalingSession> = None;
    let mut handed_off = false;
    let mut down_since: Option<Instant> = None;
    let mut check = tokio::time::interval(Duration::from_secs(1));
    check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let msg = tokio::select! {
            _ = &mut stop => {
                if handed_off {
                    say_bye(&transport, ByeReason::HostShutdown).await;
                }
                break;
            }
            s = sessions.recv() => {
                let Some(s) = s else { break };
                tracing::info!(device = %s.device_name, "signaling session");
                if current.is_some() && handed_off {
                    say_bye(&transport, ByeReason::Replaced).await;
                }
                let device = s.device_id.clone();
                if peers.borrow().device != device {
                    // The last connection's bandwidth says nothing about
                    // this device's path.
                    transport.forget_estimate();
                }
                current = Some(s);
                handed_off = false;
                // Until this browser answers, its codecs are unknown.
                codecs.send_replace(None);
                peers.send_modify(|p| {
                    p.count += 1;
                    p.device = device;
                });
                let ip = *public_ip.borrow();
                if !send_offer(&transport, current.as_ref(), ip).await {
                    current = None;
                }
                continue;
            }
            _ = check.tick(), if handed_off => {
                if transport.is_connected() {
                    down_since = None;
                } else if down_since.get_or_insert_with(Instant::now).elapsed() >= HANDED_OFF_DOWN_LIMIT {
                    tracing::info!("peer connection stayed down; session over");
                    current = None;
                    handed_off = false;
                    transport.close();
                }
                continue;
            }
            msg = next_signal(&mut current) => msg,
        };
        match msg {
            Some(SessionEvent::Signal(ClientSignal::Answer { sdp })) => {
                let accepted = answer_video_codecs(&sdp);
                tracing::debug!(?accepted, "browser video codecs");
                if let Err(e) = transport.accept_answer(sdp).await {
                    tracing::warn!("bad answer: {e}");
                    send(
                        current.as_ref(),
                        HostSignal::Error {
                            code: "bad_answer".into(),
                            message: e.to_string(),
                        },
                    )
                    .await;
                } else {
                    codecs.send_replace(Some(accepted));
                }
            }
            Some(SessionEvent::Signal(ClientSignal::Candidate { candidate, .. })) => {
                transport.add_remote_candidate(candidate)
            }
            Some(SessionEvent::Signal(ClientSignal::Restart)) => {
                codecs.send_replace(None);
                peers.send_modify(|p| p.count += 1);
                let ip = *public_ip.borrow();
                if !send_offer(&transport, current.as_ref(), ip).await {
                    current = None;
                    transport.close();
                }
            }
            // The server turns this into `HandedOff`.
            Some(SessionEvent::Signal(ClientSignal::Handoff)) => {}
            Some(SessionEvent::HandedOff) => {
                handed_off = true;
                down_since = None;
            }
            Some(SessionEvent::Ended(reason)) => {
                say_bye(&transport, reason).await;
                current = None;
                handed_off = false;
                transport.close();
            }
            None => {
                current = None;
                handed_off = false;
                transport.close();
            }
        }
    }
}

/// Tells a handed-off browser why its session ends, and gives the message a
/// moment to arrive before the caller closes the peer connection.
async fn say_bye(transport: &Transport, reason: ByeReason) {
    let text = serde_json::to_vec(&HostControl::Bye { reason }).expect("bye serializes");
    if matches!(
        transport.send_data(channels::CONTROL, false, text).await,
        Ok(true)
    ) {
        tokio::time::sleep(BYE_GRACE).await;
    }
}

async fn next_signal(session: &mut Option<SignalingSession>) -> Option<SessionEvent> {
    match session {
        Some(s) => s.recv().await,
        None => std::future::pending().await,
    }
}

/// Starts a fresh peer connection and sends its offer, reachable at the
/// address the browser used and at `public_ip` (learned over STUN when a
/// tunnel hides the first). False if the browser is gone.
async fn send_offer(
    transport: &Transport,
    session: Option<&SignalingSession>,
    public_ip: Option<IpAddr>,
) -> bool {
    let mut public = reachable_at(session.and_then(|s| s.host.as_deref())).await;
    public.extend(public_ip.filter(|ip| !public.contains(ip)));
    if !public.is_empty() {
        tracing::info!(?public, "advertising the address the browser used");
    }
    match transport.offer_reachable_at(public).await {
        Ok(sdp) => send(session, HostSignal::Offer { sdp }).await,
        Err(e) => {
            tracing::error!("offer failed: {e}");
            send(
                session,
                HostSignal::Error {
                    code: "offer_failed".into(),
                    message: e.to_string(),
                },
            )
            .await
        }
    }
}

/// The IPv4 addresses behind the host name the browser used (an address
/// already, or a DDNS name to resolve), so the offer advertises where the
/// browser can actually reach us. The transport skips local ones.
async fn reachable_at(host: Option<&str>) -> Vec<std::net::IpAddr> {
    let Some(host) = host else {
        return Vec::new();
    };
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return vec![ip];
    }
    let lookup = tokio::net::lookup_host((host, 0));
    match tokio::time::timeout(std::time::Duration::from_secs(2), lookup).await {
        Ok(Ok(addrs)) => {
            let mut ips: Vec<_> = addrs.map(|a| a.ip()).filter(|ip| ip.is_ipv4()).collect();
            ips.dedup();
            ips
        }
        Ok(Err(e)) => {
            tracing::debug!(%host, "could not resolve: {e}");
            Vec::new()
        }
        Err(_) => {
            tracing::debug!(%host, "resolving timed out");
            Vec::new()
        }
    }
}

async fn send(session: Option<&SignalingSession>, msg: HostSignal) -> bool {
    match session {
        Some(s) => s.send(msg).await,
        None => false,
    }
}

/// The video codecs in an SDP answer's video section (empty if the browser
/// rejected video).
fn answer_video_codecs(sdp: &str) -> Vec<VideoCodec> {
    let mut codecs = Vec::new();
    let mut in_video = false;
    for line in sdp.lines().map(str::trim) {
        if let Some(media) = line.strip_prefix("m=") {
            let mut parts = media.split_whitespace();
            in_video = parts.next() == Some("video") && parts.next() != Some("0");
            continue;
        }
        if !in_video {
            continue;
        }
        let Some(rtpmap) = line.strip_prefix("a=rtpmap:") else {
            continue;
        };
        let name = rtpmap
            .split_whitespace()
            .nth(1)
            .and_then(|enc| enc.split('/').next())
            .unwrap_or("");
        let codec = match name.to_ascii_uppercase().as_str() {
            "H264" => VideoCodec::H264,
            "H265" => VideoCodec::H265,
            "AV1" => VideoCodec::Av1,
            _ => continue,
        };
        if !codecs.contains(&codec) {
            codecs.push(codec);
        }
    }
    codecs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_denied_on_the_data_dir_says_how_to_fix_it() {
        let dir = std::path::Path::new("data");
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let e = data_dir_error(ws_signaling::Error::Io(denied), dir);
        assert!(format!("{e:#}").contains("as administrator once"));
        let other = ws_signaling::Error::Tls("bad".into());
        assert!(!format!("{:#}", data_dir_error(other, dir)).contains("administrator"));
    }

    #[test]
    fn reads_video_codecs_from_answer() {
        let sdp = "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96 98 45\r\n\
                   a=rtpmap:96 H264/90000\r\na=rtpmap:97 rtx/90000\r\n\
                   a=rtpmap:98 H264/90000\r\na=rtpmap:45 AV1/90000\r\n\
                   m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=rtpmap:111 opus/48000/2\r\n";
        assert_eq!(
            answer_video_codecs(sdp),
            vec![VideoCodec::H264, VideoCodec::Av1]
        );
        let rejected = "v=0\r\nm=video 0 UDP/TLS/RTP/SAVPF 96\r\na=rtpmap:96 H264/90000\r\n";
        assert!(answer_video_codecs(rejected).is_empty());
    }
}
