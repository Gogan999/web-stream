//! Linux audio through the signed-in user's PulseAudio or pipewire-pulse server: loopback of the
//! default output's monitor, and the browser mic played into a virtual source.

mod pulse;
pub mod session;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use ws_proto::AudioPacket;

use crate::convert::{CHANNELS, Converter, InputFormat, SAMPLE_RATE, SampleFormat};
use crate::encode::{FRAME_DURATION, FRAME_SAMPLES, OpusFrameEncoder};
use crate::mic::{MicSource, mic_channel};
use crate::{AUTO_MIC_DEVICE, AudioError, LoopbackConfig, MicConfig, MicReceiver, Result};
use pulse::Conn;
use session::Session;

/// Silence is sent after this long without audio.
const SILENCE_AFTER: Duration = Duration::from_millis(30);
/// At most 100 ms of silence per fill.
const MAX_SILENCE_FRAMES: usize = 10;
/// How often the session and the default output are checked.
const CHECK_EVERY: Duration = Duration::from_secs(1);
/// Wait before connecting again after a failure.
const RETRY: Duration = Duration::from_secs(2);
/// Mainloop wake-up interval.
const TICK: Duration = Duration::from_millis(5);

/// Null sink the mic is played into.
pub const MIC_SINK: &str = "web_stream_mic";
/// The virtual microphone apps record from.
pub const MIC_SOURCE: &str = "web_stream_microphone";
/// What apps show for the virtual microphone.
pub const MIC_NAME: &str = "web-stream microphone";

const NATIVE: InputFormat = InputFormat {
    format: SampleFormat::F32,
    channels: CHANNELS,
    rate: SAMPLE_RATE,
};

fn server(session: &Session) -> Option<String> {
    let socket = session.runtime_dir.join("pulse/native");
    socket
        .exists()
        .then(|| format!("unix:{}", socket.display()))
}

fn same_session(a: &Session, b: Option<&Session>) -> bool {
    b.is_some_and(|b| b.uid == a.uid && b.id == a.id)
}

/// The first output whose name or description contains `wanted` (case-insensitive).
fn find_sink(conn: &Conn, wanted: &str) -> Result<String> {
    let sinks = conn.sinks()?;
    let lower = wanted.to_lowercase();
    sinks
        .iter()
        .find(|(name, desc)| {
            name.to_lowercase().contains(&lower) || desc.to_lowercase().contains(&lower)
        })
        .map(|(name, _)| name.clone())
        .ok_or_else(|| {
            let names: Vec<&str> = sinks.iter().map(|(_, d)| d.as_str()).collect();
            AudioError::DeviceNotFound(format!(
                "no output device matching {wanted:?}; available: {}",
                names.join(", ")
            ))
        })
}

/// Names of the signed-in user's output devices.
pub fn list_render_devices() -> Result<Vec<String>> {
    let session =
        session::active().ok_or_else(|| AudioError::Platform("nobody is signed in".into()))?;
    let server = server(&session)
        .ok_or_else(|| AudioError::Platform("no audio server in the session".into()))?;
    let conn = Conn::connect(&server, "web-stream")?;
    Ok(conn
        .sinks()?
        .into_iter()
        .filter(|(name, _)| name != MIC_SINK)
        .map(|(_, desc)| desc)
        .collect())
}

/// Encoder settings the capture thread picks up while it runs.
struct Tuning {
    bitrate: Arc<AtomicU32>,
    frame_ms: Arc<AtomicU32>,
}

/// Captures what the signed-in user's PC plays and sends it as 10 or 20 ms Opus packets.
pub struct LoopbackCapture {
    stop: Arc<AtomicBool>,
    bitrate: Arc<AtomicU32>,
    frame_ms: Arc<AtomicU32>,
    thread: Option<JoinHandle<()>>,
}

