//! Stand-ins for game audio and the virtual mic, for tests on machines
//! without sound devices (CI, cloud sandboxes). Built on the same Opus
//! encoder and mic jitter buffer as the real ones.
//!
//! [`BeepSource`] plays a 1 kHz beep for the first [`BEEP_MS`] of every
//! wall-clock second (Unix time), so a browser that hears a beep start
//! knows when it was captured: its audio latency is how far past the whole
//! second it heard it. [`MicMeter`] plays the browser's mic into nothing and
//! logs how loud it was, once a second.

use std::f32::consts::TAU;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc;
use ws_proto::AudioPacket;

use crate::convert::{CHANNELS, SAMPLE_RATE};
use crate::encode::OpusFrameEncoder;
use crate::mic::{MicReceiver, mic_channel};
use crate::{AudioError, LoopbackConfig, MicConfig, Result};

/// How long each second's beep lasts.
pub const BEEP_MS: u64 = 100;
const BEEP_HZ: f32 = 1000.0;
/// Peak amplitude of the beep (about -10 dBFS).
const BEEP_LEVEL: f32 = 0.3;
/// How often the threads wake to produce or consume 10 ms blocks.
const TICK: Duration = Duration::from_millis(10);

fn unix_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as u64)
}

/// Samples of the beep pattern for frames `first..first + n`, where frame 0
/// is at Unix time `origin_us`, appended to `out` as interleaved stereo.
fn beep_samples(origin_us: u64, first: u64, n: usize, out: &mut Vec<f32>) {
    for i in 0..n as u64 {
        let frame = first + i;
        let t_us = origin_us + frame * 1_000_000 / u64::from(SAMPLE_RATE);
        let in_second_us = t_us % 1_000_000;
        let s = if in_second_us < BEEP_MS * 1000 {
            let phase = (t_us as f64 / 1_000_000.0 * f64::from(BEEP_HZ)).fract() as f32;
            (phase * TAU).sin() * BEEP_LEVEL
        } else {
            0.0
        };
        for _ in 0..CHANNELS {
            out.push(s);
        }
    }
}

/// The beep, encoded and sent like game audio.
pub struct BeepSource {
    stop: Arc<AtomicBool>,
    bitrate: Arc<AtomicU32>,
    frame_ms: Arc<AtomicU32>,
    thread: Option<JoinHandle<()>>,
}

impl BeepSource {
    pub fn start(config: LoopbackConfig, packets: mpsc::Sender<AudioPacket>) -> Result<Self> {
        let mut encoder = OpusFrameEncoder::new(config.bitrate)?;
        encoder.set_frame_ms(config.frame_ms)?;
        let stop = Arc::new(AtomicBool::new(false));
        let bitrate = Arc::new(AtomicU32::new(config.bitrate));
        let frame_ms = Arc::new(AtomicU32::new(config.frame_ms));
        let thread = std::thread::Builder::new()
            .name("ws-audio-beep".into())
            .spawn({
                let (stop, bitrate, frame_ms) = (stop.clone(), bitrate.clone(), frame_ms.clone());
                move || {
                    let started = Instant::now();
                    let origin_us = unix_micros();
                    let (mut cur_bitrate, mut cur_frame) = (config.bitrate, config.frame_ms);
                    let mut produced: u64 = 0;
                    let mut block = Vec::new();
                    while !stop.load(Ordering::Relaxed) {
                        let (b, f) = (
                            bitrate.load(Ordering::Relaxed),
                            frame_ms.load(Ordering::Relaxed),
                        );
                        if b != cur_bitrate && encoder.set_bitrate(b).is_ok() {
                            cur_bitrate = b;
                        }
                        if f != cur_frame && encoder.set_frame_ms(f).is_ok() {
                            cur_frame = f;
                        }
                        // Every sample due by now, in real time.
                        let due = (started.elapsed().as_micros() as u64) * u64::from(SAMPLE_RATE)
                            / 1_000_000;
                        if due > produced {
                            let n = (due - produced) as usize;
                            block.clear();
                            beep_samples(origin_us, produced, n, &mut block);
                            let at = started
                                + Duration::from_micros(
                                    produced * 1_000_000 / u64::from(SAMPLE_RATE),
                                );
                            produced = due;
                            let pushed = encoder.push(&block, at, |p| {
                                // A full channel drops the packet, as the
                                // real capture does.
                                let _ = packets.try_send(p);
                            });
                            if let Err(e) = pushed {
                                tracing::warn!("beep encode: {e}");
                            }
                            if packets.is_closed() {
                                return;
                            }
                        }
                        std::thread::sleep(TICK);
                    }
                }
            })
            .map_err(|e| AudioError::Platform(format!("spawn beep thread: {e}")))?;
        tracing::info!("synthetic game audio: a {BEEP_MS} ms beep every second");
        Ok(Self {
            stop,
            bitrate,
            frame_ms,
            thread: Some(thread),
        })
    }

