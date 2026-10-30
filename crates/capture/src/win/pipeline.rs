//! The capture thread: duplicate -> convert -> queue for encoding, newest
//! image wins. Frames come out of the encoder on its own thread, which
//! hands them off ([`deliver`]); meanwhile the capture thread is already
//! taking the next image.

use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::{self, error::TrySendError};
use windows::Win32::Foundation::LUID;
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
};
use ws_diag::diag;
use ws_proto::{
    DisplayId, EncodedFrame, SendBacklog, VideoCodec, VideoConfig, mono_micros, mono_now,
};

use super::convert::Converter;
use super::display::{self, Output};
use super::duplication::{Acquired, Duplicator};
use super::nvenc::{DPB_FRAMES, Finished, NvEncoder};
use super::{Gpu, init_process};
use crate::pacing::{Backpressure, BitrateGate, EncodeJob, Scheduler};
use crate::{CaptureError, Result};

enum Command {
    Select(DisplayId, std_mpsc::Sender<Result<()>>),
    Resize(u32, u32, std_mpsc::Sender<Result<()>>),
    Keyframe,
    Recover(u64),
    Bitrate(u32),
}

/// Frames further back than this are not invalidated one by one; a
/// keyframe is sent instead. Comfortably more than the encoder keeps as
/// references, so nothing in the gap can still be referenced.
const MAX_INVALIDATE: u64 = 16;
/// While a due frame is held back for the send queue, how often the queue
/// is looked at again (new desktop images are still taken meanwhile).
const HOLD_POLL_MS: u32 = 2;

pub(crate) struct PipelineHandle {
    /// The only strong reference: dropping it closes the channel, which
    /// tells the thread to exit, however many [`Control`]s are around.
    commands: Option<Arc<std_mpsc::Sender<Command>>>,
    thread: Option<JoinHandle<()>>,
}