impl LoopbackCapture {
    /// Starts capturing; until someone signs in, silence is sent.
    pub fn start(config: LoopbackConfig, packets: mpsc::Sender<AudioPacket>) -> Result<Self> {
        if !pulse::available() {
            return Err(AudioError::Platform(
                "libpulse is not installed (install PipeWire's pulse support or PulseAudio)".into(),
            ));
        }
        let mut encoder = OpusFrameEncoder::new(config.bitrate)?;
        encoder.set_frame_ms(config.frame_ms)?;
        let stop = Arc::new(AtomicBool::new(false));
        let bitrate = Arc::new(AtomicU32::new(config.bitrate));
        let frame_ms = Arc::new(AtomicU32::new(config.frame_ms));
        let tuning = Tuning {
            bitrate: bitrate.clone(),
            frame_ms: frame_ms.clone(),
        };
        let thread_stop = stop.clone();
        let thread = std::thread::Builder::new()
            .name("ws-audio-loopback".into())
            .spawn(move || run_loopback(config, packets, encoder, &thread_stop, &tuning))
            .map_err(|e| AudioError::Platform(e.to_string()))?;
        Ok(Self {
            stop,
            bitrate,
            frame_ms,
            thread: Some(thread),
        })
    }

    /// Changes the Opus bitrate from the next packet on.
    pub fn set_bitrate(&self, bits_per_second: u32) {
        self.bitrate.store(bits_per_second, Ordering::Relaxed);
    }

    /// Changes the Opus frame length (one of [`crate::FRAME_MS`]) from the next frame on.
    pub fn set_frame_ms(&self, ms: u32) {
        self.frame_ms.store(ms, Ordering::Relaxed);
    }
}

impl Drop for LoopbackCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The encoder, its live settings and the silence that fills gaps.
struct Encode<'a> {
    encoder: OpusFrameEncoder,
    tuning: &'a Tuning,
    bitrate: u32,
    frame_ms: u32,
    audio_end: Instant,
    samples: Vec<f32>,
    send: Box<dyn FnMut(AudioPacket) + 'a>,
}

impl Encode<'_> {
    fn retune(&mut self) -> Result<()> {
        let wanted = self.tuning.bitrate.load(Ordering::Relaxed);
        if wanted != self.bitrate {
            self.encoder.set_bitrate(wanted)?;
            self.bitrate = wanted;
        }
        let wanted = self.tuning.frame_ms.load(Ordering::Relaxed);
        if wanted != self.frame_ms {
            self.encoder.set_frame_ms(wanted)?;
            self.frame_ms = wanted;
        }
        Ok(())
    }

    /// Encodes `self.samples`, captured over the time up to now.
    fn push_samples(&mut self) -> Result<()> {
        let now = Instant::now();
        let frames = self.samples.len() / CHANNELS;
        let captured_at = now
            .checked_sub(Duration::from_secs_f64(
                frames as f64 / f64::from(SAMPLE_RATE),
            ))
            .unwrap_or(now);
        self.encoder
            .push(&self.samples, captured_at, &mut *self.send)?;
        self.audio_end = now;
        Ok(())
    }

    /// Sends silence for a gap in the audio, as the Windows loopback does.
    fn fill_gap(&mut self) -> Result<()> {
        let now = Instant::now();
        if now.duration_since(self.audio_end) < SILENCE_AFTER {
            return Ok(());
        }
        let gap = now.duration_since(self.audio_end) - FRAME_DURATION;
        let frames = (gap.as_micros() / FRAME_DURATION.as_micros()) as usize;
        let capped = frames.min(MAX_SILENCE_FRAMES);
        if capped > 0 {
            self.samples.clear();
            self.samples.resize(capped * FRAME_SAMPLES * CHANNELS, 0.0);
            self.encoder
                .push(&self.samples, self.audio_end, &mut *self.send)?;
            self.audio_end = if capped < frames {
                now
            } else {
                self.audio_end + FRAME_DURATION * capped as u32
            };
        }
        Ok(())
    }

    /// Sends silence for `d` or until `stop`.
    fn idle(&mut self, d: Duration, stop: &AtomicBool) -> Result<()> {
        let until = Instant::now() + d;
        while Instant::now() < until && !stop.load(Ordering::Relaxed) {
            std::thread::sleep(FRAME_DURATION);
            self.retune()?;
            self.fill_gap()?;
        }
        Ok(())
    }
}

enum End {
    Stopped,
    Reopen,
}

