//! The transport against a plain str0m peer playing the browser, over real
//! UDP on localhost: negotiation, video delivery, data channels both ways and
//! keyframe requests.

use std::time::{Duration, Instant};

use bytes::Bytes;
use str0m::change::SdpOffer;
use str0m::media::{KeyframeRequestKind, MediaKind};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc};
use tokio::net::UdpSocket;
use ws_proto::{DisplayId, EncodedFrame, VideoCodec, channels};
use ws_transport::{Transport, TransportConfig, TransportEvent};

fn frame(index: u64, keyframe: bool) -> EncodedFrame {
    // Annex B: SPS, PPS, then an IDR (or non-IDR) slice.
    let mut data = vec![
        0, 0, 0, 1, 0x67, 0x42, 0xe0, 0x1f, 0, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80,
    ];
    data.extend_from_slice(&[0, 0, 0, 1, if keyframe { 0x65 } else { 0x41 }]);
    data.extend(std::iter::repeat_n(index as u8, 3000));
    EncodedFrame {
        display: DisplayId(0),
        codec: VideoCodec::H264,
        frame_index: index,
        keyframe,
        recovers_from: None,
        capture_time: index * 16_667,
        encode_done_time: index * 16_667 + 2_000,
        width: 1920,
        height: 1080,
        data: Bytes::from(data),
    }
}

