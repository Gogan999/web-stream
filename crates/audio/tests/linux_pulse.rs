//! Loopback capture and the virtual microphone against a running PulseAudio or pipewire-pulse
//! server; skipped when there is none or `pactl` is missing.

#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use opus::{Channels, Decoder, Encoder};
use ws_audio::{LoopbackConfig, MIC_SOURCE, MicConfig, session};

fn pactl(args: &[&str]) -> Option<String> {
    let out = Command::new("pactl").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn server_ready() -> bool {
    let Some(s) = session::active() else {
        return false;
    };
    s.runtime_dir.join("pulse/native").exists() && pactl(&["info"]).is_some()
}

fn tone(frames: usize, amplitude: f32) -> Vec<f32> {
    (0..frames)
        .flat_map(|i| {
            let v = amplitude * (i as f32 * 2.0 * std::f32::consts::PI * 440.0 / 48_000.0).sin();
            [v, v]
        })
        .collect()
}

fn bytes(samples: &[f32]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

fn peak(samples: &[f32]) -> f32 {
    samples.iter().fold(0f32, |m, s| m.max(s.abs()))
}

/// A null sink made the default output for the test.
struct TestSink(String);

impl TestSink {
    fn new() -> Self {
        let index = pactl(&[
            "load-module",
            "module-null-sink",
            "sink_name=ws_test_out",
            "rate=48000",
            "channels=2",
        ])
        .expect("load null sink");
        pactl(&["set-default-sink", "ws_test_out"]).expect("set default sink");
        std::thread::sleep(Duration::from_millis(300));
        Self(index.trim().into())
    }
}

impl Drop for TestSink {
    fn drop(&mut self) {
        let _ = pactl(&["unload-module", &self.0]);
    }
}

#[test]
fn loopback_and_virtual_microphone() {
    if !server_ready() {
        eprintln!("skipped: no audio server in this session");
        return;
    }
    let _sink = TestSink::new();

    let (tx, mut rx) = tokio::sync::mpsc::channel(1024);
    let capture = ws_audio::LoopbackCapture::start(LoopbackConfig::default(), tx).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while rx.try_recv().is_err() {
        assert!(Instant::now() < deadline, "loopback capture never started");
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut play = Command::new("pacat")
        .args([
            "--playback",
            "--raw",
            "--format=float32le",
            "--rate=48000",
            "--channels=2",
            "--device=ws_test_out",
        ])
        .stdin(Stdio::piped())
        .spawn()
        .expect("pacat");
    play.stdin
        .take()
        .unwrap()
        .write_all(&bytes(&tone(48_000, 0.5)))
        .unwrap();
    play.wait().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    drop(capture);
    let mut decoder = Decoder::new(48_000, Channels::Stereo).unwrap();
    let mut pcm = vec![0f32; 5760 * 2];
    let (mut packets, mut loudest) = (0, 0f32);
    while let Ok(p) = rx.try_recv() {
        packets += 1;
        let n = decoder.decode_float(&p.data, &mut pcm, false).unwrap();
        loudest = loudest.max(peak(&pcm[..n * 2]));
    }
    assert!(packets > 50, "only {packets} packets");
    assert!(loudest > 0.2, "the tone was not captured: peak {loudest}");

    let (mic, receiver) = ws_audio::MicPlayback::start(MicConfig::default()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !pactl(&["list", "short", "sources"]).is_some_and(|s| s.contains(MIC_SOURCE)) {
        assert!(
            Instant::now() < deadline,
            "virtual microphone never appeared"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut record = Command::new("parec")
        .args([
            "--raw",
            "--format=float32le",
            "--rate=48000",
            "--channels=2",
            &format!("--device={MIC_SOURCE}"),
        ])
        .stdout(Stdio::piped())
        .spawn()
        .expect("parec");
    let mut stdout = record.stdout.take().unwrap();
    let loudest_heard = Arc::new(AtomicU32::new(0));
    let reader = {
        let loudest_heard = loudest_heard.clone();
        std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            while let Ok(n) = stdout.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                let samples: Vec<f32> = chunk[..n - n % 4]
                    .chunks_exact(4)
                    .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                    .collect();
                loudest_heard.fetch_max(peak(&samples).to_bits(), Ordering::Relaxed);
            }
        })
    };
    let heard = || f32::from_bits(loudest_heard.load(Ordering::Relaxed));
    let mut encoder = Encoder::new(48_000, Channels::Mono, opus::Application::Voip).unwrap();
    let mono: Vec<f32> = tone(960, 0.5).iter().step_by(2).copied().collect();
    let mut buf = [0u8; 1500];
    let deadline = Instant::now() + Duration::from_secs(6);
    let mut seq = 0u16;
    while heard() <= 0.2 && Instant::now() < deadline {
        let n = encoder.encode_float(&mono, &mut buf).unwrap();
        receiver.push(seq, &buf[..n]);
        seq = seq.wrapping_add(1);
        std::thread::sleep(Duration::from_millis(20));
    }
    record.kill().unwrap();
    let _ = record.wait();
    let _ = reader.join();
    assert!(
        heard() > 0.2,
        "the mic did not reach the virtual source in {seq} packets: peak {}",
        heard()
    );

    drop(mic);
    let sources = pactl(&["list", "short", "sources"]).unwrap_or_default();
    assert!(
        !sources.contains(MIC_SOURCE),
        "virtual microphone left behind"
    );
}