impl PipelineHandle {
    pub(crate) fn start(
        display: DisplayId,
        config: VideoConfig,
        max: (u32, u32),
        frames: mpsc::Sender<EncodedFrame>,
        backlog: SendBacklog,
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
                // The session may start on the lock screen.
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

/// Keyframe and repair requests for the capture thread, from any thread.
/// Holding one doesn't keep the pipeline running.
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

impl Drop for PipelineHandle {
    fn drop(&mut self) {
        // Closing the command channel tells the thread to exit.
        self.commands.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// What the capture thread tags a frame with when it goes into the
/// encoder, for [`deliver`] when it comes out.
struct FrameMeta {
    index: u64,
    display: DisplayId,
    codec: VideoCodec,
    width: u32,
    height: u32,
    captured: Instant,
    /// The frame skips frames from this index on (see `recovers_from` in
    /// [`EncodedFrame`]).
    recovers_from: Option<u64>,
    // For diagnostics.
    idr: bool,
    intra: bool,
    repair: bool,
    why: Option<&'static str>,
    /// Re-encodes the last image because the desktop was still.
    repeat: bool,
    /// Captured -> the start of `encode`. It ends before the encoder's own
    /// timing starts (slot wait, copy and encode call), which
    /// `EncodeTiming` covers, with `pre_ms` bridging from `started`.
    held: Duration,
    /// Where `held` ends.
    started: Instant,
    kbps: u32,
}

/// From the encoder's output thread back to the capture thread.
enum Report {
    /// The frame `index` was not handed off (the consumer's channel was
    /// full), nor anything after it still in the encoder that built on
    /// it; frames from `from` on are missing.
    Lost { from: u64, keyframe: bool },
    /// The consumer has gone away.
    Closed,
}

/// GPU objects for one adapter. Field order is drop order: the encoder
/// (whose output thread finishes first) before the device.
struct Encode {
    encoder: NvEncoder<FrameMeta>,
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
        let on_output = deliver(frames.clone(), reports.clone());
        let encoder = NvEncoder::new(&gpu, config, max, on_output).map_err(|e| match e {
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
    /// The next frame must not reference frames from this index on, which
    /// were invalidated: it is tagged so the receiver can let it follow
    /// the gap.
    recovers_from: Option<u64>,
    /// Every reference the encoder kept was invalidated, so the next frame
    /// is coded from scratch: give it a keyframe's budget.
    intra_next: bool,
    /// Recent GPU/encoder rebuilds, to give up on a persistently broken
    /// device instead of looping.
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
            sched: Scheduler::new(config.fps),
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

    /// One iteration: encode if due, otherwise wait for a desktop update.
    /// Returns false once the consumer or the handle has gone away.
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
        // Round up so a sub-millisecond wait doesn't become a busy poll.
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
        diag!("capture_error", "rebuilding error={error}");
        tracing::warn!("capture error, rebuilding the encoder: {error}");
        // Let a driver reset settle before touching the GPU again.
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
            // Applied with the next frame, at most so often (see BitrateGate).
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

    /// Frames from `from` on never reached the browser. Invalidates every
    /// one encoded so far, so the next frame builds only on frames the
    /// browser has; falls back to a keyframe if the encoder can't, or the
    /// gap is too long.
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

    /// Lets every frame still in the encoder finish, so each one about to
    /// be invalidated has its reconstructed picture: a driver may ignore an
    /// invalidation for a frame it hasn't finished, and the next frame
    /// would then still predict from it. Only on loss, so the wait is rare.
    /// False if the encoder is stuck (a keyframe follows instead).
    fn settle_for_invalidate(&mut self) -> bool {
        match self.enc.encoder.drain() {
            Ok(()) => true,
            Err(e) => {
                tracing::debug!("encoder drain before invalidating: {e}");
                false
            }
        }
    }

    /// A frame never reached the consumer, and nor will the frames after it
    /// still in the encoder (they build on it). Invalidates all of them so
    /// the next paced frame skips the gap, or makes it an IDR if the
    /// encoder can't (or the lost frame was a keyframe). Not at once: a
    /// stalled consumer must not make the thread spin.
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

    /// Changes the stream size. In place where the encoder allows it: a new
    /// converter output on the same device and the encoder reset to the new
    /// size, so there is no stall for a new device and session. Otherwise
    /// (larger than the encoder was opened for, or the driver refuses) the
    /// encoder is rebuilt. Either way the next frame is an IDR, from a fresh
    /// desktop image.
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
                // A new duplication hands over the whole desktop at once,
                // which a still desktop otherwise wouldn't.
                self.switch_to(self.display, false)
            }
            Err(e) => {
                tracing::debug!("resizing in place failed ({e}); opening a new encoder");
                self.max = (self.max.0.max(width), self.max.1.max(height));
                self.switch_to(self.display, true)
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

    /// Reopens duplication after it was lost, retrying while the desktop is
    /// unavailable (mode change, UAC prompt). Moves to the desktop that
    /// shows now first: the lock screen and UAC prompts are on their own,
    /// which only a host running as SYSTEM can capture (see the service).
    /// If the display disappeared, falls back to the primary display.
    /// Returns false on shutdown.
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

    /// Queues the converter's current image for encoding; it is handed off
    /// once it comes out ([`deliver`]).
    fn encode(&mut self, now: Instant, job: EncodeJob) -> Result<bool> {
        let start = Instant::now();
        let intra = std::mem::take(&mut self.intra_next);
        // A keyframe already reconfigures the encoder (for its larger
        // budget): a waiting bitrate change rides along with that.
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
            // Only the first frame after an invalidation needs the tag;
            // later ones build on it. A keyframe needs none.
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
            &self.enc.gpu.context,
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
            self.bitrate.applied(now, actual);
        }
        self.frame_index += 1;
        // A frame lost on the way out comes back as a report.
        self.sched.encoded(now, job, true);
        Ok(true)
    }
}

/// Hands each frame out of the encoder to the consumer, on the encoder's
/// output thread, and tells the capture thread about any that didn't go.
fn deliver(
    frames: mpsc::Sender<EncodedFrame>,
    reports: std_mpsc::Sender<Report>,
) -> impl FnMut(Finished<FrameMeta>) + Send + 'static {
    let mut stats = Stats::default();
    move |out| {
        let encoded = match out {
            Ok(encoded) => encoded,
            Err((meta, e)) => {
                // The capture thread rebuilds the encoder on its next frame.
                tracing::warn!(index = meta.index, "encoding failed: {e}");
                let _ = reports.send(Report::Lost {
                    from: meta.recovers_from.unwrap_or(meta.index),
                    keyframe: true,
                });
                return;
            }
        };
        let meta = encoded.meta;
        let keyframe = encoded.keyframe;
        let bytes = encoded.data.len();
        let frame = EncodedFrame {
            display: meta.display,
            codec: meta.codec,
            frame_index: meta.index,
            keyframe,
            recovers_from: meta.recovers_from.filter(|_| !keyframe),
            capture_time: mono_micros(meta.captured),
            encode_done_time: mono_now(),
            width: meta.width,
            height: meta.height,
            data: encoded.data,
        };
        let out = match frames.try_send(frame) {
            Ok(()) => {
                stats.encoded += 1;
                "sent"
            }
            Err(TrySendError::Full(frame)) => {
                // The receiver sees a gap in frame_index: the capture
                // thread has the next frame skip it, or be an IDR.
                stats.dropped += 1;
                let _ = reports.send(Report::Lost {
                    from: frame.recovers_from.unwrap_or(frame.frame_index),
                    keyframe: frame.keyframe,
                });
                "full"
            }
            Err(TrySendError::Closed(_)) => {
                let _ = reports.send(Report::Closed);
                return;
            }
        };
        stats.encode_time += encoded.encode_time;
        let t = encoded.timing;
        let ms =
            |from: Instant, to: Instant| to.saturating_duration_since(from).as_secs_f64() * 1000.0;
        diag!(
            "frame",
            "idx={} idr={} intra={} repair={} why={} repeat={} bytes={bytes} held_ms={:.2} enc_ms={:.2} kbps={} out={out} pre_ms={:.2} slot_ms={:.2} submit_ms={:.2} event_ms={:.2} bits_ms={:.2} inenc_ms={:.2} submit_at_us={} wait_at_us={} completed_at_us={} lock_retries={}",
            meta.index,
            meta.idr,
            meta.intra,
            meta.repair,
            meta.why.unwrap_or("-"),
            meta.repeat,
            meta.held.as_secs_f64() * 1000.0,
            encoded.encode_time.as_secs_f64() * 1000.0,
            meta.kbps,
            ms(meta.started, t.before_slot),
            ms(t.before_slot, t.slot_taken),
            ms(t.slot_taken, t.submitted),
            ms(t.submitted, t.completed),
            ms(t.completed, t.done),
            ms(t.slot_taken, t.done),
            mono_micros(t.submitted),
            mono_micros(t.wait_at),
            mono_micros(t.completed),
            t.lock_retries,
        );
        let done = Instant::now();
        let since = *stats.since.get_or_insert(done);
        if done - since >= Duration::from_secs(5) {
            let n = (stats.encoded + stats.dropped).max(1);
            tracing::debug!(
                fps = f64::from(stats.encoded) / (done - since).as_secs_f64(),
                dropped = stats.dropped,
                avg_encode_ms = stats.encode_time.as_secs_f64() * 1000.0 / f64::from(n),
                "video stats"
            );
            stats = Stats {
                since: Some(done),
                ..Default::default()
            };
        }
    }
}

fn same_luid(a: LUID, b: LUID) -> bool {
    a.LowPart == b.LowPart && a.HighPart == b.HighPart
}