#[tokio::test]
async fn browser_peer_gets_video_and_data() {
    let _shared = REAL_TIME.read().await;
    let (transport, mut events) = Transport::start(TransportConfig {
        ips: vec![[127, 0, 0, 1].into()],
        udp_port: 0,
        tcp_port: None,
        ..Default::default()
    })
    .await
    .unwrap();

    let offer = transport.offer().await.unwrap();
    assert!(offer.contains("a=ice-lite"));
    assert!(offer.contains("playout-delay"));
    assert!(offer.contains("H264"));

    // The "browser".
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let local = sock.local_addr().unwrap();
    let mut rtc = Rtc::builder().build(Instant::now());
    rtc.add_local_candidate(Candidate::host(local, "udp").unwrap());
    let answer = rtc
        .sdp_api()
        .accept_offer(SdpOffer::from_sdp_string(&offer).unwrap())
        .unwrap();
    transport
        .accept_answer(answer.to_sdp_string())
        .await
        .unwrap();

    let mut buf = vec![0u8; 2048];
    let mut video_mid = None;
    let mut got_video = 0usize;
    let mut open = Vec::new();
    let mut sent_data = false;
    let mut host_connected = false;
    let mut host_data = Vec::new();
    let mut keyframe_requests = 0;
    let mut asked_keyframe = false;
    let mut next_frame = 0u64;
    let deadline = Instant::now() + Duration::from_secs(10);

    while Instant::now() < deadline {
        // Drain the client.
        let timeout = loop {
            match rtc.poll_output().unwrap() {
                Output::Timeout(t) => break t,
                Output::Transmit(t) => {
                    sock.send_to(&t.contents, t.destination).await.unwrap();
                }
                Output::Event(e) => match e {
                    Event::MediaAdded(m) if m.kind == MediaKind::Video => video_mid = Some(m.mid),
                    Event::MediaData(d) if Some(d.mid) == video_mid => got_video += 1,
                    Event::ChannelOpen(id, label) => open.push((id, label)),
                    _ => {}
                },
            }
        };

        while let Ok(ev) = events.try_recv() {
            match ev {
                TransportEvent::Connected => host_connected = true,
                TransportEvent::Data { channel, data, .. } => host_data.push((channel, data)),
                TransportEvent::KeyframeNeeded => keyframe_requests += 1,
                _ => {}
            }
        }

        if host_connected && next_frame < 30 {
            transport.send_video(frame(next_frame, next_frame == 0));
            next_frame += 1;
        }
        if open.len() == 4 && !sent_data {
            sent_data = true;
            for (id, _) in &open {
                rtc.channel(*id).unwrap().write(true, b"ping").unwrap();
            }
            // Poll before the next mutation.
            continue;
        }
        if got_video > 0 && !asked_keyframe {
            if let Some(mid) = video_mid {
                asked_keyframe = true;
                rtc.writer(mid)
                    .unwrap()
                    .request_keyframe(None, KeyframeRequestKind::Pli)
                    .unwrap();
                continue;
            }
        }
        if got_video > 0 && host_data.len() == 4 && keyframe_requests >= 2 {
            break;
        }

        let wait = timeout
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(5));
        let input = match tokio::time::timeout(wait, sock.recv_from(&mut buf)).await {
            Ok(Ok((n, source))) => Input::Receive(
                Instant::now(),
                Receive {
                    proto: Protocol::Udp,
                    source,
                    destination: local,
                    contents: buf[..n].try_into().unwrap(),
                },
            ),
            _ => Input::Timeout(Instant::now()),
        };
        rtc.handle_input(input).unwrap();
    }

    assert!(host_connected, "host never connected");
    assert!(got_video > 0, "no video reached the peer");
    let mut labels: Vec<_> = open.iter().map(|(_, l)| l.as_str()).collect();
    labels.sort();
    assert_eq!(
        labels,
        [
            channels::CONTROL,
            channels::FILE,
            channels::INPUT,
            channels::INPUT_MOTION
        ]
    );
    assert_eq!(host_data.len(), 4, "data from peer: {host_data:?}");
    assert!(host_data.iter().all(|(_, d)| d == b"ping"));
    // One on connect, one for the PLI.
    assert!(
        keyframe_requests >= 2,
        "keyframe requests: {keyframe_requests}"
    );

    // Host to browser on a channel.
    assert!(
        transport
            .send_data(channels::CONTROL, false, b"hello".to_vec())
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn no_keyframe_requests_without_a_session() {
    let _shared = REAL_TIME.read().await;
    let (transport, mut events) = Transport::start(TransportConfig {
        ips: vec![[127, 0, 0, 1].into()],
        udp_port: 0,
        tcp_port: None,
        ..Default::default()
    })
    .await
    .unwrap();
    // More frames than the queue holds drop the oldest, but with nobody
    // connected there is no one to send a keyframe to.
    for i in 0..5 {
        transport.send_video(frame(i, i == 0));
    }
    let ev = tokio::time::timeout(Duration::from_millis(200), events.recv()).await;
    assert!(ev.is_err(), "unexpected event: {ev:?}");
}

#[tokio::test]
async fn junk_and_bad_tcp_are_ignored() {
    let _shared = REAL_TIME.read().await;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (transport, mut events) = Transport::start(TransportConfig {
        ips: vec![[127, 0, 0, 1].into()],
        udp_port: 0,
        tcp_port: Some(0),
        ..Default::default()
    })
    .await
    .unwrap();
    let offer = transport.offer().await.unwrap();
    let port = |proto: &str| -> u16 {
        offer
            .lines()
            .find(|l| {
                l.starts_with("a=candidate")
                    && l.split_whitespace().nth(2).map(str::to_lowercase) == Some(proto.into())
            })
            .and_then(|l| l.split_whitespace().nth(5))
            .unwrap()
            .parse()
            .unwrap()
    };
    // A UDP flood of junk.
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    for _ in 0..5_000 {
        let _ = sock.send_to(&[0x80; 200], ("127.0.0.1", port("udp"))).await;
    }
    // A TCP connection that sends junk instead of a STUN check is closed.
    let mut tcp = tokio::net::TcpStream::connect(("127.0.0.1", port("tcp")))
        .await
        .unwrap();
    tcp.write_all(&[0, 4, 1, 2, 3, 4]).await.unwrap();
    let mut b = [0u8; 1];
    let read = tokio::time::timeout(Duration::from_secs(1), tcp.read(&mut b)).await;
    assert!(matches!(read, Ok(Ok(0)) | Ok(Err(_))), "{read:?}");
    // The transport is still responsive.
    assert!(transport.offer().await.is_ok());
    while let Ok(ev) = events.try_recv() {
        assert_ne!(ev, TransportEvent::Connected);
    }
}

#[tokio::test]
async fn offer_advertises_public_addresses() {
    let _shared = REAL_TIME.read().await;
    let (transport, _events) = Transport::start(TransportConfig {
        ips: vec![[127, 0, 0, 1].into()],
        public_ips: vec![[198, 51, 100, 7].into()],
        udp_port: 0,
        tcp_port: Some(0),
        ..Default::default()
    })
    .await
    .unwrap();
    let candidates = |sdp: &str| -> Vec<String> {
        sdp.lines()
            .filter(|l| l.starts_with("a=candidate"))
            .map(str::to_string)
            .collect()
    };
    let plain = candidates(&transport.offer().await.unwrap());
    // Host, ICE-TCP, and both again for the configured public address.
    assert_eq!(plain.len(), 4, "{plain:?}");
    assert_eq!(
        plain.iter().filter(|l| l.contains("198.51.100.7")).count(),
        2
    );

    // The address the browser reached us at is added; local and duplicate
    // ones are not.
    let offer = transport
        .offer_reachable_at(vec![
            [203, 0, 113, 9].into(),
            [198, 51, 100, 7].into(),
            [127, 0, 0, 1].into(),
        ])
        .await
        .unwrap();
    let reachable = candidates(&offer);
    assert_eq!(reachable.len(), 6, "{reachable:?}");
    assert_eq!(
        reachable
            .iter()
            .filter(|l| l.contains("203.0.113.9"))
            .count(),
        2
    );
}

/// The browser's bitrate setting in the timed tests, which also caps the
/// encoder there. The str0m peer only NACKs a packet while it is within 100
/// packets of the newest (Chrome: 1000), so at higher rates a lost resend
/// falls out of its window before it asks again, and every frame after it
/// is held for good (a browser would ask for a keyframe). Those tests are
/// about the send queue and resends, not throughput.
const PEER_VIDEO_BPS: u64 = 8_000_000;
/// The transport's ceiling for [`PEER_VIDEO_BPS`], as the host sets it
/// (media.rs set_ceiling: the setting over the headroom, plus audio).
const PEER_MAX_BPS: u64 = PEER_VIDEO_BPS * 100 / 75 + 500_000;

/// A frame of `bytes` total (Annex B, H.264 slice) for the load test.
fn sized_frame(index: u64, keyframe: bool, bytes: usize) -> EncodedFrame {
    let mut f = frame(index, keyframe);
    let mut data = f.data.to_vec();
    data.resize(bytes.max(data.len()), index as u8 | 1);
    f.data = Bytes::from(data);
    f
}

/// The real-time tests stream at full rate for seconds and check timing, so
/// each runs alone: with any other test in this file running, they get too
/// little CPU on a small CI runner. The other tests share the lock.
static REAL_TIME: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());

