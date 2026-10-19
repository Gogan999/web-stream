//! Runs the media side of a connection: starts capture, game audio, the mic
//! and input when the transport connects, stops them when it disconnects,
//! and answers the browser's `control` messages.
//!
//! Everything here reacts to [`TransportEvent`]s on one task. Starting and
//! switching the capture pipeline blocks for tens of milliseconds, which is
//! done in place (`block_in_place`), as are the thread joins when a pipeline
//! stops; events queue in the transport meanwhile.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::block_in_place;
use ws_audio::{LoopbackConfig, MicConfig, MicReceiver};
use ws_capture::VideoPipeline;
use ws_input::DisplayRect;
use ws_proto::{
    ClientControl, DisplayId, DisplayInfo, EncodedFrame, HostControl, VideoCodec, VideoConfig,
    VideoStats, channels,
};
use ws_transport::{Transport, TransportConfig, TransportEvent, TransportStats};

use crate::cursor::{CursorSession, CursorTarget};
use crate::files::{FileCommand, FileMsg, FileSession};
use crate::input::InputThread;
use crate::options::MediaOptions;
use crate::platform;
use crate::serve::Event;
use crate::uploads::{self, UploadMsg, UploadSession};

/// Encoded frames waiting for the transport. The transport keeps only the
/// newest frame anyway; a full channel makes the encoder drop and send a
/// keyframe next.
const FRAME_QUEUE: usize = 1;
/// About 40 ms of 10 ms Opus packets.
const AUDIO_QUEUE: usize = 4;
const CONTROL_QUEUE: usize = 64;
/// A pipeline that stopped by itself is restarted after this pause.
const VIDEO_RESTART_DELAY: Duration = Duration::from_secs(1);
/// Encode this far below the bandwidth estimate. The transport's pacer sends
/// at 1.1x the estimate, and RTP and encryption add about 5%, so at 0.9 a
/// fast-motion stream (every frame at its full budget) used nearly all of
/// the send rate: any dip backed the queue up, frames went out late and
/// were dropped, and each drop forced a keyframe. That showed as the picture
/// going soft and sharpening again several times a second. 0.75 leaves the
/// pacer a third more than the encoder makes.
const BWE_HEADROOM: f64 = 0.75;
/// Increases smaller than this fraction are not passed to the encoder.
const BITRATE_STEP_UP: f64 = 0.1;
/// Decreases follow sooner: an encoder above the send rate backs up the
/// queue.
const BITRATE_STEP_DOWN: f64 = 0.02;
const MIN_KBPS: u32 = 500;
/// Room above the video ceiling the bandwidth estimate may probe for, for
/// game audio and data channels.
const NON_VIDEO_BPS: u64 = 500_000;
/// Replies kept while the `control` channel is not open yet.
const UNSENT_MAX: usize = 16;
/// A `setVideoConfig` that needs a new pipeline waits until the last one is
/// at least this old.
const MIN_RESTART_GAP: Duration = Duration::from_secs(1);

/// Video codecs the browser's answer accepted; `None` until known.
pub type AcceptedCodecs = watch::Receiver<Option<Vec<VideoCodec>>>;

/// How many peer connections signaling has set up. A count rather than an
/// event, so a busy loop can't miss one.
pub type PeerCount = watch::Receiver<u64>;

/// What signaling tells media about the current peer connection.
pub struct Signaled {
    pub codecs: AcceptedCodecs,
    pub peers: PeerCount,
}

pub async fn run(
    transport: Transport,
    mut events: mpsc::Receiver<TransportEvent>,
    opts: MediaOptions,
    signaled: Signaled,
    mut file_commands: mpsc::Receiver<FileCommand>,
    host_events: mpsc::UnboundedSender<Event>,
    mut shutdown: oneshot::Receiver<()>,
) {
    let (control_tx, mut control_rx) = mpsc::channel(CONTROL_QUEUE);
    let (stopped_tx, mut stopped_rx) = mpsc::channel(4);
    let (later_tx, mut later_rx) = mpsc::channel(8);
    let (cursor_target, _) = watch::channel(CursorTarget::default());
    let Signaled { codecs, mut peers } = signaled;
    let mut media = Media {
        transport,
        config: opts.video.clone(),
        display: opts.display,
        // Off until the browser turns it on.
        mic_enabled: false,
        audio: (opts.audio_kbps, opts.audio_frame_ms),
        opts,
        control_tx,
        control_open: false,
        unsent: Vec::new(),
        stopped_tx,
        later_tx,
        cursor_target,
        display_origin: (0, 0),
        session: 0,
        rebuild_pending: false,
        generation: 0,
        last_bwe: None,
        last_restart: None,
        codecs,
        live: None,
        transfers: None,
        peer: 0,
        peers: peers.clone(),
        transitions: 0,
        sent: Arc::default(),
        keyframe_requests: 0,
        changed_modes: ChangedModes::default(),
        mode_busy: false,
        last_mode_change: None,
        host_events,
    };
    media.set_ceiling();
    loop {
        tokio::select! {
            ev = events.recv() => match ev {
                Some(ev) => {
                    // A new peer's events must not reach the old one's transfers.
                    media.sync_peer();
                    let down = matches!(ev, TransportEvent::Disconnected);
                    media.event(ev);
                    if down {
                        media.restore_host_resolution();
                    }
                    // Replies meant for the browser that just left.
                    while down && control_rx.try_recv().is_ok() {}
                }
                None => break,
            },
            Some(msg) = control_rx.recv() => media.send_control(msg).await,
            Some(generation) = stopped_rx.recv() => media.video_stopped(generation),
            Some(later) = later_rx.recv() => media.later(later),
            Some(cmd) = file_commands.recv() => media.file_command(cmd),
            Ok(()) = peers.changed() => media.sync_peer(),
            _ = &mut shutdown => break,
        }
    }
    media.stop();
    // A resolution change still running would never be put back: wait for
    // it, so it is recorded. The tray app keeps running after the host
    // stops, so nothing else would restore it.
    if media.mode_busy {
        let wait = tokio::time::timeout(MODE_CHANGE_WAIT, async {
            while let Some(later) = later_rx.recv().await {
                if let Later::ModeSet { id, result, .. } = later {
                    if result.is_ok_and(|c| c.changed) {
                        media.changed_modes.add(id);
                    }
                    break;
                }
            }
        });
        if wait.await.is_err() {
            tracing::warn!("a resolution change did not finish; it may not be put back");
        }
    }
    // Dropping `media` puts changed display resolutions back (ChangedModes),
    // here and if this task panics.
}

