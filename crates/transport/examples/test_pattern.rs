//! End-to-end harness: the embedded signaling server plus the transport,
//! streaming a moving AV1 test pattern (software-encoded with rav1e, since
//! test Chromium builds lack H.264) and echoing data channel messages.
//!
//! ```text
//! cargo run -p ws-transport --example test_pattern -- 18443
//! node crates/transport/examples/e2e/run.mjs 18443
//! ```
//!
//! Pairing codes are printed as `PAIRING_CODE <code>` on stdout.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use rav1e::prelude::*;
use ws_proto::{ClientSignal, DisplayId, EncodedFrame, HostSignal, VideoCodec};
use ws_signaling::{Notice, SignalingConfig, SignalingServer, SignalingSession};
use ws_transport::{Transport, TransportConfig, TransportEvent};

const W: usize = 320;
const H: usize = 240;
const FPS: u64 = 30;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,str0m=warn".into()),
        )
        .init();
    let port: u16 = std::env::args()
        .nth(1)
        .map(|p| p.parse())
        .transpose()?
        .unwrap_or(18443);

    let data_dir = std::env::temp_dir().join(format!("ws-test-pattern-{}", std::process::id()));
    let mut config = SignalingConfig::new(([127, 0, 0, 1], port).into(), data_dir);
    config.web_root = Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/e2e"));
    let (server, mut events) = SignalingServer::new(config)?;
    // Stands in for the host user choosing "pair a browser" in the tray.
    server
        .handle()
        .open_pairing_for(Duration::from_secs(600))
        .await?;
    tokio::spawn(server.serve());

    // Fixed media ports (UDP n, ICE-TCP n+1) let a test forward them from
    // a fake public address; 0 picks free ones.
    let media_port: u16 = std::env::var("TEST_MEDIA_PORT")
        .ok()
        .map(|p| p.parse())
        .transpose()?
        .unwrap_or(0);
    let (transport, mut transport_events) = Transport::start(TransportConfig {
        ips: vec![[127, 0, 0, 1].into()],
        udp_port: media_port,
        tcp_port: Some(if media_port == 0 { 0 } else { media_port + 1 }),
        ..Default::default()
    })
    .await?;

    let want_keyframe = Arc::new(AtomicBool::new(true));
    std::thread::spawn({
        let transport = transport.clone();
        let want_keyframe = want_keyframe.clone();
        move || encode_loop(transport, want_keyframe)
    });

    tokio::spawn(async move {
        while let Some(n) = events.notices.recv().await {
            if let Notice::PairingCode { code, .. } = n {
                println!("PAIRING_CODE {code}");
            }
        }
    });

    let echo = transport.clone();
    tokio::spawn(async move {
        while let Some(ev) = transport_events.recv().await {
            match ev {
                TransportEvent::KeyframeNeeded => want_keyframe.store(true, Ordering::Relaxed),
                TransportEvent::Data {
                    channel,
                    binary,
                    data,
                } => {
                    let mut reply = b"echo:".to_vec();
                    reply.extend_from_slice(&data);
                    let _ = echo.send_data(channel, binary, reply).await;
                }
                TransportEvent::Stats(_) | TransportEvent::TargetBitrate(_) => {}
                other => tracing::info!(?other, "transport event"),
            }
        }
    });

    println!("LISTENING https://127.0.0.1:{port}");
    while let Some(session) = events.sessions.recv().await {
        tokio::spawn(run_session(session, transport.clone()));
    }
    Ok(())
}

/// The glue the host agent needs: offer on connect (advertising the address
/// the browser reached us at), apply the answer, pass candidates through,
/// re-offer on restart.
async fn run_session(mut session: SignalingSession, transport: Transport) {
    let mut send_offer = true;
    loop {
        if send_offer {
            send_offer = false;
            let public = session.host.iter().filter_map(|h| h.parse().ok()).collect();
            match transport.offer_reachable_at(public).await {
                Ok(sdp) => {
                    if !session.send(HostSignal::Offer { sdp }).await {
                        break;
                    }
                }
                Err(e) => tracing::error!("offer failed: {e}"),
            }
        }
        match session.recv().await {
            Some(ClientSignal::Answer { sdp }) => {
                if let Err(e) = transport.accept_answer(sdp).await {
                    let msg = HostSignal::Error {
                        code: "bad_answer".into(),
                        message: e.to_string(),
                    };
                    let _ = session.send(msg).await;
                }
            }
            Some(ClientSignal::Candidate { candidate, .. }) => {
                transport.add_remote_candidate(candidate)
            }
            Some(ClientSignal::Restart) => send_offer = true,
            None => break,
        }
    }
    transport.close();
}

fn encode_loop(transport: Transport, want_keyframe: Arc<AtomicBool>) {
    let enc = EncoderConfig {
        width: W,
        height: H,
        time_base: Rational::new(1, FPS),
        low_latency: true,
        speed_settings: SpeedSettings::from_preset(10),
        max_key_frame_interval: 600,
        bitrate: 500_000,
        ..Default::default()
    };
    let mut ctx: Context<u8> = Config::new()
        .with_encoder_config(enc)
        .with_threads(2)
        .new_context()
        .expect("rav1e config");
    let start = Instant::now();
    let mut index = 0u64;
    loop {
        let capture = start.elapsed();
        let mut frame = ctx.new_frame();
        let bar = (index as usize * 4) % W;
        let luma: Vec<u8> = (0..W * H)
            .map(|i| {
                if (i % W).abs_diff(bar) < 16 {
                    235
                } else {
                    16 + ((i / W) % 200) as u8
                }
            })
            .collect();
        frame.planes[0].copy_from_raw_u8(&luma, W, 1);
        let chroma = vec![128u8; (W / 2) * (H / 2)];
        frame.planes[1].copy_from_raw_u8(&chroma, W / 2, 1);
        frame.planes[2].copy_from_raw_u8(&chroma, W / 2, 1);
        let params = FrameParameters {
            frame_type_override: if want_keyframe.swap(false, Ordering::Relaxed) {
                FrameTypeOverride::Key
            } else {
                FrameTypeOverride::No
            },
            ..Default::default()
        };
        if let Err(e) = ctx.send_frame((frame, params)) {
            tracing::warn!("rav1e send_frame: {e:?}");
        }
        while let Ok(pkt) = ctx.receive_packet() {
            transport.send_video(EncodedFrame {
                display: DisplayId(0),
                codec: VideoCodec::Av1,
                frame_index: pkt.input_frameno,
                keyframe: pkt.frame_type == FrameType::KEY,
                recovers_from: None,
                capture_time: capture.as_micros() as u64,
                encode_done_time: start.elapsed().as_micros() as u64,
                width: W as u32,
                height: H as u32,
                data: Bytes::from(pkt.data),
            });
        }
        index += 1;
        let next = start + Duration::from_micros(index * 1_000_000 / FPS);
        std::thread::sleep(next.saturating_duration_since(Instant::now()));
    }
}