fn run_loopback(
    config: LoopbackConfig,
    packets: mpsc::Sender<AudioPacket>,
    encoder: OpusFrameEncoder,
    stop: &AtomicBool,
    tuning: &Tuning,
) {
    let mut dropped = 0u64;
    let send = move |p: AudioPacket| {
        if packets.try_send(p).is_err() {
            dropped += 1;
            if dropped.is_power_of_two() {
                debug!(dropped, "audio packet dropped, consumer is behind or gone");
            }
        }
    };
    let mut enc = Encode {
        bitrate: tuning.bitrate.load(Ordering::Relaxed),
        frame_ms: tuning.frame_ms.load(Ordering::Relaxed),
        encoder,
        tuning,
        audio_end: Instant::now(),
        samples: Vec::with_capacity(SAMPLE_RATE as usize / 50 * CHANNELS),
        send: Box::new(send),
    };
    let mut last_error = String::new();
    while !stop.load(Ordering::Relaxed) {
        let Some((session, server)) =
            session::active().and_then(|s| Some((s.clone(), server(&s)?)))
        else {
            if enc.idle(CHECK_EVERY, stop).is_err() {
                break;
            }
            continue;
        };
        match capture(&config, &session, &server, &mut enc, stop) {
            Ok(End::Stopped) => break,
            Ok(End::Reopen) => info!("audio output or user changed, reopening loopback"),
            Err(e) => {
                let msg = e.to_string();
                if msg != last_error {
                    warn!("loopback capture unavailable, retrying: {msg}");
                    last_error = msg;
                }
                if enc.idle(RETRY, stop).is_err() {
                    break;
                }
            }
        }
    }
}

fn capture(
    config: &LoopbackConfig,
    session: &Session,
    server: &str,
    enc: &mut Encode,
    stop: &AtomicBool,
) -> Result<End> {
    let conn = Conn::connect(server, "web-stream")?;
    let sink = match &config.device {
        None => conn.default_sink()?,
        Some(name) => find_sink(&conn, name)?,
    };
    if sink.is_empty() || sink.starts_with('@') {
        return Err(AudioError::DeviceNotFound(
            "the session has no output device".into(),
        ));
    }
    if sink == MIC_SINK {
        warn!("the default output is the web-stream mic, so the browser will hear its own mic");
    }
    let stream = conn.stream("web-stream loopback", &format!("{sink}.monitor"), true, 10)?;
    info!(uid = session.uid, sink, "loopback capture started");
    let mut converter = Converter::new(NATIVE);
    let mut checked = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        conn.iterate(TICK)?;
        enc.retune()?;
        let mut got = false;
        let mut result = Ok(());
        stream.read(|data, len| {
            if result.is_err() {
                return;
            }
            enc.samples.clear();
            match data {
                Some(bytes) => converter.push(bytes, &mut enc.samples),
                None => converter.push_silence(len / NATIVE.frame_bytes(), &mut enc.samples),
            }
            got = true;
            result = enc.push_samples();
        })?;
        result?;
        if !got {
            enc.fill_gap()?;
        }
        if checked.elapsed() >= CHECK_EVERY {
            checked = Instant::now();
            if !same_session(session, session::active().as_ref()) {
                return Ok(End::Reopen);
            }
            if config.device.is_none() && conn.default_sink()? != sink {
                return Ok(End::Reopen);
            }
        }
    }
    Ok(End::Stopped)
}