/// How long stopping waits for a resolution change in progress.
const MODE_CHANGE_WAIT: Duration = Duration::from_secs(10);

/// Work finished off the event loop, or due later.
enum Later {
    /// A deferred pipeline rebuild may run, if `session` is still current.
    Rebuild { session: u64 },
    ModeSet {
        session: u64,
        id: DisplayId,
        result: ws_capture::Result<ws_capture::ModeChange>,
    },
    /// A second after a resolution change, when the desktop has settled.
    ModeSettled { session: u64 },
    /// The connection has been down for [`TRANSFER_GRACE`]: end `peer`'s
    /// file transfers if it hasn't come back.
    TransfersExpire { peer: u64 },
}

/// How long file transfers wait for a dropped connection to come back.
const TRANSFER_GRACE: Duration = Duration::from_secs(30);

/// File transfers both ways for one peer connection. They live as long as
/// its channels, so they carry on when ICE drops and comes back.
struct Transfers {
    files: FileSession,
    uploads: UploadSession,
}

/// Displays whose resolution this host changed. Dropping it puts them back,
/// so a panic in the media task doesn't leave the user's screen changed.
#[derive(Default)]
struct ChangedModes(Vec<DisplayId>);

impl ChangedModes {
    fn add(&mut self, id: DisplayId) {
        if !self.0.contains(&id) {
            self.0.push(id);
        }
    }

    fn take(&mut self) -> Vec<DisplayId> {
        std::mem::take(&mut self.0)
    }
}

impl Drop for ChangedModes {
    fn drop(&mut self) {
        restore_modes(self.take());
    }
}

fn restore_modes(ids: Vec<DisplayId>) {
    for id in ids {
        if let Err(e) = ws_capture::restore_display_mode(id) {
            tracing::warn!(display = id.0, "restore resolution: {e}");
        }
    }
}

struct Media {
    transport: Transport,
    opts: MediaOptions,
    /// What the browser chose; kept across reconnects.
    config: VideoConfig,
    display: Option<DisplayId>,
    mic_enabled: bool,
    /// Game audio bitrate (kb/s) and Opus frame length (ms).
    audio: (u32, u32),
    control_tx: mpsc::Sender<HostControl>,
    /// Replies made before the `control` channel opened (an error while
    /// starting media, say) wait here.
    control_open: bool,
    unsent: Vec<HostControl>,
    stopped_tx: mpsc::Sender<u64>,
    /// Hands work done elsewhere (or due later) back to the loop.
    later_tx: mpsc::Sender<Later>,
    /// Steers the session's cursor sender (see `cursor.rs`).
    cursor_target: watch::Sender<CursorTarget>,
    /// Desktop position of the streamed display's top-left corner; pointer
    /// positions are sent relative to it.
    display_origin: (i32, i32),
    /// Bumped on every disconnect, so late results for a browser that left
    /// are recognised.
    session: u64,
    /// `config` changed while the last rebuild was too recent; rebuild on
    /// [`Later::Rebuild`].
    rebuild_pending: bool,
    /// Bumped for every pipeline started, so a stale "stopped" is ignored.
    generation: u64,
    /// Latest bandwidth estimate of this connection, in bits per second.
    last_bwe: Option<u64>,
    last_restart: Option<Instant>,
    codecs: AcceptedCodecs,
    live: Option<Live>,
    /// Frames handed to the transport since the last stats report.
    sent: Arc<Mutex<SentFrames>>,
    keyframe_requests: u32,
    /// Displays whose resolution this session changed, to put back.
    changed_modes: ChangedModes,
    /// A resolution change or restore is running.
    mode_busy: bool,
    last_mode_change: Option<Instant>,
    /// What the host user sees.
    host_events: mpsc::UnboundedSender<Event>,
    /// From when the `control` channel opens until the peer connection is
    /// replaced (or stays down for [`TRANSFER_GRACE`]).
    transfers: Option<Transfers>,
    /// The peer connection `transfers` belong to.
    peer: u64,
    peers: PeerCount,
    /// Connected and Disconnected events handled, for
    /// [`Transport::set_input_sink`].
    transitions: u64,
}