    pub fn set_bitrate(&self, bitrate: u32) {
        self.bitrate.store(bitrate, Ordering::Relaxed);
    }

    pub fn set_frame_ms(&self, ms: u32) {
        self.frame_ms.store(ms, Ordering::Relaxed);
    }
}

impl Drop for BeepSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Plays the browser's mic into nothing, at real time, and logs its level
/// once a second (`synthetic mic: peak <dBFS> rms <dBFS>`), so a test can
/// tell the mic reached the host.
pub struct MicMeter {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

fn dbfs(v: f32) -> f32 {
    20.0 * v.max(1e-6).log10()
}

impl MicMeter {
    pub fn start(config: MicConfig) -> Result<(Self, MicReceiver)> {
        let (receiver, mut source) = mic_channel(config.jitter)?;
        let stop = Arc::new(AtomicBool::new(false));
        let stats = receiver.clone();
        let thread = std::thread::Builder::new()
            .name("ws-audio-mic-meter".into())
            .spawn({
                let stop = stop.clone();
                move || {
                    let started = Instant::now();
                    let mut consumed: u64 = 0;
                    let mut buf = Vec::new();
                    let (mut peak, mut sum_sq, mut count) = (0f32, 0f64, 0u64);
                    let mut window = Instant::now();
                    while !stop.load(Ordering::Relaxed) {
                        let due = (started.elapsed().as_micros() as u64) * u64::from(SAMPLE_RATE)
                            / 1_000_000;
                        if due > consumed {
                            buf.resize((due - consumed) as usize * CHANNELS, 0.0);
                            consumed = due;
                            source.fill(&mut buf);
                            for &s in &buf {
                                peak = peak.max(s.abs());
                                sum_sq += f64::from(s) * f64::from(s);
                            }
                            count += buf.len() as u64;
                        }
                        if window.elapsed() >= Duration::from_secs(1) && count > 0 {
                            let rms = (sum_sq / count as f64).sqrt() as f32;
                            let jitter = stats.stats();
                            tracing::info!(
                                "synthetic mic: peak {:.1} dBFS rms {:.1} dBFS ({jitter:?})",
                                dbfs(peak),
                                dbfs(rms),
                            );
                            (peak, sum_sq, count) = (0.0, 0.0, 0);
                            window = Instant::now();
                        }
                        std::thread::sleep(TICK);
                    }
                }
            })
            .map_err(|e| AudioError::Platform(format!("spawn mic thread: {e}")))?;
        Ok((
            Self {
                stop,
                thread: Some(thread),
            },
            receiver,
        ))
    }
}

impl Drop for MicMeter {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beeps_at_the_top_of_each_second() {
        // Start 50 ms before a whole second.
        let origin = 1_000_000 * 1_700_000_000 - 50_000;
        let mut out = Vec::new();
        beep_samples(origin, 0, SAMPLE_RATE as usize / 5, &mut out);
        let at = |ms: usize| out[ms * 48 * CHANNELS..(ms + 5) * 48 * CHANNELS].to_vec();
        let loud = |v: Vec<f32>| v.iter().any(|s| s.abs() > 0.1);
        assert!(!loud(at(0)), "silent before the second");
        assert!(loud(at(60)), "beeping just after it");
        assert!(!loud(at(160)), "silent once the beep is over");
    }

    #[tokio::test]
    async fn sends_opus_packets_in_real_time() {
        let (tx, mut rx) = mpsc::channel(256);
        let beep = BeepSource::start(LoopbackConfig::default(), tx).unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        drop(beep);
        let mut n = 0;
        while rx.try_recv().is_ok() {
            n += 1;
        }
        // 10 ms packets for 300 ms, give or take scheduling.
        assert!((20..=40).contains(&n), "{n} packets");
    }
}
