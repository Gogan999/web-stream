//! The capture thread: KMS scanout -> GPU conversion -> queue for encoding, newest image wins.

use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use ws_diag::diag;
use ws_proto::{DisplayId, EncodedFrame, SendBacklog, VideoConfig, VideoQuality, mono_now};

use super::display::{self, Output};
use super::kms::{Acquired, Frame, KmsSource};
use super::nvenc::GlNvenc;
use super::va::encoder::VaEncoder;
use crate::caps::{EncoderBackend, backends_for};
use crate::nvenc::DPB_FRAMES;
use crate::pacing::{Backpressure, BitrateGate, EncodeJob, Scheduler};
use crate::session::{
    Command, FrameMeta, HOLD_POLL_MS, MAX_INVALIDATE, PipelineHandle, Report, deliver,
};
use crate::{CaptureError, Result};

/// Starts capturing `display` on a new capture thread.
pub(crate) fn start(
    display: DisplayId,
    config: VideoConfig,
    max: (u32, u32),
    frames: mpsc::Sender<EncodedFrame>,
    backlog: SendBacklog,
) -> Result<PipelineHandle> {
    let _ = mono_now();
    PipelineHandle::spawn(move |cmd_rx, ready_tx| {
        raise_priority();
        let mut session = match Session::open(display, config, max, frames, backlog) {
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
}

/// Gives the calling thread a higher scheduling priority, where allowed.
pub(crate) fn raise_priority() {
    // SAFETY: plain syscalls on the calling thread.
    let r = unsafe {
        let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
        libc::setpriority(libc::PRIO_PROCESS, tid, -10)
    };
    if r != 0 {
        tracing::debug!(
            "could not raise the capture thread's priority: {}",
            std::io::Error::last_os_error()
        );
    }
}

/// The encoder backend, with the converter that feeds it.
enum Backend {
    Va(Box<VaEncoder<FrameMeta>>),
    Nvenc(Box<GlNvenc<FrameMeta>>),
}

impl Backend {
    fn convert(&mut self, frame: &Frame) -> Result<()> {
        match self {
            Self::Va(e) => e.convert(frame),
            Self::Nvenc(e) => e.convert(frame),
        }
    }

    fn submit(
        &mut self,
        index: u64,
        idr: bool,
        intra: bool,
        kbps: Option<u32>,
        meta: FrameMeta,
    ) -> Result<()> {
        match self {
            Self::Va(e) => e.submit(idr || intra, kbps, meta),
            Self::Nvenc(e) => e.submit(index, idr, intra, kbps, meta),
        }
    }

    fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        match self {
            Self::Va(_) => Err(CaptureError::InvalidConfig(
                "VA-API encoders are reopened to resize".into(),
            )),
            Self::Nvenc(e) => e.resize(width, height),
        }
    }

    fn can_invalidate(&self) -> bool {
        match self {
            Self::Va(_) => false,
            Self::Nvenc(e) => e.can_invalidate(),
        }
    }

    fn invalidate(&mut self, index: u64) -> Result<()> {
        match self {
            Self::Va(_) => Err(CaptureError::Platform(
                "reference invalidation not available".into(),
            )),
            Self::Nvenc(e) => e.invalidate(index),
        }
    }

    fn drain(&mut self) -> Result<()> {
        match self {
            Self::Va(e) => e.drain(),
            Self::Nvenc(e) => e.drain(),
        }
    }

    fn bitrate_kbps(&self) -> u32 {
        match self {
            Self::Va(e) => e.bitrate_kbps(),
            Self::Nvenc(e) => e.bitrate_kbps(),
        }
    }
}

/// The encoder for one GPU.
struct Encode {
    backend: Backend,
    card: PathBuf,
}

impl Encode {
    /// Opens the first backend that works for `output`'s GPU, in [`backends_for`] order.
    fn open(
        output: &Output,
        config: &VideoConfig,
        max: (u32, u32),
        frames: &mpsc::Sender<EncodedFrame>,
        reports: &std_mpsc::Sender<Report>,
    ) -> Result<Self> {
        let encoders = super::device_encoders(output);
        let order = backends_for(&encoders, config.codec, config.width, config.height);
        if order.is_empty() {
            return Err(CaptureError::NoEncoder(format!(
                "no hardware {:?} encoder at {}x{} on {} ({})",
                config.codec,
                config.width,
                config.height,
                output.card.display(),
                if output.driver.is_empty() {
                    "unknown driver"
                } else {
                    &output.driver
                }
            )));
        }
        let mut errors = Vec::new();
        for backend in order {
            let on_output = deliver(frames.clone(), reports.clone());
            let opened = match backend {
                EncoderBackend::Vaapi => {
                    let node = output.render.clone().unwrap_or_else(|| output.card.clone());
                    VaEncoder::new(&node, config, on_output).map(|e| Backend::Va(Box::new(e)))
                }
                EncoderBackend::Nvenc => GlNvenc::new(output, config, max, on_output)
                    .map(|e| Backend::Nvenc(Box::new(e))),
                EncoderBackend::MediaFoundation => continue,
            };
            match opened {
                Ok(backend) => {
                    return Ok(Self {
                        backend,
                        card: output.card.clone(),
                    });
                }
                Err(e) => {
                    tracing::warn!("{} encoder failed to open: {e}", backend.name());
                    errors.push(format!("{}: {e}", backend.name()));
                }
            }
        }
        Err(CaptureError::NoEncoder(errors.join("; ")))
    }
}

struct Session {
    source: Option<KmsSource>,
    /// From the encoder's output thread.
    reports: std_mpsc::Receiver<Report>,
    reports_tx: std_mpsc::Sender<Report>,
    enc: Encode,
    display: DisplayId,
    config: VideoConfig,
    /// The largest size the encoder can be resized to in place.
    max: (u32, u32),
    frames: mpsc::Sender<EncodedFrame>,
    sched: Scheduler,
    /// Bitrate changes waiting to reach the encoder.
    bitrate: BitrateGate,
    backlog: SendBacklog,
    backpressure: Backpressure,
    frame_index: u64,
    /// The next frame must not reference frames from this index on.
    recovers_from: Option<u64>,
    /// Every reference the encoder kept was invalidated.
    intra_next: bool,
    /// Recent GPU/encoder rebuilds, to give up on a persistently broken device instead of looping.
    recoveries: Vec<Instant>,
    /// When the scanout last changed (diagnostics).
    last_scanout: Option<Instant>,
}

impl Session {
    fn open(
        id: DisplayId,
        config: VideoConfig,
        max: (u32, u32),
        frames: mpsc::Sender<EncodedFrame>,
        backlog: SendBacklog,
    ) -> Result<Self> {
        let output = display::find(id)?;
        let max = (max.0.max(config.width), max.1.max(config.height));
        let (reports_tx, reports) = std_mpsc::channel();
        let source = KmsSource::open(&output)?;
        let enc = Encode::open(&output, &config, max, &frames, &reports_tx)?;
        tracing::info!(display = id.0, name = %output.info.name, "capturing");
        Ok(Self {
            source: Some(source),
            reports,
            reports_tx,
            enc,
            display: id,
            sched: {
                let sched = Scheduler::new(config.fps);
                if config.quality == VideoQuality::Source {
                    sched.pace_by_bitrate(config.bitrate_kbps)
                } else {
                    sched
                }
            },
            bitrate: BitrateGate::new(config.bitrate_kbps),
            config,
            max,
            frames,
            backlog,
            backpressure: Backpressure::default(),
            frame_index: 0,
            recovers_from: None,
            intra_next: false,
            recoveries: Vec::new(),
            last_scanout: None,
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
            while let Ok(report) = self.reports.try_recv() {
                match report {
                    Report::Lost { from, keyframe } => self.frame_lost(from, keyframe),
                    Report::Done { bytes } => self.sched.frame_done(Instant::now(), bytes),
                    Report::Closed => return Ok(()),
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

    /// One iteration: encode if due.
    fn step(&mut self, commands: &std_mpsc::Receiver<Command>) -> Result<bool> {
        let now = Instant::now();
        let mut holding = false;
        if self.sched.due(now) {
            let backlog = self.backlog.get();
            let was = self.backpressure.holding_since();
            holding = self.backpressure.hold(now, backlog);
            if holding && was.is_none() {
                diag!(
                    "hold",
                    "start backlog_ms={:.2}",
                    backlog.as_secs_f64() * 1000.0
                );
            }
            if !holding {
                if let Some(since) = was {
                    let how = if self.backpressure.holding_since().is_none() {
                        "caught_up"
                    } else {
                        "max_hold"
                    };
                    diag!(
                        "hold",
                        "end {how} after_ms={:.2} backlog_ms={:.2}",
                        (now - since).as_secs_f64() * 1000.0,
                        backlog.as_secs_f64() * 1000.0
                    );
                }
                if let Some(job) = self.sched.poll(now) {
                    return self.encode(now, job);
                }
            }
        }
        let Some(source) = self.source.as_mut() else {
            return self.reacquire(commands);
        };
        let wait = if holding {
            Duration::from_millis(u64::from(HOLD_POLL_MS))
        } else {
            self.sched.wait(now)
        };
        match source.acquire(wait)? {
            Acquired::Frame(frame) => {
                let captured = Instant::now();
                let result = self.enc.backend.convert(&frame);
                if ws_diag::enabled() {
                    let gap = self.last_scanout.map(|t| captured - t);
                    diag!(
                        "kms",
                        "gap_ms={:.2} convert_ms={:.2}",
                        gap.map_or(-1.0, |g| g.as_secs_f64() * 1000.0),
                        captured.elapsed().as_secs_f64() * 1000.0
                    );
                }
                self.last_scanout = Some(captured);
                result?;
                self.sched.new_image(captured);
            }
            Acquired::Nothing => {}
            Acquired::Lost => {
                diag!("kms_lost", "");
                tracing::debug!("scanout lost, reopening");
                self.source = None;
            }
        }
        Ok(true)
    }

    /// Rebuilds the encoder after a GPU or encoder error.
    fn recover(&mut self, error: CaptureError) -> Result<()> {
        let now = Instant::now();
        self.recoveries
            .retain(|t| now.duration_since(*t) < Duration::from_secs(30));
        if self.recoveries.len() >= 3 {
            return Err(error);
        }
        self.recoveries.push(now);
        diag!("capture_error", "rebuilding error={error}");
        tracing::warn!("capture error, rebuilding the encoder: {error}");
        std::thread::sleep(Duration::from_millis(250));
        self.switch_to(self.display, true)
    }

    fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::Keyframe => {
                diag!("cap_cmd", "keyframe");
                self.sched.request_keyframe()
            }
            Command::Recover(from) => self.recover_from(from),
            Command::Bitrate(kbps) => self.bitrate.request(kbps),
            Command::Resize(width, height, reply) => {
                let result = self.resize(width, height);
                if let Err(e) = &result {
                    tracing::warn!(width, height, "resize failed: {e}");
                }
                let _ = reply.send(result);
            }
            Command::Select(id, reply) => {
                let result = if id == self.display && self.source.is_some() {
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

    /// Frames from `from` on never reached the browser.
    fn recover_from(&mut self, from: u64) {
        let next = self.frame_index;
        if from >= next {
            return;
        }
        let invalidated = self.enc.backend.can_invalidate()
            && next - from <= MAX_INVALIDATE
            && self.settle_for_invalidate()
            && (from..next)
                .try_for_each(|i| self.enc.backend.invalidate(i))
                .is_ok();
        diag!(
            "cap_cmd",
            "recover from={from} next={next} invalidated={invalidated}"
        );
        if invalidated {
            self.recovers_from = Some(self.recovers_from.map_or(from, |r| r.min(from)));
            self.intra_next |= next - from >= u64::from(DPB_FRAMES);
            self.sched.request_repair();
        } else {
            self.sched.request_keyframe();
        }
    }

    /// Lets every frame still in the encoder finish.
    fn settle_for_invalidate(&mut self) -> bool {
        match self.enc.backend.drain() {
            Ok(()) => true,
            Err(e) => {
                tracing::debug!("encoder drain before invalidating: {e}");
                false
            }
        }
    }

    /// A frame never reached the consumer, and nor will the frames after it still in the encoder.
    fn frame_lost(&mut self, from: u64, keyframe: bool) {
        let next = self.frame_index;
        let invalidated = !keyframe
            && from < next
            && self.enc.backend.can_invalidate()
            && next - from <= MAX_INVALIDATE
            && self.settle_for_invalidate()
            && (from..next)
                .try_for_each(|i| self.enc.backend.invalidate(i))
                .is_ok();
        diag!(
            "cap_cmd",
            "lost from={from} next={next} keyframe={keyframe} invalidated={invalidated}"
        );
        if invalidated {
            self.recovers_from = Some(self.recovers_from.map_or(from, |r| r.min(from)));
            self.intra_next |= next - from >= u64::from(DPB_FRAMES);
        } else {
            self.sched.dropped();
        }
    }

    /// Changes the stream size.
    fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        if (width, height) == (self.config.width, self.config.height) {
            return Ok(());
        }
        let started = Instant::now();
        let (old_w, old_h) = (self.config.width, self.config.height);
        self.config.width = width;
        self.config.height = height;
        let in_place = self.enc.backend.resize(width, height);
        diag!(
            "capture_resize",
            "size={width}x{height} in_place={} ms={:.2}",
            in_place.is_ok(),
            started.elapsed().as_secs_f64() * 1000.0
        );
        let result = match in_place {
            Ok(()) => {
                if let Some(source) = self.source.as_mut() {
                    source.refresh();
                }
                self.sched.reset();
                Ok(())
            }
            Err(e) => {
                tracing::debug!("resizing in place failed ({e}); opening a new encoder");
                self.max = (self.max.0.max(width), self.max.1.max(height));
                self.switch_to(self.display, true)
            }
        };
        if result.is_err() {
            self.config.width = old_w;
            self.config.height = old_h;
        }
        result
    }

    /// Points capture at `id`.
    fn switch_to(&mut self, id: DisplayId, rebuild: bool) -> Result<()> {
        let output = display::find(id)?;
        let source = KmsSource::open(&output)?;
        if rebuild || output.card != self.enc.card {
            let enc = Encode::open(
                &output,
                &self.config,
                self.max,
                &self.frames,
                &self.reports_tx,
            )?;
            self.enc = enc;
        }
        self.source = Some(source);
        tracing::info!(display = id.0, name = %output.info.name, "capturing");
        diag!("capture_switch", "display={} rebuild={rebuild}", id.0);
        self.display = id;
        self.sched.reset();
        Ok(())
    }

    /// Reopens the scanout after it was lost, retrying while the display is unavailable.
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
                        tracing::warn!("display unavailable, still retrying: {e}");
                        logged = true;
                    }
                }
            }
            match commands.recv_timeout(Duration::from_millis(100)) {
                Ok(cmd) => self.handle(cmd),
                Err(std_mpsc::RecvTimeoutError::Timeout) => {}
                Err(std_mpsc::RecvTimeoutError::Disconnected) => return Ok(false),
            }
            if self.source.is_some() {
                return Ok(true);
            }
        }
    }

    /// Queues the current image for encoding; it is handed off once it comes out ([`deliver`]).
    fn encode(&mut self, now: Instant, job: EncodeJob) -> Result<bool> {
        let start = Instant::now();
        let intra = std::mem::take(&mut self.intra_next);
        let kbps = if job.idr || intra {
            self.bitrate.take_now()
        } else {
            self.bitrate.take_due(now)
        };
        let meta = FrameMeta {
            index: self.frame_index,
            display: self.display,
            codec: self.config.codec,
            width: self.config.width,
            height: self.config.height,
            captured: job.captured,
            recovers_from: self.recovers_from.take().filter(|_| !job.idr),
            idr: job.idr,
            intra,
            repair: self.sched.repair_requested(),
            why: self.sched.idr_cause(),
            repeat: job.captured == now,
            started: start,
            held: start.saturating_duration_since(job.captured),
            kbps: kbps.unwrap_or(self.config.bitrate_kbps),
        };
        self.enc
            .backend
            .submit(self.frame_index, job.idr, intra, kbps, meta)?;
        if let Some(kbps) = kbps {
            let actual = self.enc.backend.bitrate_kbps();
            if actual != kbps {
                tracing::warn!("bitrate change to {kbps} kbps failed");
            }
            diag!("cap_cmd", "bitrate kbps={kbps} applied={actual}");
            self.config.bitrate_kbps = actual;
            self.sched.set_bitrate(actual);
            self.bitrate.applied(now, actual);
        }
        self.frame_index += 1;
        // A frame lost on the way out comes back as a report.
        self.sched.encoded(now, job, true);
        Ok(true)
    }
}