#[derive(Debug, Default)]
struct SentFrames {
    frames: u32,
    bytes: u64,
    /// Sum of capture-to-encoded times.
    encode_micros: u64,
}

/// What runs while a browser is connected. Fields drop in order: input is
/// released first.
struct Live {
    input: Option<InputThread>,
    _cursor: CursorSession,
    video: Option<Video>,
    /// Held only to keep game audio capture running.
    audio: Option<platform::Loopback>,
    mic: Option<(platform::Mic, MicReceiver)>,
    bitrate_kbps: u32,
}

struct Video {
    pipeline: VideoPipeline,
    generation: u64,
}

impl Media {
    fn event(&mut self, ev: TransportEvent) {
        match ev {
            TransportEvent::Connected => {
                self.transitions += 1;
                self.start()
            }
            TransportEvent::Disconnected => {
                self.transitions += 1;
                self.stop()
            }
            TransportEvent::KeyframeNeeded => {
                if let Some(v) = self.live.as_ref().and_then(|l| l.video.as_ref()) {
                    self.keyframe_requests += 1;
                    v.pipeline.request_keyframe();
                }
            }
            TransportEvent::FramesLost(from) => {
                if let Some(v) = self.live.as_ref().and_then(|l| l.video.as_ref()) {
                    v.pipeline.recover_from(from);
                }
            }
            TransportEvent::TargetBitrate(bps) => {
                let video = self.live.as_ref().is_some_and(|l| l.video.is_some());
                self.to_files(FileMsg::Bitrate(video.then_some(bps)));
                self.target_bitrate(bps)
            }
            TransportEvent::Data {
                channel,
                binary,
                data,
            } => match channel {
                channels::CONTROL if !binary => self.control(&data),
                channels::FILE if binary => self.to_uploads(UploadMsg::Data(data)),
                other => tracing::debug!(channel = other, binary, "ignoring data channel message"),
            },
            TransportEvent::MicAudio(packet) => {
                if let Some((_, rx)) = self.live.as_ref().and_then(|l| l.mic.as_ref()) {
                    // The transport extends RTP sequence numbers; the jitter
                    // buffer wants the wire value back.
                    rx.push(packet.sequence as u16, &packet.data);
                }
            }
            TransportEvent::Stats(s) => {
                tracing::debug!(?s, "transport stats");
                self.report_stats(&s);
            }
            TransportEvent::ChannelOpen(c) => {
                tracing::debug!(channel = c, "channel open");
                if c == channels::CONTROL {
                    self.control_open = true;
                    self.transfers = Some(Transfers {
                        files: FileSession::start(
                            self.transport.clone(),
                            self.control_tx.clone(),
                            self.host_events.clone(),
                        ),
                        uploads: UploadSession::start(
                            self.opts.upload_dir.clone(),
                            self.control_tx.clone(),
                            self.host_events.clone(),
                        ),
                    });
                    self.reply(HostControl::Uploads {
                        enabled: self.opts.upload_dir.is_some(),
                        max_size: uploads::MAX_SIZE,
                    });
                    for msg in std::mem::take(&mut self.unsent) {
                        self.reply(msg);
                    }
                    let origin = self.display_origin;
                    self.cursor_target.send_modify(|t| t.origin = Some(origin));
                }
            }
            TransportEvent::ChannelClosed(c) => {
                tracing::debug!(channel = c, "channel closed");
                if c == channels::CONTROL {
                    self.control_open = false;
                    self.transfers = None;
                    self.cursor_target.send_modify(|t| t.origin = None);
                }
            }
            TransportEvent::ChannelLow(channels::FILE) => self.to_files(FileMsg::ChannelLow),
            TransportEvent::ChannelLow(_) => {}
        }
    }

    fn start(&mut self) {
        if let Some(live) = &self.live {
            // Re-established without a reported drop; just resync the decoder
            // (and point input at the input thread again: Connected cleared
            // it).
            if let Some(v) = &live.video {
                v.pipeline.request_keyframe();
            }
            if let Some(input) = &live.input {
                self.route_input(input);
            }
            return;
        }
        tracing::info!("browser connected; starting media");
        let rumble = {
            let tx = self.control_tx.clone();
            move |msg| {
                let _ = tx.try_send(msg);
            }
        };
        let display_rect = self.chosen_display().map(|d| rect(&d));
        let input = InputThread::start(display_rect, rumble)
            .inspect_err(|e| tracing::error!("input thread: {e}"))
            .ok();
        // Straight from the transport to the input thread: input never
        // waits behind this loop (starting capture takes a while).
        if let Some(input) = &input {
            self.route_input(input);
        }

        let audio = if self.opts.game_audio {
            let (tx, rx) = mpsc::channel(AUDIO_QUEUE);
            let config = LoopbackConfig {
                device: self.opts.audio_device.clone(),
                bitrate: self.audio.0 * 1000,
                frame_ms: self.audio.1,
            };
            match block_in_place(|| platform::start_loopback(config, tx)) {
                Ok(capture) => {
                    tokio::spawn(forward_audio(rx, self.transport.clone()));
                    Some(capture)
                }
                Err(e) => {
                    tracing::warn!("game audio off: {e}");
                    None
                }
            }
        } else {
            None
        };

        let cursor = CursorSession::start(self.transport.clone(), self.cursor_target.subscribe());
        if self.transfers.is_some() {
            // ICE came back on the same peer connection: its channels, open
            // before the drop, are still open.
            self.control_open = true;
        }
        self.live = Some(Live {
            input,
            _cursor: cursor,
            video: None,
            audio,
            mic: None,
            bitrate_kbps: self.start_kbps(),
        });
        if self.mic_enabled {
            self.start_mic();
        }
        if let Err(e) = self.start_video() {
            tracing::error!("video: {e}");
            self.error(format!("video is not available: {e}"));
        }
    }