/// Heavy motion: a constant-bitrate encoder fills every frame to its budget
/// (bitrate / fps), following the bandwidth estimate as the host does. Each keyframe request
/// is a soft "reset" frame on screen, so a steady stream must not need more
/// than the odd one beyond the one that starts it (at 0.9 headroom it needed
/// 20-50). A small allowance absorbs a loaded test machine's scheduling.
/// Once a second two frames arrive together, as after an encoder stall; when
/// the transport kept only the newest unsent frame, each forced a keyframe.
// Multi-threaded like the host, so the peer's loop doesn't delay the
// transport's sends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_frames_at_the_target_bitrate_need_almost_no_keyframes() {
    let _alone = REAL_TIME.write().await;
    // On failure, the transport's debug log says why each keyframe was asked for.
    let _ = tracing_subscriber::fmt()
        .with_env_filter("ws_transport=debug")
        .with_test_writer()
        .try_init();
    const FPS: u64 = 60;
    // The host's BWE_HEADROOM. At 90 this forced 20-50 keyframes here.
    const HEADROOM_PCT: u64 = 75;
    const MAX_KEYFRAMES: u32 = 2;
    const FRAME: Duration = Duration::from_micros(1_000_000 / FPS);
    let (transport, mut events) = Transport::start(TransportConfig {
        ips: vec![[127, 0, 0, 1].into()],
        udp_port: 0,
        tcp_port: None,
        max_bitrate_bps: PEER_MAX_BPS,
        ..Default::default()
    })
    .await
    .unwrap();
    let offer = transport.offer().await.unwrap();
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let local = sock.local_addr().unwrap();
    let mut rtc = Rtc::builder().build(Instant::now());
    rtc.add_local_candidate(Candidate::host(local, "udp").unwrap());
    let answer = rtc
        .sdp_api()
        .accept_offer(SdpOffer::from_sdp_string(&offer).unwrap())
        .unwrap();
    transport
        .accept_answer(answer.to_sdp_string())
        .await
        .unwrap();

    let mut buf = vec![0u8; 2048];
    let mut connected_at = None;
    // What the host's encoder would be set to: under the estimate.
    let mut encoder_bps = TransportConfig::default().initial_bitrate_bps * HEADROOM_PCT / 100;
    let mut keyframe_requests = 0u32;
    let mut index = 0u64;
    let mut next_frame = Instant::now();
    let mut keyframe_due = true;
    let mut held = None;
    let mut got_video = 0usize;

    loop {
        let timeout = loop {
            match rtc.poll_output().unwrap() {
                Output::Timeout(t) => break t,
                Output::Transmit(t) => {
                    sock.send_to(&t.contents, t.destination).await.unwrap();
                }
                Output::Event(Event::MediaData(_)) => got_video += 1,
                Output::Event(_) => {}
            }
        };
        while let Ok(ev) = events.try_recv() {
            match ev {
                TransportEvent::Connected => connected_at = Some(Instant::now()),
                TransportEvent::KeyframeNeeded => {
                    keyframe_due = true;
                    // The one that starts the stream doesn't count.
                    if connected_at.is_some_and(|t| t.elapsed() > Duration::from_millis(500)) {
                        keyframe_requests += 1;
                    }
                }
                TransportEvent::TargetBitrate(bps) => {
                    // As the host (media.rs worth_changing): follow the
                    // estimate down in steps of at least 2%, up in 10%.
                    let target = (bps * HEADROOM_PCT / 100).min(PEER_VIDEO_BPS);
                    let step = if target < encoder_bps { 50 } else { 10 };
                    if target.abs_diff(encoder_bps) * step >= encoder_bps {
                        encoder_bps = target;
                    }
                }
                _ => {}
            }
        }
        let Some(started) = connected_at else {
            // Wait for the connection first.
            let got = recv_until(&sock, &mut buf, timeout).await;
            rtc.handle_input(input(got, &buf, local)).unwrap();
            continue;
        };
        if started.elapsed() > Duration::from_secs(4) {
            break;
        }
        let now = Instant::now();
        if now >= next_frame {
            // A capture clock: never two frames back to back because this
            // loop woke late (Windows timers are coarse). Those come only from
            // the stalls below.
            next_frame = (next_frame + FRAME).max(now + FRAME / 2);
            let budget = (encoder_bps / FPS / 8) as usize;
            // Constant bitrate with fast motion fills every frame; a
            // keyframe overshoots the one-frame VBV to about twice it.
            let bytes = if keyframe_due { budget * 2 } else { budget };
            let frame = sized_frame(index, keyframe_due, bytes);
            keyframe_due = false;
            index += 1;
            // Once a second a frame is held back and handed over with the
            // next one, as from an encoder catching up after a stall: both
            // must go out.
            if index % FPS == 30 {
                held = Some(frame);
            } else {
                if let Some(held) = held.take() {
                    transport.send_video(held);
                }
                transport.send_video(frame);
            }
        }
        let got = recv_until(&sock, &mut buf, timeout).await;
        rtc.handle_input(input(got, &buf, local)).unwrap();
    }
    assert!(got_video > 0, "no video reached the peer");
    assert!(
        keyframe_requests <= MAX_KEYFRAMES,
        "steady full frames forced {keyframe_requests} keyframes in 3.5 s (encoder at {encoder_bps} b/s)"
    );
}

