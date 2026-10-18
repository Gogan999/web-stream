//! Host agent entry point.
//!
//! ```text
//! ws-host [serve] [options]   run the host: web client, pairing, signaling,
//!                             WebRTC, capture, audio and input (see --help)
//! ws-host displays
//! ws-host audio-devices
//! ws-host capture-test [--display N] [--seconds S] [--fps F] [--kbps K]
//!                      [--size WxH] [--codec h264|h265|av1]
//!                      [--quality speed|balanced|clarity] [--out FILE]
//! ```
//!
//! `capture-test` writes the raw bitstream (play it with `ffplay FILE`) and
//! prints frame rate, drops and capture-to-encoded latency once a second.

mod console;

use std::io::Write;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use tokio::sync::mpsc;
use ws_proto::{DisplayId, VideoCodec, VideoConfig, mono_now};

use ws_host::options::{self, ServeOptions, parse_codec, parse_quality, parse_size};

const CAPTURE_TEST_USAGE: &str = "\
ws-host capture-test [options]   encode a display to a file and print fps,
                                 drops and latency once a second
  --display N          display to capture, default the primary
  --seconds S          how long, default 10
  --codec h264|h265|av1  --size WxH  --fps F  --kbps K
  --quality speed|balanced|clarity
  --out FILE           default capture-test.<codec>; play it with ffplay FILE
";

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,str0m=warn".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("displays") => list_displays(),
        Some("audio-devices") => list_audio_devices(),
        Some("capture-test") => capture_test(&args[1..]),
        Some("serve") => run_serve(&args[1..]),
        Some(flag) if flag.starts_with('-') => run_serve(&args),
        None => run_serve(&[]),
        Some(other) => bail!("unknown command {other:?}\n\n{}", options::USAGE),
    }
}

fn run_serve(args: &[String]) -> anyhow::Result<()> {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{}", options::USAGE);
        return Ok(());
    }
    let opts = ServeOptions::parse(args)?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(console::run(opts))
}

fn list_displays() -> anyhow::Result<()> {
    for d in ws_capture::list_displays()? {
        println!(
            "{:>3}  {}x{} @ {:.2} Hz at ({}, {}){}  {}",
            d.id.0,
            d.width,
            d.height,
            f64::from(d.refresh_mhz) / 1000.0,
            d.x,
            d.y,
            if d.primary { " primary" } else { "" },
            d.name
        );
    }
    Ok(())
}

fn list_audio_devices() -> anyhow::Result<()> {
    for name in ws_host::audio_devices()? {
        println!("{name}");
    }
    Ok(())
}

fn capture_test(args: &[String]) -> anyhow::Result<()> {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{CAPTURE_TEST_USAGE}");
        return Ok(());
    }
    let mut display = None;
    let mut seconds = 10u64;
    let mut out = None;
    let mut config = VideoConfig::default();
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let mut value = || it.next().with_context(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--display" => display = Some(DisplayId(value()?.parse()?)),
            "--seconds" => seconds = value()?.parse()?,
            "--fps" => config.fps = value()?.parse()?,
            "--kbps" => config.bitrate_kbps = value()?.parse()?,
            "--out" => out = Some(value()?.clone()),
            "--size" => (config.width, config.height) = parse_size(value()?)?,
            "--codec" => config.codec = parse_codec(value()?)?,
            "--quality" => config.quality = parse_quality(value()?)?,
            f => bail!("unknown flag {f}\n\n{CAPTURE_TEST_USAGE}"),
        }
    }
    let display = match display {
        Some(d) => d,
        None => {
            ws_capture::list_displays()?
                .into_iter()
                .find(|d| d.primary)
                .context("no primary display")?
                .id
        }
    };
    let ext = match config.codec {
        VideoCodec::H264 => "h264",
        VideoCodec::H265 => "h265",
        VideoCodec::Av1 => "obu",
    };
    let out = out.unwrap_or_else(|| format!("capture-test.{ext}"));
    let mut file = std::io::BufWriter::new(std::fs::File::create(&out)?);

    let (tx, mut rx) = mpsc::channel(4);
    let _pipeline = ws_capture::VideoPipeline::start(display, config, tx)?;
    println!("capturing display {} to {out} for {seconds}s", display.0);

    let end = Instant::now() + Duration::from_secs(seconds);
    let mut window = Instant::now();
    let (mut frames, mut bytes, mut keyframes, mut latency_us, mut gaps) = (0u64, 0u64, 0, 0u64, 0);
    let mut last_index = None;
    while Instant::now() < end {
        let Some(frame) = rx.blocking_recv() else {
            bail!("pipeline stopped (see log)");
        };
        if let Some(prev) = last_index {
            if frame.frame_index != prev + 1 {
                gaps += 1;
            }
        }
        last_index = Some(frame.frame_index);
        latency_us += mono_now().saturating_sub(frame.capture_time);
        frames += 1;
        bytes += frame.data.len() as u64;
        keyframes += u32::from(frame.keyframe);
        file.write_all(&frame.data)?;

        let elapsed = window.elapsed();
        if elapsed >= Duration::from_secs(1) {
            println!(
                "{:5.1} fps  {:6.0} kbps  capture->here {:5.2} ms  keyframes {keyframes}  drops {gaps}",
                frames as f64 / elapsed.as_secs_f64(),
                bytes as f64 * 8.0 / 1000.0 / elapsed.as_secs_f64(),
                latency_us as f64 / 1000.0 / frames.max(1) as f64,
            );
            (frames, bytes, keyframes, latency_us, gaps) = (0, 0, 0, 0, 0);
            window = Instant::now();
        }
    }
    file.flush()?;
    Ok(())
}