    fn stop(&mut self) {
        self.control_open = false;
        self.unsent.clear();
        self.last_bwe = None;
        self.rebuild_pending = false;
        self.session += 1;
        self.keyframe_requests = 0;
        self.cursor_target.send_replace(CursorTarget::default());
        // The next browser turns the mic on if it wants it, and sets its
        // own audio quality.
        self.mic_enabled = false;
        self.audio = (self.opts.audio_kbps, self.opts.audio_frame_ms);
        *self.sent.lock().expect("sent stats lock") = SentFrames::default();
        self.transport.clear_input_sink();
        if let Some(live) = self.live.take() {
            drop_blocking(live);
            tracing::info!("browser disconnected; media stopped");
        }
        if self.transfers.is_some() {
            let (tx, peer) = (self.later_tx.clone(), self.peer);
            tokio::spawn(async move {
                tokio::time::sleep(TRANSFER_GRACE).await;
                let _ = tx.send(Later::TransfersExpire { peer }).await;
            });
        }
    }

    /// When signaling has set up a new peer connection, the old one's file
    /// transfers end.
    fn sync_peer(&mut self) {
        let peer = *self.peers.borrow();
        if peer != self.peer {
            self.peer = peer;
            self.transfers = None;
        }
    }

    /// Input goes from the transport straight to `input`, unless this loop
    /// is behind on connection changes (then the connection `input` was
    /// started for is gone, and a later Connected sets the right one).
    fn route_input(&self, input: &InputThread) {
        if !self
            .transport
            .set_input_sink(Box::new(input.sink()), self.transitions)
        {
            tracing::debug!("input thread is for an earlier connection; not routing input to it");
        }
    }

    fn start_mic(&mut self) {
        let Some(device) = self.opts.mic_device.clone() else {
            return;
        };
        let Some(live) = &mut self.live else { return };
        if live.mic.is_some() {
            return;
        }
        let config = MicConfig {
            device,
            ..Default::default()
        };
        match block_in_place(|| platform::start_mic(config)) {
            Ok(mic) => live.mic = Some(mic),
            Err(e) => {
                tracing::warn!("browser mic off: {e}");
                self.mic_enabled = false;
                self.reply(HostControl::MicUnavailable {
                    message: format!("the mic can't play on the host: {e}"),
                });
            }
        }
    }

    /// The display to stream: the client's or configured choice if it still
    /// exists, else the primary.
    fn chosen_display(&self) -> Option<DisplayInfo> {
        let displays = ws_capture::list_displays().ok()?;
        pick_display(&displays, self.display).cloned()
    }

    /// (Re)starts capture with the current config. Any running pipeline is
    /// stopped first: two on one GPU output can fail.
    fn start_video(&mut self) -> anyhow::Result<()> {
        let Some(live) = &mut self.live else {
            return Ok(());
        };
        if let Some(video) = live.video.take() {
            drop_blocking(video);
        }
        let displays = ws_capture::list_displays()?;
        let info = pick_display(&displays, self.display)
            .ok_or_else(|| anyhow::anyhow!("no display to capture"))?
            .clone();
        let accepted = self.codecs.borrow().clone();
        let codec = pick_codec(self.config.codec, accepted.as_deref()).ok_or_else(|| {
            anyhow::anyhow!("the browser accepted none of the video codecs the host encodes")
        })?;
        if codec != self.config.codec {
            let msg = format!(
                "the browser can't decode {:?}; streaming {codec:?} instead",
                self.config.codec
            );
            tracing::warn!("{msg}");
            self.error(msg);
        }
        let start_kbps = self.start_kbps();
        let config = VideoConfig {
            codec,
            bitrate_kbps: start_kbps,
            ..self.config.clone()
        };
        let (tx, rx) = mpsc::channel(FRAME_QUEUE);
        let Some(live) = &mut self.live else {
            return Ok(());
        };
        let pipeline = block_in_place(|| VideoPipeline::start(info.id, config.clone(), tx))?;
        self.generation += 1;
        tokio::spawn(forward_video(
            rx,
            self.transport.clone(),
            self.sent.clone(),
            self.stopped_tx.clone(),
            self.generation,
        ));
        live.video = Some(Video {
            pipeline,
            generation: self.generation,
        });
        live.bitrate_kbps = start_kbps;
        self.last_restart = Some(Instant::now());
        if let Some(input) = &live.input {
            input.set_display(rect(&info));
        }
        self.display = Some(info.id);
        self.set_display_origin(&info);
        tracing::info!(display = info.id.0, ?config, "streaming");
        Ok(())
    }