/// Packet loss is repaired by resends (NACK/RTX), so it costs neither a
/// keyframe nor a frame.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lost_packets_are_resent_not_keyframed() {
    let _alone = REAL_TIME.write().await;
    // On failure, the transport's debug log says why each keyframe was asked for.
    let _ = tracing_subscriber::fmt()
        .with_env_filter("ws_transport=debug")
        .with_test_writer()
        .try_init();
    const FPS: u64 = 60;
    // The host's BWE_HEADROOM. At 90 this forced 20-50 keyframes here.
    const HEADROOM_PCT: u64 = 75;
    const MAX_KEYFRAMES: u32 = 2;
    const FRAME: Duration = Duration::from_micros(1_000_000 / FPS);
    let (transport, mut events) = Transport::start(TransportConfig {
        ips: vec![[127, 0, 0, 1].into()],
        udp_port: 0,
        tcp_port: None,
        max_bitrate_bps: PEER_MAX_BPS,
        ..Default::default()
    })
    .await
    .unwrap();
    let offer = transport.offer().await.unwrap();
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let local = sock.local_addr().unwrap();
    let mut rtc = Rtc::builder().build(Instant::now());
    rtc.add_local_candidate(Candidate::host(local, "udp").unwrap());
    let answer = rtc
        .sdp_api()
        .accept_offer(SdpOffer::from_sdp_string(&offer).unwrap())
        .unwrap();
    transport
        .accept_answer(answer.to_sdp_string())
        .await
        .unwrap();

    let mut buf = vec![0u8; 2048];
    let mut connected_at = None;
    // What the host's encoder would be set to: under the estimate.
    let mut encoder_bps = TransportConfig::default().initial_bitrate_bps * HEADROOM_PCT / 100;
    let mut keyframe_requests = 0u32;
    let mut index = 0u64;
    let mut next_frame = Instant::now();
    let mut keyframe_due = true;
    let mut held = None;
    let mut got_video = 0usize;
    let mut broken = 0usize;
    let mut dropped = 0usize;
    let mut big = 0usize;
    // Every 50th media packet (2%) never arrives, resends included.
    let every = 50;

    loop {
        let timeout = loop {
            match rtc.poll_output().unwrap() {
                Output::Timeout(t) => break t,
                Output::Transmit(t) => {
                    sock.send_to(&t.contents, t.destination).await.unwrap();
                }
                Output::Event(Event::MediaData(d)) => {
                    got_video += 1;
                    if !d.contiguous {
                        broken += 1;
                    }
                }
                Output::Event(_) => {}
            }
        };
        while let Ok(ev) = events.try_recv() {
            match ev {
                TransportEvent::Connected => connected_at = Some(Instant::now()),
                TransportEvent::KeyframeNeeded => {
                    keyframe_due = true;
                    // The one that starts the stream doesn't count.
                    if connected_at.is_some_and(|t| t.elapsed() > Duration::from_millis(500)) {
                        keyframe_requests += 1;
                    }
                }
                TransportEvent::TargetBitrate(bps) => {
                    // As the host (media.rs worth_changing): follow the
                    // estimate down in steps of at least 2%, up in 10%.
                    let target = (bps * HEADROOM_PCT / 100).min(PEER_VIDEO_BPS);
                    let step = if target < encoder_bps { 50 } else { 10 };
                    if target.abs_diff(encoder_bps) * step >= encoder_bps {
                        encoder_bps = target;
                    }
                }
                _ => {}
            }
        }
        let Some(started) = connected_at else {
            // Wait for the connection first.
            let got = recv_until(&sock, &mut buf, timeout).await;
            rtc.handle_input(input(got, &buf, local)).unwrap();
            continue;
        };
        if started.elapsed() > Duration::from_secs(4) {
            break;
        }
        let now = Instant::now();
        if now >= next_frame {
            // A capture clock: never two frames back to back because this
            // loop woke late (Windows timers are coarse). Those come only from
            // the stalls below.
            next_frame = (next_frame + FRAME).max(now + FRAME / 2);
            let budget = (encoder_bps / FPS / 8) as usize;
            // Constant bitrate with fast motion fills every frame; a
            // keyframe overshoots the one-frame VBV to about twice it.
            let bytes = if keyframe_due { budget * 2 } else { budget };
            let frame = sized_frame(index, keyframe_due, bytes);
            keyframe_due = false;
            index += 1;
            // Once a second a frame is held back and handed over with the
            // next one, as from an encoder catching up after a stall: both
            // must go out.
            if index % FPS == 30 {
                held = Some(frame);
            } else {
                if let Some(held) = held.take() {
                    transport.send_video(held);
                }
                transport.send_video(frame);
            }
        }
        let got = recv_until(&sock, &mut buf, timeout).await;
        if let Some((n, _)) = got {
            if n > 600 {
                big += 1;
                if big % every == 0 {
                    dropped += 1;
                    continue;
                }
            }
        }
        rtc.handle_input(input(got, &buf, local)).unwrap();
    }
    assert!(dropped > 20, "only {dropped} packets dropped");
    assert_eq!(broken, 0, "{broken} frames arrived with packets missing");
    assert!(
        keyframe_requests <= MAX_KEYFRAMES,
        "losing {dropped} packets forced {keyframe_requests} keyframes"
    );
    assert!(got_video > 200, "only {got_video} frames arrived");
}

