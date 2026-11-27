//! What every platform's capture thread shares: the handle and its commands, the tags a frame
//! carries through the encoder, and the hand-off of encoded frames to the consumer.

use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::{self, error::TrySendError};
use ws_diag::diag;
use ws_proto::{DisplayId, EncodedFrame, VideoCodec, mono_micros, mono_now};

use crate::encode::Finished;
use crate::{CaptureError, Result};

pub(crate) enum Command {
    Select(DisplayId, std_mpsc::Sender<Result<()>>),
    Resize(u32, u32, std_mpsc::Sender<Result<()>>),
    Keyframe,
    Recover(u64),
    Bitrate(u32),
}

/// Frames further back than this are not invalidated one by one; a keyframe is sent instead.
pub(crate) const MAX_INVALIDATE: u64 = 16;
/// While a due frame is held back for the send queue, how often the queue is looked at again.
pub(crate) const HOLD_POLL_MS: u32 = 2;

pub(crate) struct PipelineHandle {
    /// The only strong reference: dropping it closes the channel.
    commands: Option<Arc<std_mpsc::Sender<Command>>>,
    thread: Option<JoinHandle<()>>,
}

impl PipelineHandle {
    /// Runs `body` on a new high-priority capture thread and waits for it to report that it
    /// opened.
    pub(crate) fn spawn(
        body: impl FnOnce(std_mpsc::Receiver<Command>, std_mpsc::Sender<Result<()>>) + Send + 'static,
    ) -> Result<Self> {
        let (cmd_tx, cmd_rx) = std_mpsc::channel();
        let (ready_tx, ready_rx) = std_mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("ws-capture".into())
            .spawn(move || body(cmd_rx, ready_tx))
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
        self.commands.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// What the capture thread tags a frame with when it goes into the encoder, for [`deliver`] when it
/// comes out.
pub(crate) struct FrameMeta {
    pub index: u64,
    pub display: DisplayId,
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    pub captured: Instant,
    /// The frame skips frames from this index on.
    pub recovers_from: Option<u64>,
    // For diagnostics.
    pub idr: bool,
    pub intra: bool,
    pub repair: bool,
    pub why: Option<&'static str>,
    /// Re-encodes the last image.
    pub repeat: bool,
    /// Captured -> the start of `encode`.
    pub held: Duration,
    /// Where `held` ends.
    pub started: Instant,
    pub kbps: u32,
}

/// From the encoder's output thread back to the capture thread.
pub(crate) enum Report {
    /// The frame `index` was not handed off, nor anything after it still in the encoder that built
    /// on it; frames from `from` on are missing.
    Lost { from: u64, keyframe: bool },
    /// A frame left the encoder, `bytes` long: what it cost, for pacing by bitrate.
    Done { bytes: usize },
    /// The consumer has gone away.
    Closed,
}

#[derive(Default)]
struct Stats {
    since: Option<Instant>,
    encoded: u32,
    dropped: u32,
    encode_time: Duration,
}

/// Hands each frame out of the encoder to the consumer, on the encoder's output thread, and tells
/// the capture thread about any that didn't go.
pub(crate) fn deliver(
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
                let _ = reports.send(Report::Done { bytes: 0 });
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
        let _ = reports.send(Report::Done { bytes });
        let frame = EncodedFrame {
            // The host numbers the stream this pipeline feeds.
            lane: 0,
            display: meta.display,
            codec: meta.codec,
            frame_index: meta.index,
            keyframe,
            recovers_from: meta.recovers_from.filter(|_| !keyframe),
            capture_time: mono_micros(meta.captured),
            encode_start_time: mono_micros(meta.started),
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