/// Plays mic audio from a [`MicReceiver`] into a virtual microphone of the signed-in user until
/// dropped.
pub struct MicPlayback {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl MicPlayback {
    /// Returns the handle to feed with the browser's mic packets; the virtual microphone appears
    /// once someone is signed in.
    pub fn start(config: MicConfig) -> Result<(Self, MicReceiver)> {
        if !pulse::available() {
            return Err(AudioError::Platform(
                "libpulse is not installed (install PipeWire's pulse support or PulseAudio)".into(),
            ));
        }
        let (receiver, source) = mic_channel(config.jitter)?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = std::thread::Builder::new()
            .name("ws-audio-mic".into())
            .spawn(move || run_mic(config, source, &thread_stop))
            .map_err(|e| AudioError::Platform(e.to_string()))?;
        Ok((
            Self {
                stop,
                thread: Some(thread),
            },
            receiver,
        ))
    }
}

impl Drop for MicPlayback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn sleep_unless(stop: &AtomicBool, d: Duration) {
    let until = Instant::now() + d;
    while Instant::now() < until && !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn run_mic(config: MicConfig, mut source: MicSource, stop: &AtomicBool) {
    let mut last_error = String::new();
    while !stop.load(Ordering::Relaxed) {
        let Some((session, server)) =
            session::active().and_then(|s| Some((s.clone(), server(&s)?)))
        else {
            sleep_unless(stop, CHECK_EVERY);
            continue;
        };
        match play(&config, &session, &server, &mut source, stop) {
            Ok(End::Stopped) => break,
            Ok(End::Reopen) => info!("user changed, moving the virtual microphone"),
            Err(e) => {
                let msg = e.to_string();
                if msg != last_error {
                    warn!("mic playback unavailable, retrying: {msg}");
                    last_error = msg;
                }
                sleep_unless(stop, RETRY);
            }
        }
    }
}

/// Module arguments of the null sink and the source that remaps its monitor.
pub fn mic_modules() -> [(&'static str, String); 2] {
    [
        (
            "module-null-sink",
            format!(
                "sink_name={MIC_SINK} rate={SAMPLE_RATE} channels={CHANNELS} \
                 sink_properties=device.description=web-stream-mic-feed"
            ),
        ),
        (
            "module-remap-source",
            format!(
                "master={MIC_SINK}.monitor source_name={MIC_SOURCE} \
                 source_properties=\"device.description='{MIC_NAME}'\""
            ),
        ),
    ]
}

/// The modules of the virtual microphone; unloaded on drop.
struct VirtualMic<'a> {
    conn: &'a Conn,
    modules: Vec<u32>,
}

impl<'a> VirtualMic<'a> {
    fn create(conn: &'a Conn) -> Result<Self> {
        for (index, _, args) in conn.modules()? {
            if args.contains(MIC_SINK) {
                debug!(index, "unloading a virtual mic left from before");
                let _ = conn.unload_module(index);
            }
        }
        let mut mic = Self {
            conn,
            modules: Vec::new(),
        };
        for (name, args) in mic_modules() {
            mic.modules.push(conn.load_module(name, &args)?);
        }
        Ok(mic)
    }
}

impl Drop for VirtualMic<'_> {
    fn drop(&mut self) {
        for &index in self.modules.iter().rev() {
            if let Err(e) = self.conn.unload_module(index) {
                debug!(index, "virtual mic module not unloaded: {e}");
            }
        }
    }
}

fn play(
    config: &MicConfig,
    session: &Session,
    server: &str,
    source: &mut MicSource,
    stop: &AtomicBool,
) -> Result<End> {
    let conn = Conn::connect(server, "web-stream")?;
    let (_mic, sink) = if config.device.eq_ignore_ascii_case(AUTO_MIC_DEVICE) {
        (Some(VirtualMic::create(&conn)?), MIC_SINK.to_string())
    } else {
        (None, find_sink(&conn, &config.device)?)
    };
    let stream = conn.stream("web-stream microphone", &sink, false, 10)?;
    info!(uid = session.uid, sink, "mic playback started");
    let mut scratch = vec![0f32; SAMPLE_RATE as usize / 10 * CHANNELS];
    let mut checked = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        conn.iterate(TICK)?;
        let frames = (stream.writable()? / NATIVE.frame_bytes()).min(scratch.len() / CHANNELS);
        if frames > 0 {
            let pcm = &mut scratch[..frames * CHANNELS];
            source.fill(pcm);
            stream.write(pcm)?;
        }
        if checked.elapsed() >= CHECK_EVERY {
            checked = Instant::now();
            if !same_session(session, session::active().as_ref()) {
                return Ok(End::Reopen);
            }
        }
    }
    Ok(End::Stopped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mic_modules_name_the_virtual_source() {
        let [(sink, sink_args), (remap, remap_args)] = mic_modules();
        assert_eq!(sink, "module-null-sink");
        assert!(sink_args.contains("sink_name=web_stream_mic "));
        assert_eq!(remap, "module-remap-source");
        assert!(remap_args.starts_with("master=web_stream_mic.monitor "));
        assert!(remap_args.contains("'web-stream microphone'"));
    }

    #[test]
    fn session_changes_are_noticed() {
        let a = Session {
            id: "2".into(),
            uid: 1000,
            kind: "wayland".into(),
            runtime_dir: "/run/user/1000".into(),
            display: None,
        };
        assert!(same_session(&a, Some(&a.clone())));
        let b = Session {
            uid: 1001,
            ..a.clone()
        };
        assert!(!same_session(&a, Some(&b)));
        assert!(!same_session(&a, None));
    }
}