/// A frame the transport could not send is skipped over: it asks the
/// encoder for a frame built without it ([`TransportEvent::FramesLost`]),
/// sends that frame, and needs no keyframe. If no such frame comes, it asks
/// for a keyframe instead.
#[tokio::test]
async fn a_dropped_frame_is_skipped_over_without_a_keyframe() {
    // Timed: an unrepaired gap gets a keyframe after the recovery wait.
    let _alone = REAL_TIME.write().await;
    let (transport, mut events) = Transport::start(TransportConfig {
        ips: vec![[127, 0, 0, 1].into()],
        udp_port: 0,
        tcp_port: None,
        ..Default::default()
    })
    .await
    .unwrap();
    let offer = transport.offer().await.unwrap();
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let local = sock.local_addr().unwrap();
    let mut rtc = Rtc::builder().build(Instant::now());
    rtc.add_local_candidate(Candidate::host(local, "udp").unwrap());
    let answer = rtc
        .sdp_api()
        .accept_offer(SdpOffer::from_sdp_string(&offer).unwrap())
        .unwrap();
    transport
        .accept_answer(answer.to_sdp_string())
        .await
        .unwrap();

    // Frame 10 never reaches the transport, as if dropped on the host. The
    // encoder answers the first FramesLost by skipping the gap; frame 30 is
    // dropped too, and that request goes unanswered.
    const SKIPPED: u64 = 10;
    const UNREPAIRED: u64 = 30;
    let mut buf = vec![0u8; 2048];
    let mut connected = false;
    let mut lost = Vec::new();
    let mut keyframe_requests = 0;
    let mut repair_due = None;
    let mut got_video = 0usize;
    let mut index = 0u64;
    let mut next_frame = Instant::now();
    let deadline = Instant::now() + Duration::from_secs(10);

    while Instant::now() < deadline && keyframe_requests < 2 {
        let timeout = loop {
            match rtc.poll_output().unwrap() {
                Output::Timeout(t) => break t,
                Output::Transmit(t) => {
                    sock.send_to(&t.contents, t.destination).await.unwrap();
                }
                Output::Event(Event::MediaData(_)) => got_video += 1,
                Output::Event(_) => {}
            }
        };
        while let Ok(ev) = events.try_recv() {
            match ev {
                TransportEvent::Connected => connected = true,
                TransportEvent::KeyframeNeeded => keyframe_requests += 1,
                TransportEvent::FramesLost(from) => {
                    if lost.is_empty() {
                        repair_due = Some(from);
                    }
                    lost.push(from);
                }
                _ => {}
            }
        }
        let now = Instant::now();
        if connected && now >= next_frame {
            next_frame = now + Duration::from_millis(10);
            let mut f = frame(index, index == 0);
            f.recovers_from = repair_due.take();
            if index != SKIPPED && index != UNREPAIRED {
                transport.send_video(f);
            }
            index += 1;
        }
        let got = recv_until(&sock, &mut buf, timeout).await;
        rtc.handle_input(input(got, &buf, local)).unwrap();
    }

    assert_eq!(lost, [SKIPPED, UNREPAIRED], "frames lost");
    // One to start, one once the second gap went unrepaired.
    assert_eq!(keyframe_requests, 2);
    // Everything up to the second gap arrived but the skipped frame and the
    // one that built on it before the repair came.
    assert!(
        got_video >= UNREPAIRED as usize - 3,
        "{got_video} frames arrived"
    );
}

