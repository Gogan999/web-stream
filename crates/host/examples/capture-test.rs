//! Encodes a display to a file and prints frame rate, drops and
//! capture-to-encoded latency once a second. For checking capture and the
//! encoder on their own, without a browser:
//!
//! ```text
//! cargo run --release -p ws-host --example capture-test -- [--display N] [--seconds S]
//!     [--fps F] [--kbps K] [--size WxH] [--codec h264|h265|av1]
//!     [--quality speed|balanced|clarity] [--out FILE]
//! ```
//!
//! It writes the raw bitstream; play it with `ffplay FILE`.

use std::io::Write;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use tokio::sync::mpsc;
use ws_proto::{DisplayId, VideoCodec, VideoConfig, mono_now};

use ws_host::options::{parse_codec, parse_quality, parse_size};

const CAPTURE_TEST_USAGE: &str = "\
capture-test [options]   encode a display to a file and print fps,
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
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    capture_test(&args)
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
    let max = (config.width, config.height);
    let _pipeline = ws_capture::VideoPipeline::start(
        display,
        config,
        max,
        tx,
        ws_proto::SendBacklog::default(),
    )?;
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
