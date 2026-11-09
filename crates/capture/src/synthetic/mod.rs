//! A stand-in for display capture, for tests on machines without a desktop
//! or GPU (CI, cloud sandboxes): fake displays whose frames are drawn in
//! memory and encoded in software with OpenH264.
//!
//! Each frame carries a stamp ([`stamp`]) with its capture time, display and
//! frame index, so a test reading the decoded picture in the browser can
//! measure latency, see dropped frames and tell which display it shows.
//! Pacing, backpressure and bitrate changes go through the same code as the
//! Windows pipeline (`crate::pacing`); what differs is only where the image
//! comes from and which encoder turns it into H.264.
//!
//! Off until [`enable`] is called (the test host's `--synthetic`); then every
//! function of this crate uses it instead of the platform's capture.

pub mod stamp;

use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use openh264::OpenH264API;
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod,
    RateControlMode, UsageType,
};
use openh264::formats::YUVSlices;
use tokio::sync::mpsc::{self, error::TrySendError};
use ws_diag::diag;
use ws_proto::{
    DisplayId, DisplayInfo, EncodedFrame, SendBacklog, VideoCodec, VideoConfig, mono_micros,
    mono_now,
};

use crate::pacing::{Backpressure, BitrateGate, EncodeJob, Scheduler};
use crate::{CaptureError, ModeChange, Result};

use stamp::Stamp;

/// The fake displays' sizes and positions, in the order they are offered.
const LAYOUT: [(&str, i32, i32, u32, u32); 4] = [
    ("Synthetic 1", 0, 0, 1920, 1080),
    ("Synthetic 2", 1920, 0, 1280, 720),
    ("Synthetic 3", -2560, 0, 2560, 1440),
    ("Synthetic 4", 0, 1080, 1024, 768),
];
/// They all "refresh" at 60 Hz: a new image every 1/60 s.
const REFRESH_MHZ: u32 = 60_000;
/// While a due frame is held back for the send queue, how often the queue
/// is looked at again (as in the Windows pipeline).
const HOLD_POLL: Duration = Duration::from_millis(2);

struct Displays {
    current: Vec<DisplayInfo>,
    /// Each display's size before [`set_display_mode`] changed it.
    saved: Vec<(u32, u32)>,
}

static DISPLAYS: OnceLock<Mutex<Displays>> = OnceLock::new();

/// Turns the synthetic source on for the rest of the process, with
/// `displays` fake displays (1 to 4; the first is primary).
pub fn enable(displays: usize) {
    let current: Vec<DisplayInfo> = LAYOUT
        .iter()
        .take(displays.clamp(1, LAYOUT.len()))
        .enumerate()
        .map(|(i, &(name, x, y, width, height))| DisplayInfo {
            id: DisplayId(i as u32),
            name: name.into(),
            x,
            y,
            width,
            height,
            refresh_mhz: REFRESH_MHZ,
            primary: i == 0,
        })
        .collect();
    let saved = current.iter().map(|d| (d.width, d.height)).collect();
    let _ = DISPLAYS.set(Mutex::new(Displays { current, saved }));
    tracing::info!(displays, "synthetic capture on");
}

/// Whether [`enable`] was called.
pub fn enabled() -> bool {
    DISPLAYS.get().is_some()
}

fn displays() -> std::sync::MutexGuard<'static, Displays> {
    DISPLAYS
        .get()
        .expect("synthetic capture not enabled")
        .lock()
        .expect("synthetic displays lock")
}

pub(crate) fn list_displays() -> Result<Vec<DisplayInfo>> {
    Ok(displays().current.clone())
}

fn find(id: DisplayId) -> Result<DisplayInfo> {
    displays()
        .current
        .iter()
        .find(|d| d.id == id)
        .cloned()
        .ok_or(CaptureError::DisplayNotFound(id))
}

/// Any even size from 640x360 up to the display's own is a "mode".
pub(crate) fn set_display_mode(id: DisplayId, width: u32, height: u32) -> Result<ModeChange> {
    let mut d = displays();
    let i = d
        .current
        .iter()
        .position(|d| d.id == id)
        .ok_or(CaptureError::DisplayNotFound(id))?;
    let (max_w, max_h) = d.saved[i];
    let width = width.clamp(640, max_w) & !1;
    let height = height.clamp(360, max_h) & !1;
    let info = &mut d.current[i];
    let changed = (info.width, info.height) != (width, height);
    info.width = width;
    info.height = height;
    Ok(ModeChange {
        width,
        height,
        changed,
    })
}

