//! `ws-host serve`: the signaling server, the transport and the media side
//! wired together, plus a small console for the host user.

use std::io::BufRead;

use anyhow::Context;
use tokio::sync::{mpsc, oneshot, watch};
use ws_proto::{ClientSignal, HostSignal, VideoCodec};
use ws_signaling::{
    Notice, SignalingConfig, SignalingHandle, SignalingServer, SignalingSession, TlsSource,
};
use ws_transport::{Transport, TransportConfig};

use crate::media;
use crate::options::ServeOptions;

const CONSOLE_HELP: &str = "\
commands: pair (let a browser pair for two minutes), unpair-all, drop (end the session), quit";

pub async fn serve(opts: ServeOptions) -> anyhow::Result<()> {
    let mut config = SignalingConfig::new(opts.bind(), opts.data_dir.clone());
    config.web_root = opts.web_root.clone();
    config.tls = match &opts.cert {
        Some((cert, key)) => TlsSource::Files {
            cert: cert.clone(),
            key: key.clone(),
        },
        None => TlsSource::SelfSigned {
            hostnames: opts.hostnames.clone(),
        },
    };
    let (server, events) = SignalingServer::new(config).context("signaling server")?;
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
    let media_task = tokio::spawn(media::run(
        transport.clone(),
        transport_events,
        opts.media.clone(),
        codecs,
        media_stop_rx,
    ));
    tokio::spawn(print_notices(events.notices));
    let sessions = tokio::spawn(run_signaling(events.sessions, transport.clone(), codecs_tx));

    println!("web-stream host on https://<this PC>:{}/", opts.port);
    println!("certificate SHA-256 {fingerprint}");
    match &opts.web_root {
        Some(dir) => println!("serving the web client from {}", dir.display()),
        None => println!("no web client found (pass --web-root); serving a placeholder page"),
    }
    println!("data in {}", opts.data_dir.display());
    println!("{CONSOLE_HELP}");
    if opts.pair_at_start {
        open_pairing(&signaling).await;
    }

    let mut console = console();
    let mut console_open = true;
    let result = loop {
        tokio::select! {
            r = tokio::signal::ctrl_c() => break r.context("ctrl-c handler"),
            r = &mut server_task => break match r {
                Ok(Ok(())) => Err(anyhow::anyhow!("signaling server stopped")),
                Ok(Err(e)) => Err(e).context("signaling server"),
                Err(e) => Err(e).context("signaling server"),
            },
            cmd = console.recv(), if console_open => match cmd.as_deref().map(str::trim) {
                // stdin closed (run without a console): keep serving.
                None => console_open = false,
                Some("pair" | "p") => open_pairing(&signaling).await,
                Some("drop" | "d") => signaling.drop_session().await,
                Some("unpair-all") => match signaling.revoke_all().await {
                    Ok(()) => println!("every paired browser was removed"),
                    Err(e) => println!("could not remove paired browsers: {e}"),
                },
                Some("quit" | "q" | "exit") => break Ok(()),
                Some("") => {}
                Some(_) => println!("{CONSOLE_HELP}"),
            },
        }
    };

    signaling.drop_session().await;
    sessions.abort();
    transport.close();
    let _ = media_stop.send(());
    let _ = media_task.await;
    result
}

async fn open_pairing(signaling: &SignalingHandle) {
    match signaling.open_pairing().await {
        Ok(()) => println!("pairing is open for two minutes: request a code in the browser"),
        Err(e) => println!("could not open pairing: {e}"),
    }
}

async fn print_notices(mut notices: mpsc::UnboundedReceiver<Notice>) {
    while let Some(n) = notices.recv().await {
        match n {
            Notice::PairingCode {
                code,
                device_name,
                expires_in_secs,
            } => println!(
                "\n  PAIRING CODE {code}  for {device_name:?} (expires in {expires_in_secs}s)\n"
            ),
            Notice::Paired { device_name } => println!("paired {device_name:?}"),
            Notice::PairingFailed { attempts_left } => {
                println!("wrong pairing code; {attempts_left} attempts left")
            }
            Notice::PairingLocked => {
                println!("too many wrong codes: pairing is locked until you type `pair`")
            }
            Notice::SessionStarted { device_name } => println!("{device_name:?} connected"),
            Notice::SessionEnded { device_name } => println!("{device_name:?} disconnected"),
        }
    }
}

/// Lines typed on stdin. A thread, since tokio's stdin would block shutdown.
fn console() -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel(8);
    std::thread::Builder::new()
        .name("ws-console".into())
        .spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if tx.blocking_send(line).is_err() {
                    break;
                }
            }
        })
        .expect("spawn console thread");
    rx
}

/// Signaling for one browser at a time: offer on connect, apply the answer,
/// pass candidates through, re-offer on restart. A new session replaces the
/// current one (the server has already told the old browser).
async fn run_signaling(
    mut sessions: mpsc::Receiver<SignalingSession>,
    transport: Transport,
    codecs: watch::Sender<Option<Vec<VideoCodec>>>,
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
