//! The capture thread: duplicate -> convert -> queue for encoding, newest image wins.

use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use windows::Win32::Foundation::LUID;
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
};
use ws_diag::diag;
use ws_proto::{DisplayId, EncodedFrame, SendBacklog, VideoConfig, VideoQuality, mono_now};

use super::convert::Converter;
use super::display::{self, Output};
use super::duplication::{Acquired, Duplicator};
use super::encode::Encoder;
use super::nvenc::DPB_FRAMES;
use super::{Gpu, init_process};
use crate::caps::EncoderBackend;
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
    init_process();
    let _ = mono_now();
    PipelineHandle::spawn(move |cmd_rx, ready_tx| {
        let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) };
        super::enter_mmcss();
        ws_desktop::follow_input_desktop();
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

/// GPU objects for one adapter.
struct Encode {
    encoder: Encoder<FrameMeta>,
    converter: Converter,
    gpu: Gpu,
    luid: LUID,
}

impl Encode {
    fn open(
        output: &Output,
        config: &VideoConfig,
        max: (u32, u32),
        frames: &mpsc::Sender<EncodedFrame>,
        reports: &std_mpsc::Sender<Report>,
    ) -> Result<Self> {
        let gpu = Gpu::new(&output.adapter)?;
        let converter = Converter::new(&gpu, config.width, config.height, config.fps)?;
        let backends = crate::caps::backends_on(
            crate::encoders(),
            &output.adapter_name,
            config.codec,
            config.width,
            config.height,
            if output.adapter_vendor == super::NVIDIA {
                &[EncoderBackend::Nvenc, EncoderBackend::MediaFoundation]
            } else {
                &[EncoderBackend::MediaFoundation]
            },
        );
        let mut errors = Vec::new();
        let mut transient = false;
        for backend in backends {
            let on_output = deliver(frames.clone(), reports.clone());
            match Encoder::open(backend, &gpu, config, max, on_output) {
                Ok(encoder) => {
                    return Ok(Self {
                        encoder,
                        converter,
                        gpu,
                        luid: output.adapter_luid,
                    });
                }
                Err(e) => {
                    tracing::debug!(backend = backend.name(), "encoder did not open: {e}");
                    transient |= !matches!(e, CaptureError::NoEncoder(_));
                    errors.push(format!("{}: {e}", backend.name()));
                }
            }
        }
        let message = if errors.is_empty() {
            format!(
                "no hardware encoder on {} produces {:?} at {}x{}",
                output.adapter_name, config.codec, config.width, config.height
            )
        } else {
            format!(
                "display {:?} on {}: {}",
                output.info.id,
                output.adapter_name,
                errors.join("; ")
            )
        };
        Err(if transient {
            CaptureError::Platform(message)
        } else {
            CaptureError::NoEncoder(message)
        })
    }
}

