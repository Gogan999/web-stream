//! The host: the signaling server, the transport and the media side wired
//! together. A front end (the console in `ws-host`, the tray app in
//! `ws-host-gui`) drives it with [`Command`]s and shows its [`Event`]s.

use std::net::IpAddr;
use std::path::PathBuf;

use anyhow::Context;
use tokio::sync::{mpsc, oneshot, watch};
use ws_proto::{ClientSignal, HostSignal, VideoCodec};
pub use ws_signaling::Notice;
use ws_signaling::{
    SignalingConfig, SignalingHandle, SignalingServer, SignalingSession, TlsSource,
};
use ws_transport::{Transport, TransportConfig};

use crate::files::FileCommand;
pub use crate::files::FileEvent;
use crate::media;
use crate::options::ServeOptions;
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

/// What the host user should see.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Everything is listening.
    Started {
        port: u16,
        /// LAN addresses the host listens on.
        ips: Vec<IpAddr>,
        fingerprint: String,
        web_root: Option<PathBuf>,
        data_dir: PathBuf,
    },
    /// Pairing is open for two minutes.
    PairingOpened,
    /// Every paired browser was removed.
    UnpairedAll,
    Notice(Notice),
    /// Something happened to a shared file.
    File(FileEvent),
    /// A command failed.
    Error(String),
}

/// Runs the host until [`Command::Quit`], until every command sender is
/// dropped, or until the signaling server fails. Shutting down drops the
/// session and stops capture, which puts back a changed display resolution.
pub async fn serve(
    opts: ServeOptions,
    mut commands: mpsc::Receiver<Command>,
    events: mpsc::UnboundedSender<Event>,
) -> anyhow::Result<()> {
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
    let fingerprint = server.fingerprint().to_string();

    let (transport, transport_events) = Transport::start(TransportConfig {
        ips: opts.ips.clone(),
        public_ips: opts.public_ips.clone(),
        udp_port: opts.port,
        tcp_port: opts.ice_tcp_port,
        ..Default::default()
    })
    .await
    .context("transport")?;

    let mut server_task = tokio::spawn(server.serve());
    let (media_stop, media_stop_rx) = oneshot::channel();
    let (codecs_tx, codecs) = watch::channel(None);
    let (peers_tx, peers) = watch::channel(0);
    let (files, files_rx) = mpsc::channel(8);
    let media_task = tokio::spawn(media::run(
        transport.clone(),
        transport_events,
        opts.media.clone(),
        media::Signaled { codecs, peers },
        files_rx,
        events.clone(),
        media_stop_rx,
    ));
    tokio::spawn(forward_notices(signaling_events.notices, events.clone()));
    let sessions = tokio::spawn(run_signaling(
        signaling_events.sessions,
        transport.clone(),
        codecs_tx,
        peers_tx,
    ));

    let _ = events.send(Event::Started {
        port: opts.port,
        ips: if opts.ips.is_empty() {
            ws_transport::lan_ips()
        } else {
            opts.ips.clone()
        },
        fingerprint,
        web_root: opts.web_root.clone(),
        data_dir: opts.data_dir.clone(),
    });
    if opts.pair_at_start {
        open_pairing(&signaling, &events).await;
    }

    let result = loop {
        tokio::select! {
            r = &mut server_task => break match r {
                Ok(Ok(())) => Err(anyhow::anyhow!("signaling server stopped")),
                Ok(Err(e)) => Err(e).context("signaling server"),
                Err(e) => Err(e).context("signaling server"),
            },
            cmd = commands.recv() => match cmd {
                Some(Command::OpenPairing) => open_pairing(&signaling, &events).await,
                Some(Command::DropSession) => signaling.drop_session().await,
                Some(Command::UnpairAll) => {
                    let _ = events.send(match signaling.revoke_all().await {
                        Ok(()) => Event::UnpairedAll,
                        Err(e) => Event::Error(format!("could not remove paired browsers: {e}")),
                    });
                }
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

    signaling.drop_session().await;
    sessions.abort();
    server_task.abort();
    transport.close();
    let _ = media_stop.send(());
    let _ = media_task.await;
    result
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

async fn open_pairing(signaling: &SignalingHandle, events: &mpsc::UnboundedSender<Event>) {
    let _ = events.send(match signaling.open_pairing().await {
        Ok(()) => Event::PairingOpened,
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

/// Signaling for one browser at a time: offer on connect, apply the answer,
/// pass candidates through, re-offer on restart. A new session replaces the
/// current one (the server has already told the old browser).
async fn run_signaling(
    mut sessions: mpsc::Receiver<SignalingSession>,
    transport: Transport,
    codecs: watch::Sender<Option<Vec<VideoCodec>>>,
    peers: watch::Sender<u64>,
) {
    let mut current: Option<SignalingSession> = None;
    loop {
        let msg = tokio::select! {
            s = sessions.recv() => {
                let Some(s) = s else { break };
                tracing::info!(device = %s.device_name, "signaling session");
                current = Some(s);
                // Until this browser answers, its codecs are unknown.
                codecs.send_replace(None);
                peers.send_modify(|n| *n += 1);
                if !send_offer(&transport, current.as_ref()).await {
                    current = None;
                }
                continue;
            }
            msg = next_signal(&mut current) => msg,
        };
        match msg {
            Some(ClientSignal::Answer { sdp }) => {
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
            Some(ClientSignal::Candidate { candidate, .. }) => {
                transport.add_remote_candidate(candidate)
            }
            Some(ClientSignal::Restart) => {
                codecs.send_replace(None);
                peers.send_modify(|n| *n += 1);
                if !send_offer(&transport, current.as_ref()).await {
                    current = None;
                    transport.close();
                }
            }
            None => {
                current = None;
                transport.close();
            }
        }
    }
}

async fn next_signal(session: &mut Option<SignalingSession>) -> Option<ClientSignal> {
    match session {
        Some(s) => s.recv().await,
        None => std::future::pending().await,
    }
}

/// Starts a fresh peer connection and sends its offer. False if the browser
/// is gone.
async fn send_offer(transport: &Transport, session: Option<&SignalingSession>) -> bool {
    let public = reachable_at(session.and_then(|s| s.host.as_deref())).await;
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