    fn video_stopped(&mut self, generation: u64) {
        let current = self
            .live
            .as_ref()
            .and_then(|l| l.video.as_ref())
            .is_some_and(|v| v.generation == generation);
        if !current {
            return;
        }
        tracing::warn!("capture pipeline stopped; restarting it");
        if let Err(e) = self.start_video() {
            tracing::error!("video: {e}");
            self.error(format!("video stopped: {e}"));
        }
    }

    /// Where the encoder starts: under the current estimate (or the
    /// transport's starting estimate), never above the configured rate.
    fn start_kbps(&self) -> u32 {
        let bwe = self
            .last_bwe
            .unwrap_or_else(|| TransportConfig::default().initial_bitrate_bps);
        target_kbps(bwe, self.config.bitrate_kbps)
    }

    /// Lets the bandwidth estimate probe up to the browser's bitrate
    /// setting (plus room for audio), and no further.
    fn set_ceiling(&self) {
        let video = (f64::from(self.config.bitrate_kbps) * 1000.0 / BWE_HEADROOM) as u64;
        self.transport.set_max_bitrate(video + NON_VIDEO_BPS);
    }

    /// Tells the browser how the host side of the stream is doing.
    fn report_stats(&mut self, s: &TransportStats) {
        let Some(live) = &self.live else { return };
        if live.video.is_none() || !self.control_open {
            return;
        }
        let sent = std::mem::take(&mut *self.sent.lock().expect("sent stats lock"));
        let keyframe_requests = std::mem::take(&mut self.keyframe_requests);
        // Transport stats arrive once a second.
        self.reply(HostControl::VideoStats(VideoStats {
            encoder_kbps: live.bitrate_kbps,
            max_kbps: self.config.bitrate_kbps,
            bwe_kbps: s.bwe.map(|b| (b / 1000) as u32),
            fps: sent.frames as f32,
            sent_kbps: (sent.bytes * 8 / 1000) as u32,
            encode_ms: if sent.frames == 0 {
                0.0
            } else {
                sent.encode_micros as f32 / sent.frames as f32 / 1000.0
            },
            keyframe_requests,
            loss: s.egress_loss,
            browser_keyframe_requests: s.trouble.browser_keyframe_requests,
            late_frames: s.trouble.late_frames,
            lost_frames: s.trouble.lost_frames,
            send_drops: s.trouble.send_drops,
            recovered_frames: s.trouble.recovered,
        }));
    }

    fn target_bitrate(&mut self, bps: u64) {
        self.last_bwe = Some(bps);
        self.apply_bitrate(false);
    }

    /// Moves the encoder to the target for the latest estimate, unless the
    /// change is too small to matter.
    fn apply_bitrate(&mut self, force: bool) {
        let kbps = self.start_kbps();
        let Some(live) = &mut self.live else { return };
        let Some(video) = &live.video else { return };
        if force || worth_changing(live.bitrate_kbps, kbps) {
            tracing::debug!(kbps, "encoder bitrate");
            video.pipeline.set_bitrate(kbps);
            live.bitrate_kbps = kbps;
        }
    }