struct Session {
    dup: Option<Duplicator>,
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
    /// When Desktop Duplication last delivered an image (diagnostics).
    last_dxgi: Option<Instant>,
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
        let enc = Encode::open(&output, &config, max, &frames, &reports_tx)?;
        let dup = Duplicator::new(&output.output, &enc.gpu)?;
        tracing::info!(display = id.0, name = %output.info.name, "capturing");
        Ok(Self {
            dup: Some(dup),
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
            last_dxgi: None,
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
        let Some(dup) = self.dup.as_mut() else {
            return self.reacquire(commands);
        };
        let wait_ms = if holding {
            HOLD_POLL_MS
        } else {
            self.sched.wait(now).as_micros().div_ceil(1000) as u32
        };
        match dup.acquire(wait_ms)? {
            Acquired::Frame(texture, accumulated) => {
                let captured = Instant::now();
                let result = self.enc.converter.convert(&self.enc.gpu, &texture);
                dup.release();
                if ws_diag::enabled() {
                    let gap = self.last_dxgi.map(|t| captured - t);
                    diag!(
                        "dxgi",
                        "gap_ms={:.2} acc={accumulated} convert_ms={:.2}",
                        gap.map_or(-1.0, |g| g.as_secs_f64() * 1000.0),
                        captured.elapsed().as_secs_f64() * 1000.0
                    );
                }
                self.last_dxgi = Some(captured);
                result?;
                self.sched.new_image(captured);
            }
            Acquired::Nothing => {}
            Acquired::Lost => {
                diag!("dxgi_lost", "");
                tracing::debug!("desktop duplication lost, reopening");
                self.dup = None;
            }
        }
        Ok(true)
    }

    /// Rebuilds the device, converter and encoder after a GPU or encoder error.
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

    /// Frames from `from` on never reached the browser.
    fn recover_from(&mut self, from: u64) {
        let next = self.frame_index;
        if from >= next {
            return;
        }
        let invalidated = self.enc.encoder.can_invalidate()
            && next - from <= MAX_INVALIDATE
            && self.settle_for_invalidate()
            && (from..next)
                .try_for_each(|i| self.enc.encoder.invalidate(i))
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
        match self.enc.encoder.drain() {
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
            && self.enc.encoder.can_invalidate()
            && next - from <= MAX_INVALIDATE
            && self.settle_for_invalidate()
            && (from..next)
                .try_for_each(|i| self.enc.encoder.invalidate(i))
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
        let converter = Converter::new(&self.enc.gpu, width, height, self.config.fps)?;
        self.config.width = width;
        self.config.height = height;
        let in_place = self.enc.encoder.resize(width, height);
        diag!(
            "capture_resize",
            "size={width}x{height} in_place={} ms={:.2}",
            in_place.is_ok(),
            started.elapsed().as_secs_f64() * 1000.0
        );
        match in_place {
            Ok(()) => {
                self.enc.converter = converter;
                if let Err(e) = self.switch_to(self.display, false) {
                    diag!("capture_resize", "dup_failed={e}");
                    tracing::warn!("reopening desktop duplication after resize: {e}");
                    self.dup = None;
                }
                Ok(())
            }
            Err(e) => {
                tracing::debug!("resizing in place failed ({e}); opening a new encoder");
                self.max = (self.max.0.max(width), self.max.1.max(height));
                self.switch_to(self.display, true)
            }
        }
    }

    /// Points capture at `id`.
    fn switch_to(&mut self, id: DisplayId, rebuild: bool) -> Result<()> {
        let output = display::find(id)?;
        if rebuild || !same_luid(output.adapter_luid, self.enc.luid) {
            let enc = Encode::open(
                &output,
                &self.config,
                self.max,
                &self.frames,
                &self.reports_tx,
            )?;
            let dup = Duplicator::new(&output.output, &enc.gpu)?;
            self.dup = None;
            self.enc = enc;
            self.dup = Some(dup);
        } else {
            self.dup = None;
            self.dup = Some(Duplicator::new(&output.output, &self.enc.gpu)?);
        }
        tracing::info!(display = id.0, name = %output.info.name, "capturing");
        diag!("capture_switch", "display={} rebuild={rebuild}", id.0);
        self.display = id;
        self.sched.reset();
        Ok(())
    }

    /// Reopens duplication after it was lost, retrying while the desktop is unavailable.
    fn reacquire(&mut self, commands: &std_mpsc::Receiver<Command>) -> Result<bool> {
        let started = Instant::now();
        let mut logged = false;
        loop {
            ws_desktop::follow_input_desktop();
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

    /// Queues the converter's current image for encoding; it is handed off once it comes out
    /// ([`deliver`]).
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
        self.enc.encoder.submit(
            self.enc.converter.output(),
            self.frame_index,
            job.idr,
            intra,
            kbps,
            meta,
        )?;
        if let Some(kbps) = kbps {
            let actual = self.enc.encoder.bitrate_kbps();
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

fn same_luid(a: LUID, b: LUID) -> bool {
    a.LowPart == b.LowPart && a.HighPart == b.HighPart
}
