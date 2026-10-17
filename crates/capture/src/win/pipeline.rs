//! The capture thread: duplicate -> convert -> encode -> hand off, one frame
//! at a time, newest image wins.

use std::sync::mpsc as std_mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::{self, error::TrySendError};
use windows::Win32::Foundation::LUID;
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
};
use ws_proto::{DisplayId, EncodedFrame, VideoConfig, mono_micros, mono_now};

use super::convert::Converter;
use super::display::{self, Output};
use super::duplication::{Acquired, Duplicator};
use super::nvenc::NvEncoder;
use super::{Gpu, init_process};
use crate::pacing::{EncodeJob, Scheduler};
use crate::{CaptureError, Result};

enum Command {
    Select(DisplayId, std_mpsc::Sender<Result<()>>),
    Keyframe,
    Bitrate(u32),
}

pub(crate) struct PipelineHandle {
    commands: Option<std_mpsc::Sender<Command>>,
    thread: Option<JoinHandle<()>>,
}

impl PipelineHandle {
    pub(crate) fn start(
        display: DisplayId,
        config: VideoConfig,
        frames: mpsc::Sender<EncodedFrame>,
    ) -> Result<Self> {
        init_process();
        // Fix the shared clock's epoch before any frame timestamps exist.
        let _ = mono_now();
        let (cmd_tx, cmd_rx) = std_mpsc::channel();
        let (ready_tx, ready_rx) = std_mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("ws-capture".into())
            .spawn(move || {
                let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) };
                let mut session = match Session::open(display, config, frames) {
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
                    tracing::error!("capture pipeline stopped: {e}");
                }
            })
            .map_err(|e| CaptureError::Platform(format!("spawn capture thread: {e}")))?;
        let handle = Self {
            commands: Some(cmd_tx),
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

    pub(crate) fn request_keyframe(&self) {
        self.send(Command::Keyframe);
    }

    pub(crate) fn set_bitrate(&self, kbps: u32) {
        self.send(Command::Bitrate(kbps));
    }
}