    fn control(&mut self, data: &[u8]) {
        let msg: ClientControl = match serde_json::from_slice(data) {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!("bad control message: {e}");
                return;
            }
        };
        tracing::debug!(?msg, "control");
        match msg {
            ClientControl::ListDisplays => {
                self.send_displays();
                // And what is streaming, so the browser's settings start
                // from the host's.
                self.display_changed();
            }
            ClientControl::SetHostResolution { width, height } => {
                self.set_host_resolution(width, height)
            }
            ClientControl::RestoreHostResolution => self.restore_host_resolution(),
            ClientControl::SelectDisplay { id } => self.select_display(id),
            ClientControl::RequestKeyframe => {
                if let Some(v) = self.live.as_ref().and_then(|l| l.video.as_ref()) {
                    self.keyframe_requests += 1;
                    v.pipeline.request_keyframe();
                }
            }
            ClientControl::SetVideoConfig { config } => self.set_video_config(config.clamped()),
            ClientControl::SetAudioConfig {
                bitrate_kbps,
                frame_ms,
            } => self.set_audio_config(bitrate_kbps, frame_ms),
            ClientControl::SetMicEnabled { enabled } => {
                if enabled && self.opts.mic_device.is_none() {
                    self.reply(HostControl::MicUnavailable {
                        message: "the host has the mic turned off (--no-mic)".into(),
                    });
                    return;
                }
                self.mic_enabled = enabled;
                if enabled {
                    self.start_mic();
                } else if let Some(mic) = self.live.as_mut().and_then(|l| l.mic.take()) {
                    drop_blocking(mic);
                }
            }
            ClientControl::RequestFile { id } => self.to_files(FileMsg::Request(id)),
            ClientControl::FileAck { id, received } => self.to_files(FileMsg::Ack { id, received }),
            ClientControl::CancelFile { id } => self.to_files(FileMsg::Cancel(id)),
            ClientControl::UploadFile { id, name, size } => {
                self.to_uploads(UploadMsg::Offer { id, name, size })
            }
            ClientControl::CancelUpload { id } => self.to_uploads(UploadMsg::Cancel(id)),
            ClientControl::SetCursorPositions { enabled } => {
                self.cursor_target.send_if_modified(|t| {
                    let changed = t.positions != enabled;
                    t.positions = enabled;
                    changed
                });
            }
            ClientControl::Ping { client_time_ms } => {
                self.reply(HostControl::Pong { client_time_ms });
            }
        }
    }

    fn to_files(&self, msg: FileMsg) {
        if let Some(t) = &self.transfers {
            t.files.send(msg);
        }
    }

    fn to_uploads(&self, msg: UploadMsg) {
        if let Some(t) = &self.transfers {
            t.uploads.send(msg);
        }
    }

    fn file_command(&self, cmd: FileCommand) {
        match (&self.transfers, &self.live) {
            (Some(t), Some(_)) => t.files.send(FileMsg::Command(cmd)),
            (Some(t), None) if matches!(cmd, FileCommand::Stop(_)) => {
                t.files.send(FileMsg::Command(cmd))
            }
            _ => {
                if matches!(cmd, FileCommand::Share(_)) {
                    let _ = self.host_events.send(Event::Error(
                        "no browser is connected: connect one, then share the file".into(),
                    ));
                }
            }
        }
    }

    fn set_audio_config(&mut self, kbps: Option<u32>, frame_ms: Option<u32>) {
        let frame_ms = frame_ms.unwrap_or(self.opts.audio_frame_ms);
        if !ws_audio::FRAME_MS.contains(&frame_ms) {
            self.error(format!("audio frames are 10 or 20 ms, not {frame_ms}"));
            return;
        }
        let kbps = kbps.unwrap_or(self.opts.audio_kbps).clamp(32, 510);
        self.audio = (kbps, frame_ms);
        if let Some(capture) = self.live.as_ref().and_then(|l| l.audio.as_ref()) {
            platform::set_audio(capture, kbps * 1000, frame_ms);
        }
        tracing::info!(kbps, frame_ms, "game audio");
    }

    fn send_displays(&self) {
        let displays = ws_capture::list_displays().unwrap_or_else(|e| {
            tracing::debug!("list displays: {e}");
            Vec::new()
        });
        let active = pick_display(&displays, self.display).map(|d| d.id);
        self.reply(HostControl::Displays { displays, active });
    }

    /// Points input (and pointer positions) at the streamed display's
    /// current position and size.
    fn refresh_input_rect(&mut self) {
        let Some(info) = self.chosen_display() else {
            return;
        };
        if let Some(input) = self.live.as_ref().and_then(|l| l.input.as_ref()) {
            input.set_display(rect(&info));
        }
        self.set_display_origin(&info);
    }

    fn set_display_origin(&mut self, info: &DisplayInfo) {
        let origin = (info.x, info.y);
        self.display_origin = origin;
        self.cursor_target.send_if_modified(|t| match t.origin {
            Some(o) if o != origin => {
                t.origin = Some(origin);
                true
            }
            _ => false,
        });
    }

    fn set_host_resolution(&mut self, width: u32, height: u32) {
        if !self.opts.allow_resolution_change {
            self.error(
                "the host doesn't allow changing its resolution; \
                 start it with --allow-resolution-change"
                    .into(),
            );
            return;
        }
        if self.live.is_none() {
            return;
        }
        if self.mode_busy
            || self
                .last_mode_change
                .is_some_and(|t| t.elapsed() < MIN_RESTART_GAP)
        {
            self.error("the host resolution is still changing; try again in a moment".into());
            return;
        }
        let Some(id) = self.chosen_display().map(|d| d.id) else {
            self.error("no display to change".into());
            return;
        };
        // A mode change can take seconds; keep this loop (and input) moving.
        self.mode_busy = true;
        let (tx, session) = (self.later_tx.clone(), self.session);
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                ws_capture::set_display_mode(id, width, height)
            })
            .await
            .unwrap_or_else(|e| Err(ws_capture::CaptureError::Platform(e.to_string())));
            let _ = tx
                .send(Later::ModeSet {
                    session,
                    id,
                    result,
                })
                .await;
        });
    }

    fn mode_set(
        &mut self,
        session: u64,
        id: DisplayId,
        result: ws_capture::Result<ws_capture::ModeChange>,
    ) {
        self.mode_busy = false;
        self.last_mode_change = Some(Instant::now());
        match result {
            Ok(change) => {
                if change.changed {
                    self.changed_modes.add(id);
                }
                if session != self.session {
                    // The browser left meanwhile.
                    self.restore_host_resolution();
                    return;
                }
                tracing::info!(
                    display = id.0,
                    change.width,
                    change.height,
                    "host resolution matched to the browser"
                );
                // Capture picks the new size up by itself (duplication is
                // lost and reopened); input needs the new rectangle, now and
                // again once the desktop has settled.
                self.refresh_input_rect();
                self.send_displays();
                let tx = self.later_tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(MIN_RESTART_GAP).await;
                    let _ = tx.send(Later::ModeSettled { session }).await;
                });
            }
            Err(e) => {
                tracing::warn!(display = id.0, "change resolution: {e}");
                if session == self.session {
                    self.error(format!("could not change the host resolution: {e}"));
                }
            }
        }
    }

    /// Puts changed displays back, off the event loop.
    fn restore_host_resolution(&mut self) {
        let ids = self.changed_modes.take();
        if ids.is_empty() {
            return;
        }
        self.last_mode_change = Some(Instant::now());
        let (tx, session) = (self.later_tx.clone(), self.session);
        tokio::spawn(async move {
            let _ = tokio::task::spawn_blocking(move || restore_modes(ids)).await;
            let _ = tx.send(Later::ModeSettled { session }).await;
        });
    }

    fn later(&mut self, later: Later) {
        match later {
            Later::Rebuild { session } => {
                if session == self.session
                    && std::mem::take(&mut self.rebuild_pending)
                    && self.live.as_ref().is_some_and(|l| l.video.is_some())
                {
                    self.rebuild();
                }
            }
            Later::ModeSet {
                session,
                id,
                result,
            } => self.mode_set(session, id, result),
            Later::ModeSettled { session } => {
                if session == self.session && self.live.is_some() {
                    block_in_place(|| self.refresh_input_rect());
                    self.send_displays();
                }
            }
            Later::TransfersExpire { peer } => {
                if peer == self.peer && self.live.is_none() && self.transfers.take().is_some() {
                    tracing::info!("connection still down; file transfers ended");
                }
            }
        }
    }

    fn set_video_config(&mut self, config: VideoConfig) {
        let same_but_bitrate = VideoConfig {
            bitrate_kbps: self.config.bitrate_kbps,
            ..config.clone()
        } == self.config;
        let video_running = self.live.as_ref().is_some_and(|l| l.video.is_some());
        if same_but_bitrate || !video_running {
            // Nothing to rebuild: the bitrate changes in place, and anything
            // else applies when the pipeline next starts.
            let bitrate_changed = config.bitrate_kbps != self.config.bitrate_kbps;
            self.config = config;
            if bitrate_changed {
                self.set_ceiling();
                self.apply_bitrate(true);
            }
            self.display_changed();
            return;
        }
        self.config = config;
        self.set_ceiling();
        if let Some(wait) = self
            .last_restart
            .and_then(|t| MIN_RESTART_GAP.checked_sub(t.elapsed()))
        {
            // Too soon after the last rebuild: apply the newest settings
            // once the gap is over.
            if !self.rebuild_pending {
                self.rebuild_pending = true;
                let (tx, session) = (self.later_tx.clone(), self.session);
                tokio::spawn(async move {
                    tokio::time::sleep(wait).await;
                    let _ = tx.send(Later::Rebuild { session }).await;
                });
            }
            return;
        }
        self.rebuild();
    }

    /// Restarts the pipeline with the current config.
    fn rebuild(&mut self) {
        if let Err(e) = self.start_video() {
            tracing::error!("video: {e}");
            self.error(format!("could not apply video settings: {e}"));
            return;
        }
        self.display_changed();
    }

    fn select_display(&mut self, id: DisplayId) {
        let displays = ws_capture::list_displays().unwrap_or_default();
        let Some(info) = displays.iter().find(|d| d.id == id) else {
            self.error(format!("display {} not found", id.0));
            return;
        };
        if let Some(live) = &self.live {
            if let Some(video) = &live.video {
                if let Err(e) = block_in_place(|| video.pipeline.select_display(id)) {
                    tracing::error!("select display {}: {e}", id.0);
                    self.error(format!("could not switch display: {e}"));
                    return;
                }
            }
            if let Some(input) = &live.input {
                input.set_display(rect(info));
            }
        }
        let info = info.clone();
        self.set_display_origin(&info);
        self.display = Some(id);
        tracing::info!(display = id.0, "display selected");
        self.display_changed();
    }

    fn display_changed(&self) {
        if let Some(id) = self.display {
            let accepted = self.codecs.borrow().clone();
            let codec = pick_codec(self.config.codec, accepted.as_deref());
            self.reply(HostControl::DisplayChanged {
                id,
                config: VideoConfig {
                    codec: codec.unwrap_or(self.config.codec),
                    ..self.config.clone()
                },
            });
        }
    }

    fn error(&self, message: String) {
        self.reply(HostControl::Error { message });
    }

    fn reply(&self, msg: HostControl) {
        if self.control_tx.try_send(msg).is_err() {
            tracing::warn!("control queue full; dropping reply");
        }
    }

    async fn send_control(&mut self, msg: HostControl) {
        if !self.control_open {
            if self.unsent.len() < UNSENT_MAX {
                self.unsent.push(msg);
            }
            return;
        }
        let json = match serde_json::to_vec(&msg) {
            Ok(j) => j,
            Err(e) => {
                tracing::error!("encode control message: {e}");
                return;
            }
        };
        match self
            .transport
            .send_data(channels::CONTROL, false, json)
            .await
        {
            Ok(true) => {}
            Ok(false) => tracing::warn!("control channel full; dropped {msg:?}"),
            Err(e) => tracing::debug!("control message not sent: {e}"),
        }
    }
}