pub(crate) fn restore_display_mode(id: DisplayId) -> Result<()> {
    let mut d = displays();
    let i = d
        .current
        .iter()
        .position(|d| d.id == id)
        .ok_or(CaptureError::DisplayNotFound(id))?;
    let (w, h) = d.saved[i];
    d.current[i].width = w;
    d.current[i].height = h;
    Ok(())
}

enum Command {
    Select(DisplayId, std_mpsc::Sender<Result<()>>),
    Resize(u32, u32, std_mpsc::Sender<Result<()>>),
    Keyframe,
    Recover(u64),
    Bitrate(u32),
}

pub(crate) struct PipelineHandle {
    /// The only strong reference: dropping it tells the thread to exit.
    commands: Option<Arc<std_mpsc::Sender<Command>>>,
    thread: Option<JoinHandle<()>>,
}

impl PipelineHandle {
    pub(crate) fn start(
        display: DisplayId,
        config: VideoConfig,
        frames: mpsc::Sender<EncodedFrame>,
        backlog: SendBacklog,
    ) -> Result<Self> {
        if config.codec != VideoCodec::H264 {
            return Err(CaptureError::NoEncoder(format!(
                "the synthetic source encodes H.264 only, not {:?}",
                config.codec
            )));
        }
        find(display)?;
        let _ = mono_now();
        let (cmd_tx, cmd_rx) = std_mpsc::channel();
        let (ready_tx, ready_rx) = std_mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("ws-capture-synthetic".into())
            .spawn(move || {
                let mut session = match Session::open(display, config, frames, backlog) {
                    Ok(s) => {
                        let _ = ready_tx.send(Ok(()));
                        s
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                if let Err(e) = session.run(&cmd_rx) {
                    tracing::error!("synthetic capture stopped: {e}");
                }
            })
            .map_err(|e| CaptureError::Platform(format!("spawn capture thread: {e}")))?;
        let handle = Self {
            commands: Some(Arc::new(cmd_tx)),
            thread: Some(thread),
        };
        ready_rx.recv().map_err(|_| CaptureError::Stopped)??;
        Ok(handle)
    }

    fn send(&self, cmd: Command) {
        if let Some(tx) = &self.commands {
            let _ = tx.send(cmd);
        }
    }

    pub(crate) fn select_display(&self, display: DisplayId) -> Result<()> {
        let (tx, rx) = std_mpsc::channel();
        self.send(Command::Select(display, tx));
        rx.recv().map_err(|_| CaptureError::Stopped)?
    }

    pub(crate) fn resize(&self, width: u32, height: u32) -> Result<()> {
        let (tx, rx) = std_mpsc::channel();
        self.send(Command::Resize(width, height, tx));
        rx.recv().map_err(|_| CaptureError::Stopped)?
    }

    pub(crate) fn control(&self) -> Control {
        Control(
            self.commands
                .as_ref()
                .map(Arc::downgrade)
                .unwrap_or_default(),
        )
    }

    pub(crate) fn request_keyframe(&self) {
        self.send(Command::Keyframe);
    }

    pub(crate) fn recover_from(&self, index: u64) {
        self.send(Command::Recover(index));
    }

    pub(crate) fn set_bitrate(&self, kbps: u32) {
        self.send(Command::Bitrate(kbps));
    }
}

impl Drop for PipelineHandle {
    fn drop(&mut self) {
        self.commands.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[derive(Clone)]
pub(crate) struct Control(Weak<std_mpsc::Sender<Command>>);

impl Control {
    fn send(&self, cmd: Command) {
        if let Some(tx) = self.0.upgrade() {
            let _ = tx.send(cmd);
        }
    }

    pub(crate) fn request_keyframe(&self) {
        self.send(Command::Keyframe);
    }

    pub(crate) fn recover_from(&self, index: u64) {
        self.send(Command::Recover(index));
    }
}

/// Unix time of an [`Instant`], for the stamp.
struct WallClock {
    at: Instant,
    unix_ms: u64,
}

impl WallClock {
    fn new() -> Self {
        let unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        Self {
            at: Instant::now(),
            unix_ms,
        }
    }

    fn unix_ms(&self, t: Instant) -> u64 {
        self.unix_ms + t.saturating_duration_since(self.at).as_millis() as u64
    }
}

struct Session {
    display: DisplayId,
    config: VideoConfig,
    encoder: Encoder,
    /// The I420 picture being drawn and encoded.
    image: Vec<u8>,
    frames: mpsc::Sender<EncodedFrame>,
    sched: Scheduler,
    bitrate: BitrateGate,
    backlog: SendBacklog,
    backpressure: Backpressure,
    frame_index: u64,
    /// When the fake display next has a new image.
    next_image: Instant,
    interval: Duration,
    clock: WallClock,
    /// Frames sent and dropped since the last stats line.
    sent: u32,
    dropped: u32,
    stats_since: Instant,
}

fn i420_len(width: u32, height: u32) -> usize {
    width as usize * height as usize * 3 / 2
}

fn new_encoder(config: &VideoConfig) -> Result<Encoder> {
    let cfg = EncoderConfig::new()
        .bitrate(BitRate::from_bps(config.bitrate_kbps.saturating_mul(1000)))
        .max_frame_rate(FrameRate::from_hz(config.fps as f32))
        .rate_control_mode(RateControlMode::Bitrate)
        .usage_type(UsageType::ScreenContentRealTime)
        .complexity(Complexity::Low)
        // Every frame comes out: dropping is the pacer's job, as with NVENC.
        .skip_frames(false)
        .scene_change_detect(false)
        .background_detection(false)
        // Keyframes only when asked for, as with NVENC.
        .intra_frame_period(IntraFramePeriod::from_num_frames(0));
    Encoder::with_api_config(OpenH264API::from_source(), cfg)
        .map_err(|e| CaptureError::NoEncoder(format!("OpenH264: {e}")))
}

impl Session {
    fn open(
        display: DisplayId,
        config: VideoConfig,
        frames: mpsc::Sender<EncodedFrame>,
        backlog: SendBacklog,
    ) -> Result<Self> {
        let encoder = new_encoder(&config)?;
        let interval = Duration::from_secs(1000) / REFRESH_MHZ;
        let id = display.0;
        tracing::info!(display = id, ?config, "synthetic capture");
        let now = Instant::now();
        Ok(Self {
            display,
            image: vec![0; i420_len(config.width, config.height)],
            sched: Scheduler::new(config.fps),
            bitrate: BitrateGate::new(config.bitrate_kbps),
            config,
            encoder,
            frames,
            backlog,
            backpressure: Backpressure::default(),
            frame_index: 0,
            next_image: now,
            interval,
            clock: WallClock::new(),
            sent: 0,
            dropped: 0,
            stats_since: now,
        })
    }

    fn run(&mut self, commands: &std_mpsc::Receiver<Command>) -> Result<()> {
        loop {
            loop {
                match commands.try_recv() {
                    Ok(cmd) => self.handle(cmd),
                    Err(std_mpsc::TryRecvError::Empty) => break,
                    Err(std_mpsc::TryRecvError::Disconnected) => return Ok(()),
                }
            }
            let now = Instant::now();
            if now >= self.next_image {
                // The fake display refreshed. The picture is drawn when it
                // is encoded, stamped with this time.
                self.sched.new_image(now);
                self.next_image += self.interval;
                if self.next_image <= now {
                    self.next_image = now + self.interval;
                }
            }
            let mut holding = false;
            if self.sched.due(now) {
                holding = self.backpressure.hold(now, self.backlog.get());
                if !holding && let Some(job) = self.sched.poll(now) {
                    if !self.encode(now, job)? {
                        return Ok(());
                    }
                    continue;
                }
            }
            let now = Instant::now();
            let mut wait = self.next_image.saturating_duration_since(now);
            if holding {
                wait = wait.min(HOLD_POLL);
            } else if self.sched.due(now) {
                wait = Duration::ZERO;
            }
            match commands.recv_timeout(wait) {
                Ok(cmd) => self.handle(cmd),
                Err(std_mpsc::RecvTimeoutError::Timeout) => {}
                Err(std_mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            }
        }
    }

    fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::Keyframe => self.sched.request_keyframe(),
            // No reference invalidation in OpenH264: a keyframe repairs.
            Command::Recover(from) => {
                if from < self.frame_index {
                    self.sched.request_keyframe();
                }
            }
            Command::Bitrate(kbps) => self.bitrate.request(kbps),
            Command::Resize(width, height, reply) => {
                if (width, height) != (self.config.width, self.config.height) {
                    self.config.width = width;
                    self.config.height = height;
                    self.image = vec![0; i420_len(width, height)];
                    // OpenH264 starts again at the new size with an IDR.
                    self.sched.reset();
                }
                let _ = reply.send(Ok(()));
            }
            Command::Select(id, reply) => {
                let result = find(id).map(|_| {
                    if id != self.display {
                        tracing::info!(display = id.0, "synthetic capture");
                        self.display = id;
                        self.sched.reset();
                    }
                });
                let _ = reply.send(result);
            }
        }
    }

    /// Draws and encodes a frame and hands it off. False once the consumer
    /// has gone away.
    fn encode(&mut self, now: Instant, job: EncodeJob) -> Result<bool> {
        let kbps = if job.idr {
            self.bitrate.take_now()
        } else {
            self.bitrate.take_due(now)
        };
        if let Some(kbps) = kbps {
            self.apply_bitrate(kbps);
            self.bitrate.applied(now, kbps);
        }
        let stamp = Stamp {
            unix_ms: self.clock.unix_ms(job.captured),
            display: self.display.0,
            frame: self.frame_index,
        };
        let (w, h) = (self.config.width as usize, self.config.height as usize);
        stamp::draw(&mut self.image, w, h, &stamp);
        if job.idr {
            self.encoder.force_intra_frame();
        }
        let started = Instant::now();
        let (y, chroma) = self.image.split_at(w * h);
        let (u, v) = chroma.split_at(w * h / 4);
        let source = YUVSlices::new((y, u, v), (w, h), (w, w / 2, w / 2));
        let encoded = self
            .encoder
            .encode(&source)
            .map_err(|e| CaptureError::Platform(format!("OpenH264 encode: {e}")))?;
        let keyframe = matches!(encoded.frame_type(), FrameType::IDR | FrameType::I);
        let data = encoded.to_vec();
        let encode_ms = started.elapsed().as_secs_f64() * 1000.0;
        if data.is_empty() {
            // The encoder skipped it; nothing to send, no gap to report.
            self.sched.encoded(now, job, true);
            return Ok(true);
        }
        let bytes = data.len();
        let frame = EncodedFrame {
            display: self.display,
            codec: VideoCodec::H264,
            frame_index: self.frame_index,
            keyframe,
            recovers_from: None,
            capture_time: mono_micros(job.captured),
            encode_done_time: mono_now(),
            width: self.config.width,
            height: self.config.height,
            data: Bytes::from(data),
        };
        self.frame_index += 1;
        let delivered = match self.frames.try_send(frame) {
            Ok(()) => {
                self.sent += 1;
                true
            }
            Err(TrySendError::Full(_)) => {
                // The receiver sees the gap; the next frame is an IDR.
                self.dropped += 1;
                false
            }
            Err(TrySendError::Closed(_)) => return Ok(false),
        };
        self.sched.encoded(now, job, delivered);
        diag!(
            "frame",
            "idx={} idr={} bytes={bytes} enc_ms={encode_ms:.2} out={}",
            stamp.frame,
            keyframe,
            if delivered { "sent" } else { "full" }
        );
        if now.duration_since(self.stats_since) >= Duration::from_secs(5) {
            tracing::debug!(
                sent = self.sent,
                dropped = self.dropped,
                kbps = self.config.bitrate_kbps,
                "synthetic video stats"
            );
            self.sent = 0;
            self.dropped = 0;
            self.stats_since = now;
        }
        Ok(true)
    }

    fn apply_bitrate(&mut self, kbps: u32) {
        use openh264_sys2::{
            ENCODER_OPTION_BITRATE, ENCODER_OPTION_MAX_BITRATE, SBitrateInfo, SPATIAL_LAYER_ALL,
        };
        let bps = kbps.saturating_mul(1000).min(i32::MAX as u32) as i32;
        let mut ok = true;
        // Raise the ceiling before the target, lower it after. Only the target must take: a
        // ceiling the encoder won't move just leaves its default in place.
        let order = if kbps > self.config.bitrate_kbps {
            [ENCODER_OPTION_MAX_BITRATE, ENCODER_OPTION_BITRATE]
        } else {
            [ENCODER_OPTION_BITRATE, ENCODER_OPTION_MAX_BITRATE]
        };
        for option in order {
            let mut info = SBitrateInfo {
                iLayer: SPATIAL_LAYER_ALL,
                iBitrate: bps,
            };
            // SAFETY: both options take an SBitrateInfo, which lives for the
            // call; neither changes anything the wrapper relies on.
            let rc = unsafe {
                self.encoder
                    .raw_api()
                    .set_option(option, (&raw mut info).cast())
            };
            tracing::trace!(option, rc, "synthetic encoder bitrate option");
            if option == ENCODER_OPTION_BITRATE {
                ok &= rc == 0;
            }
        }
        if ok {
            self.config.bitrate_kbps = kbps;
        } else {
            tracing::warn!("synthetic encoder refused bitrate {kbps} kbps");
        }
        diag!("cap_cmd", "bitrate kbps={kbps} applied={ok}");
    }
}