/// Waits for one datagram until `timeout` (at most 2 ms) and returns its
/// length and source.
async fn recv_until(
    sock: &UdpSocket,
    buf: &mut [u8],
    timeout: Instant,
) -> Option<(usize, std::net::SocketAddr)> {
    let wait = timeout
        .saturating_duration_since(Instant::now())
        .min(Duration::from_millis(2));
    tokio::time::timeout(wait, sock.recv_from(buf))
        .await
        .ok()?
        .ok()
}

fn input(
    got: Option<(usize, std::net::SocketAddr)>,
    buf: &[u8],
    local: std::net::SocketAddr,
) -> Input<'_> {
    match got {
        Some((n, source)) => Input::Receive(
            Instant::now(),
            Receive {
                proto: Protocol::Udp,
                source,
                destination: local,
                contents: buf[..n].try_into().unwrap(),
            },
        ),
        None => Input::Timeout(Instant::now()),
    }
}

/// On a fast link the bandwidth estimate climbs well past 10 Mb/s within a
/// few seconds. The pacer releases packets a fraction of a millisecond
/// apart at these rates; when the run loop slept for each one (timers tick
/// at 1 ms at best) it sent about one packet per tick, every probe measured
/// that, and the estimate (so the encoder) stayed near 8 Mb/s whatever the
/// bitrate setting.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bandwidth_estimate_climbs_past_the_timer_tick() {
    let _alone = REAL_TIME.write().await;
    const WANT_BPS: u64 = 25_000_000;
    let (transport, mut events) = Transport::start(TransportConfig {
        ips: vec![[127, 0, 0, 1].into()],
        udp_port: 0,
        tcp_port: None,
        max_bitrate_bps: 200_000_000,
        ..Default::default()
    })
    .await
    .unwrap();
    let offer = transport.offer().await.unwrap();
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let local = sock.local_addr().unwrap();
    let mut rtc = Rtc::builder().build(Instant::now());
    rtc.add_local_candidate(Candidate::host(local, "udp").unwrap());
    let answer = rtc
        .sdp_api()
        .accept_offer(SdpOffer::from_sdp_string(&offer).unwrap())
        .unwrap();
    transport
        .accept_answer(answer.to_sdp_string())
        .await
        .unwrap();

    let mut buf = vec![0u8; 2048];
    let mut connected_at = None;
    let mut best = 0;
    let mut index = 0u64;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let timeout = loop {
            match rtc.poll_output().unwrap() {
                Output::Timeout(t) => break t,
                Output::Transmit(t) => {
                    sock.send_to(&t.contents, t.destination).await.unwrap();
                }
                Output::Event(_) => {}
            }
        };
        while let Ok(ev) = events.try_recv() {
            match ev {
                TransportEvent::Connected => connected_at = Some(Instant::now()),
                TransportEvent::TargetBitrate(bps) => best = best.max(bps),
                _ => {}
            }
        }
        if let Some(t) = connected_at {
            if best > WANT_BPS || t.elapsed() > Duration::from_secs(4) {
                break;
            }
            // A trickle of small frames, so the video stream is live.
            if index < t.elapsed().as_millis() as u64 / 100 {
                transport.send_video(frame(index, index == 0));
                index += 1;
            }
        }
        // Take every waiting datagram: a peer that reads one per wakeup
        // limits the rate itself.
        let got = recv_until(&sock, &mut buf, timeout).await;
        rtc.handle_input(input(got, &buf, local)).unwrap();
        while let Ok((n, source)) = sock.try_recv_from(&mut buf) {
            rtc.handle_input(input(Some((n, source)), &buf, local))
                .unwrap();
        }
    }
    assert!(connected_at.is_some(), "never connected");
    assert!(
        best > WANT_BPS,
        "bandwidth estimate peaked at {best} b/s on localhost"
    );
}