async fn forward_video(
    mut frames: mpsc::Receiver<EncodedFrame>,
    transport: Transport,
    sent: Arc<Mutex<SentFrames>>,
    stopped: mpsc::Sender<u64>,
    generation: u64,
) {
    while let Some(frame) = frames.recv().await {
        {
            let mut sent = sent.lock().expect("sent stats lock");
            sent.frames += 1;
            sent.bytes += frame.data.len() as u64;
            sent.encode_micros += frame.encode_done_time.saturating_sub(frame.capture_time);
        }
        transport.send_video(frame);
    }
    // The pipeline is gone: dropped on purpose (the generation is then stale)
    // or stopped on an error.
    tokio::time::sleep(VIDEO_RESTART_DELAY).await;
    let _ = stopped.send(generation).await;
}

async fn forward_audio(mut packets: mpsc::Receiver<ws_proto::AudioPacket>, transport: Transport) {
    while let Some(packet) = packets.recv().await {
        transport.send_audio(packet);
    }
}

/// Joins whatever threads `value` owns without stalling other tasks on this
/// worker.
fn drop_blocking<T>(value: T) {
    block_in_place(|| drop(value));
}

/// The configured codec if the browser accepted it, else H.264, else
/// anything it accepted. `None` (not known yet) trusts the configuration.
fn pick_codec(wanted: VideoCodec, accepted: Option<&[VideoCodec]>) -> Option<VideoCodec> {
    let Some(accepted) = accepted else {
        return Some(wanted);
    };
    [wanted, VideoCodec::H264]
        .into_iter()
        .find(|c| accepted.contains(c))
        .or_else(|| accepted.first().copied())
}