impl Drop for PipelineHandle {
    fn drop(&mut self) {
        // Closing the command channel tells the thread to exit.
        self.commands.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// GPU objects for one adapter. Field order is drop order: the encoder
/// unregisters the converter's texture, so it goes first.
struct Encode {
    encoder: NvEncoder,
    converter: Converter,
    gpu: Gpu,
    luid: LUID,
}

impl Encode {
    fn open(output: &Output, config: &VideoConfig) -> Result<Self> {
        let gpu = Gpu::new(&output.adapter)?;
        let converter = Converter::new(&gpu, config.width, config.height, config.fps)?;
        let encoder = NvEncoder::new(&gpu, converter.output(), config).map_err(|e| match e {
            CaptureError::NoEncoder(msg) if output.adapter_vendor != 0x10de => CaptureError::NoEncoder(format!(
                "{msg}; display {:?} is driven by a non-NVIDIA adapter (AMF/QSV not supported yet)",
                output.info.id
            )),
            e => e,
        })?;
        Ok(Self {
            encoder,
            converter,
            gpu,
            luid: output.adapter_luid,
        })
    }
}

#[derive(Default)]
struct Stats {
    since: Option<Instant>,
    encoded: u32,
    dropped: u32,
    encode_time: Duration,
}

struct Session {
    // Drop order: duplication before the device it was created on.
    dup: Option<Duplicator>,
    enc: Encode,
    display: DisplayId,
    config: VideoConfig,
    frames: mpsc::Sender<EncodedFrame>,
    sched: Scheduler,
    frame_index: u64,
    /// Recent GPU/encoder rebuilds, to give up on a persistently broken
    /// device instead of looping.
    recoveries: Vec<Instant>,
    stats: Stats,
}

impl Session {
    fn open(
        id: DisplayId,
        config: VideoConfig,
        frames: mpsc::Sender<EncodedFrame>,
    ) -> Result<Self> {
        let output = display::find(id)?;
        let enc = Encode::open(&output, &config)?;
        let dup = Duplicator::new(&output.output, &enc.gpu)?;
        tracing::info!(display = id.0, name = %output.info.name, "capturing");
        Ok(Self {
            dup: Some(dup),
            enc,
            display: id,
            sched: Scheduler::new(config.fps),
            config,
            frames,
            frame_index: 0,
            recoveries: Vec::new(),
            stats: Stats::default(),
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
            match self.step(commands) {
                Ok(true) => {}
                Ok(false) => return Ok(()),
                Err(e @ CaptureError::NoEncoder(_)) => return Err(e),
                Err(e) => self.recover(e)?,
            }
        }
    }

    /// One iteration: encode if due, otherwise wait for a desktop update.
    /// Returns false once the consumer or the handle has gone away.
    fn step(&mut self, commands: &std_mpsc::Receiver<Command>) -> Result<bool> {
        let now = Instant::now();
        if let Some(job) = self.sched.poll(now) {
            return self.encode(now, job);
        }
        let Some(dup) = self.dup.as_mut() else {
            return self.reacquire(commands);
        };
        // Round up so a sub-millisecond wait doesn't become a busy poll.
        let wait_ms = self.sched.wait(now).as_micros().div_ceil(1000) as u32;
        match dup.acquire(wait_ms)? {
            Acquired::Frame(texture) => {
                let captured = Instant::now();
                let result = self.enc.converter.convert(&self.enc.gpu, &texture);
                dup.release();
                result?;
                self.sched.new_image(captured);
            }
            Acquired::Nothing => {}
            Acquired::Lost => {
                tracing::debug!("desktop duplication lost, reopening");
                self.dup = None;
            }
        }
        Ok(true)
    }

    /// Rebuilds the device, converter and encoder after a GPU or encoder
    /// error (driver reset, device removed). Gives up after repeated
    /// failures in a short window.
    fn recover(&mut self, error: CaptureError) -> Result<()> {
        let now = Instant::now();
        self.recoveries
            .retain(|t| now.duration_since(*t) < Duration::from_secs(30));
        if self.recoveries.len() >= 3 {
            return Err(error);
        }
        self.recoveries.push(now);
        tracing::warn!("capture error, rebuilding the encoder: {error}");
        // Let a driver reset settle before touching the GPU again.
        std::thread::sleep(Duration::from_millis(250));
        self.switch_to(self.display, true)
    }

    fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::Keyframe => self.sched.request_keyframe(),
            Command::Bitrate(kbps) => {
                if let Err(e) = self.enc.encoder.set_bitrate(kbps) {
                    tracing::warn!("bitrate change to {kbps} kbps failed: {e}");
                } else {
                    self.config.bitrate_kbps = kbps;
                }
            }
            Command::Select(id, reply) => {
                let result = if id == self.display && self.dup.is_some() {
                    Ok(())
                } else {
                    self.switch_to(id, false)
                };
                if let Err(e) = &result {
                    tracing::warn!(display = id.0, "display switch failed: {e}");
                }
                let _ = reply.send(result);
            }
        }
    }

    /// Points capture at `id`. The encoder is rebuilt when the display is on
    /// another adapter or `rebuild` is set; otherwise only the duplication is
    /// replaced (the old one is released first, since DXGI may refuse a
    /// second duplication of the same output).
    fn switch_to(&mut self, id: DisplayId, rebuild: bool) -> Result<()> {
        let output = display::find(id)?;
        if rebuild || !same_luid(output.adapter_luid, self.enc.luid) {
            let enc = Encode::open(&output, &self.config)?;
            let dup = Duplicator::new(&output.output, &enc.gpu)?;
            self.dup = None;
            self.enc = enc;
            self.dup = Some(dup);
        } else {
            self.dup = None;
            self.dup = Some(Duplicator::new(&output.output, &self.enc.gpu)?);
        }
        tracing::info!(display = id.0, name = %output.info.name, "capturing");
        self.display = id;
        self.sched.reset();
        Ok(())
    }

    /// Reopens duplication after it was lost, retrying while the desktop is
    /// unavailable (mode change, UAC prompt). If the display disappeared,
    /// falls back to the primary display. Returns false on shutdown.
    fn reacquire(&mut self, commands: &std_mpsc::Receiver<Command>) -> Result<bool> {
        let started = Instant::now();
        let mut logged = false;
        loop {
            match self.switch_to(self.display, false) {
                Ok(()) => return Ok(true),
                Err(CaptureError::DisplayNotFound(_))
                    if started.elapsed() > Duration::from_secs(3) =>
                {
                    let primary = display::enumerate()?
                        .into_iter()
                        .find(|o| o.info.primary)
                        .ok_or(CaptureError::DisplayNotFound(self.display))?;
                    tracing::warn!(
                        from = self.display.0,
                        to = primary.info.id.0,
                        "display gone, switching"
                    );
                    self.display = primary.info.id;
                }
                Err(e @ CaptureError::NoEncoder(_)) => return Err(e),
                Err(e) => {
                    if !logged && started.elapsed() > Duration::from_secs(2) {
                        tracing::warn!("desktop unavailable, still retrying: {e}");
                        logged = true;
                    }
                }
            }
            // Keep honoring commands (and shutdown) while we wait.
            match commands.recv_timeout(Duration::from_millis(100)) {
                Ok(cmd) => self.handle(cmd),
                Err(std_mpsc::RecvTimeoutError::Timeout) => {}
                Err(std_mpsc::RecvTimeoutError::Disconnected) => return Ok(false),
            }
            if self.dup.is_some() {
                return Ok(true);
            }
        }
    }

    /// Encodes the converter's current image and hands it off. Returns false
    /// once the consumer has gone away.
    fn encode(&mut self, now: Instant, job: EncodeJob) -> Result<bool> {
        let start = Instant::now();
        let bits = self
            .enc
            .encoder
            .encode(mono_micros(job.captured), job.idr)?;
        let done = Instant::now();

        let frame = EncodedFrame {
            display: self.display,
            codec: self.config.codec,
            frame_index: self.frame_index,
            keyframe: bits.keyframe,
            capture_time: mono_micros(job.captured),
            encode_done_time: mono_now(),
            width: self.config.width,
            height: self.config.height,
            data: bits.data,
        };
        self.frame_index += 1;

        let stats = &mut self.stats;
        stats.encode_time += done - start;
        let delivered = match self.frames.try_send(frame) {
            Ok(()) => {
                stats.encoded += 1;
                true
            }
            Err(TrySendError::Full(_)) => {
                // The receiver sees a gap in frame_index; the next paced
                // frame is an IDR so it can resume.
                stats.dropped += 1;
                false
            }
            Err(TrySendError::Closed(_)) => return Ok(false),
        };
        self.sched.encoded(now, job, delivered);

        let since = *stats.since.get_or_insert(done);
        if done - since >= Duration::from_secs(5) {
            let n = (stats.encoded + stats.dropped).max(1);
            tracing::debug!(
                fps = f64::from(stats.encoded) / (done - since).as_secs_f64(),
                dropped = stats.dropped,
                avg_encode_ms = stats.encode_time.as_secs_f64() * 1000.0 / f64::from(n),
                "video stats"
            );
            *stats = Stats {
                since: Some(done),
                ..Default::default()
            };
        }
        Ok(true)
    }
}

fn same_luid(a: LUID, b: LUID) -> bool {
    a.LowPart == b.LowPart && a.HighPart == b.HighPart
}