fn pick_display(displays: &[DisplayInfo], wanted: Option<DisplayId>) -> Option<&DisplayInfo> {
    wanted
        .and_then(|id| displays.iter().find(|d| d.id == id))
        .or_else(|| displays.iter().find(|d| d.primary))
        .or_else(|| displays.first())
}

fn rect(d: &DisplayInfo) -> DisplayRect {
    DisplayRect {
        left: d.x,
        top: d.y,
        width: d.width as i32,
        height: d.height as i32,
    }
}

/// Whether moving the encoder from `current` to `target` kbps is worth a
/// reconfigure.
fn worth_changing(current: u32, target: u32) -> bool {
    let step = if target < current {
        BITRATE_STEP_DOWN
    } else {
        BITRATE_STEP_UP
    };
    f64::from(target.abs_diff(current)) >= f64::from(current) * step
}

/// Encoder bitrate for a bandwidth estimate, capped by the configured rate.
fn target_kbps(bwe_bps: u64, max_kbps: u32) -> u32 {
    let kbps = (bwe_bps as f64 * BWE_HEADROOM / 1000.0) as u64;
    (kbps.min(u64::from(max_kbps)) as u32).max(MIN_KBPS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(id: u32, primary: bool) -> DisplayInfo {
        DisplayInfo {
            id: DisplayId(id),
            name: format!("D{id}"),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            refresh_mhz: 60_000,
            primary,
        }
    }

    #[test]
    fn picks_chosen_then_primary_then_first() {
        let ds = [display(0, false), display(1, true), display(2, false)];
        assert_eq!(
            pick_display(&ds, Some(DisplayId(2))).unwrap().id,
            DisplayId(2)
        );
        assert_eq!(
            pick_display(&ds, Some(DisplayId(9))).unwrap().id,
            DisplayId(1)
        );
        assert_eq!(pick_display(&ds, None).unwrap().id, DisplayId(1));
        assert_eq!(pick_display(&ds[..1], None).unwrap().id, DisplayId(0));
        assert!(pick_display(&[], None).is_none());
    }

    #[test]
    fn codec_falls_back_to_h264_then_anything() {
        use VideoCodec::*;
        assert_eq!(pick_codec(Av1, None), Some(Av1));
        assert_eq!(pick_codec(Av1, Some(&[H264, Av1])), Some(Av1));
        assert_eq!(pick_codec(H265, Some(&[Av1, H264])), Some(H264));
        assert_eq!(pick_codec(H265, Some(&[Av1])), Some(Av1));
        assert_eq!(pick_codec(H264, Some(&[])), None);
    }

    #[test]
    fn bitrate_follows_estimate_under_cap() {
        assert_eq!(target_kbps(10_000_000, 20_000), 7_500);
        assert_eq!(target_kbps(100_000_000, 20_000), 20_000);
        assert_eq!(target_kbps(100_000, 20_000), MIN_KBPS);
    }

    #[test]
    fn bitrate_follows_down_sooner_than_up() {
        assert!(!worth_changing(10_000, 10_500));
        assert!(worth_changing(10_000, 11_000));
        assert!(!worth_changing(10_000, 9_900));
        assert!(worth_changing(10_000, 9_800));
    }
}
