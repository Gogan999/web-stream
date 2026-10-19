//! WebRTC transport between the host and the browser.
//!
//! Owns the peer connection, the video/audio tracks fed from
//! [`ws_proto::EncodedFrame`] and [`ws_proto::AudioPacket`], and the data
//! channels in [`ws_proto::channels`]. Signaling travels separately (see
//! `ws-signaling`): the host calls [`Transport::offer`], sends the SDP to the
//! browser, and hands the answer back with [`Transport::accept_answer`].
//!
//! Late frames are dropped, never queued: if the send side is behind (a
//! newer frame arrived before the last was sent, or the pacer queue is older
//! than [`TransportConfig::max_send_delay`]) it skips to the newest frame and
//! raises [`TransportEvent::FramesLost`], so the encoder sends a frame that
//! builds only on what the browser has (or a keyframe). Until that frame goes
//! out, frames that build on the skipped one are dropped too, so the browser
//! never decodes against a picture it doesn't have. NACKs are answered for packets
//! younger than [`TransportConfig::max_resend_age`]: a late resend costs the
//! browser a short wait, a keyframe costs a stall and a soft picture.
//!
//! The host is ICE-lite and reachable directly: one UDP port (and optionally
//! one ICE-TCP port for networks that block UDP) on each LAN address, plus any
//! public addresses that forward to them.

mod net;
mod session;
mod timer;

pub use net::lan_ips;

use std::collections::{HashSet, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use str0m::net::Protocol;
use tokio::sync::{Notify, mpsc, oneshot};
use ws_diag::diag;
use ws_proto::{AudioPacket, EncodedFrame, channels, mono_now};

use crate::net::{Net, Packet, SharedFilter};
use crate::session::{Session, Step, VideoWrite};

/// Frames the encoder may get ahead of the send loop (about 50 ms at 60 fps)
/// before the oldest is dropped. The late-frame drop still bounds latency.
const VIDEO_QUEUE: usize = 3;
/// Events waiting for the host app. If it falls this far behind, events are
/// dropped rather than stalling media.
const EVENT_QUEUE: usize = 1024;
const CMD_QUEUE: usize = 64;
/// A keyframe request is repeated only if no keyframe went out this long
/// after the last one.
const KEYFRAME_RETRY: Duration = Duration::from_millis(300);
/// How long after a host-side drop a frame that skips the gap must go out
/// before a keyframe is asked for instead.
const RECOVERY_WAIT: Duration = Duration::from_millis(150);

#[derive(Debug, Clone)]
pub struct TransportConfig {
    /// Local addresses to listen on. Empty means every LAN IPv4 address.
    pub ips: Vec<IpAddr>,
    /// Public addresses that port-forward to the first local address, for
    /// access from the internet.
    pub public_ips: Vec<IpAddr>,
    /// UDP port for all media and data. 0 picks a free port.
    pub udp_port: u16,
    /// ICE-TCP port, or `None` to disable the TCP fallback.
    pub tcp_port: Option<u16>,
    /// Don't resend packets older than this; a keyframe repairs the loss
    /// instead. Long enough for a lost resend to be asked for again: the
    /// browser repeats a NACK about once a round trip, and until it knows the
    /// round trip (a receive-only peer rarely does) it assumes 100 ms.
    pub max_resend_age: Duration,
    /// Drop a (non-key) frame instead of queueing it when the oldest unsent
    /// video packet has waited longer than this. A drop costs a keyframe (a
    /// stall and a soft picture), so this only catches a queue that has truly
    /// fallen behind: a burst can sit a few milliseconds past two frame times,
    /// on Windows especially, and is better sent a little late.
    pub max_send_delay: Duration,
    /// Starting point for bandwidth estimation.
    pub initial_bitrate_bps: u64,
    /// The most the encoder will use; bandwidth estimation probes up to it.
    pub max_bitrate_bps: u64,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            ips: Vec::new(),
            public_ips: Vec::new(),
            udp_port: 8443,
            tcp_port: Some(8444),
            max_resend_age: Duration::from_millis(250),
            max_send_delay: Duration::from_millis(50),
            initial_bitrate_bps: 10_000_000,
            max_bitrate_bps: 50_000_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum TransportEvent {
    /// ICE and DTLS are up; media can flow. Also asks for a keyframe.
    Connected,
    Disconnected,
    /// The browser lost a frame (PLI/FIR) or the transport dropped one:
    /// send an IDR or intra refresh now. Not repeated until a keyframe has
    /// been sent (or a short retry interval passes).
    KeyframeNeeded,
    /// The transport dropped frames from this `frame_index` on (sent too
    /// late, or built on a dropped frame); the browser never saw them. The
    /// next frame must build only on earlier ones
    /// ([`ws_proto::EncodedFrame::recovers_from`]), or be a keyframe.
    /// Raised again if that frame is dropped too; if none has gone out a
    /// short while later, [`TransportEvent::KeyframeNeeded`] follows.
    FramesLost(u64),
    /// Bandwidth estimate, in bits per second; drive the encoder bitrate.
    TargetBitrate(u64),
    ChannelOpen(&'static str),
    ChannelClosed(&'static str),
    /// The channel's send buffer drained; more data can be written.
    ChannelLow(&'static str),
    /// A data channel message, except on the input channels: those go to
    /// the [`InputSink`] (see [`Transport::set_input_sink`]).
    Data {
        channel: &'static str,
        binary: bool,
        data: Vec<u8>,
    },
    /// One Opus packet from the browser microphone.
    MicAudio(AudioPacket),
    Stats(TransportStats),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TransportStats {
    pub rtt: Option<Duration>,
    pub egress_loss: Option<f32>,
    pub bwe: Option<u64>,
    pub bytes_tx: u64,
    /// What went wrong since the last stats.
    pub trouble: Trouble,
}

/// Counts, since the last [`TransportStats`], of what costs the browser a
/// keyframe or a packet: to tell a bad network from a host that can't keep up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Trouble {
    /// Keyframe requests from the browser (PLI/FIR): it lost a packet that
    /// no resend repaired.
    pub browser_keyframe_requests: u32,
    /// Frames dropped because the send queue was too far behind.
    pub late_frames: u32,
    /// Frames dropped for another reason (the encoder got too far ahead, or
    /// they built on a dropped frame).
    pub lost_frames: u32,
    /// Packets the host's socket refused (its buffer was full).
    pub send_drops: u32,
    /// Dropped frames the encoder skipped over without a keyframe.
    pub recovered: u32,
}

/// Takes every message on the `input` and `input-motion` channels, called
/// straight from the transport's run loop (see [`Transport::set_input_sink`]).
/// Must return at once: it holds up all media while it runs.
pub type InputSink = Box<dyn Fn(&[u8]) + Send + Sync>;

/// Input messages kept while no sink is set (the app is still starting its
/// input side). Past this the oldest go, and the sink is first told to
/// release everything, since a dropped key-up would leave a key held.
const INPUT_PENDING: usize = 1024;

/// Where input channel messages go.
#[derive(Default)]
struct InputRoute {
    sink: Option<InputSink>,
    /// In arrival order, waiting for a sink.
    pending: VecDeque<Vec<u8>>,
    /// Some of `pending` was dropped.
    overflowed: bool,
    /// Connected and Disconnected events delivered so far. A sink is only
    /// taken from an app that has handled all of them, so it belongs to the
    /// current connection.
    transitions: u64,
}

impl InputRoute {
    fn input(&mut self, data: Vec<u8>) {
        match &self.sink {
            Some(sink) => sink(&data),
            None => {
                if self.pending.len() == INPUT_PENDING {
                    self.pending.pop_front();
                    self.overflowed = true;
                }
                self.pending.push_back(data);
            }
        }
    }

    /// The connection came up or went down: what was set up for the last
    /// state no longer applies.
    fn reset(&mut self) {
        self.sink = None;
        self.pending.clear();
        self.overflowed = false;
    }
}

/// [`Trouble`], counted as it happens.
#[derive(Default)]
struct TroubleCounts {
    browser_keyframe_requests: AtomicU32,
    late_frames: AtomicU32,
    lost_frames: AtomicU32,
    send_drops: AtomicU32,
    recovered: AtomicU32,
}

impl TroubleCounts {
    fn take(&self) -> Trouble {
        let take = |c: &AtomicU32| c.swap(0, Ordering::Relaxed);
        Trouble {
            browser_keyframe_requests: take(&self.browser_keyframe_requests),
            late_frames: take(&self.late_frames),
            lost_frames: take(&self.lost_frames),
            send_drops: take(&self.send_drops),
            recovered: take(&self.recovered),
        }
    }
}

fn bump(c: &AtomicU32) {
    c.fetch_add(1, Ordering::Relaxed);
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("negotiation: {0}")]
    Negotiation(String),
    #[error("no session")]
    NoSession,
    #[error("unknown data channel {0}")]
    UnknownChannel(String),
    #[error("data channel {0} is not open")]
    ChannelNotOpen(String),
    #[error("transport stopped")]
    Stopped,
}

enum Cmd {
    Offer(Vec<IpAddr>, oneshot::Sender<Result<String, Error>>),
    Answer(String, oneshot::Sender<Result<(), Error>>),
    Candidate(String),
    Close,
    MaxBitrate(u64),
    Data {
        channel: String,
        binary: bool,
        data: Vec<u8>,
        reply: oneshot::Sender<Result<bool, Error>>,
    },
}

/// State shared by the handle and the run loop.
struct Shared {
    /// Frames from the encoder not yet taken by the send loop (at most
    /// [`VIDEO_QUEUE`]).
    frames: Mutex<VecDeque<EncodedFrame>>,
    notify: Notify,
    connected: AtomicBool,
    /// When the outstanding keyframe request was made.
    keyframe_asked: Mutex<Option<Instant>>,
    /// The first frame index the browser is missing after a drop, and when
    /// it went missing, until a frame skipping the gap or a keyframe goes
    /// out.
    recovering: Mutex<Option<(u64, Instant)>>,
    trouble: TroubleCounts,
    events: mpsc::Sender<TransportEvent>,
    input: Mutex<InputRoute>,
}

impl Shared {
    fn emit(&self, ev: TransportEvent) {
        let transition = matches!(ev, TransportEvent::Connected | TransportEvent::Disconnected);
        // Held across the send, so a sink set meanwhile is checked against
        // the count this event makes.
        let mut route = transition.then(|| self.input_route());
        match self.events.try_send(ev) {
            Ok(()) => {
                if let Some(route) = &mut route {
                    route.transitions += 1;
                    route.reset();
                }
            }
            Err(mpsc::error::TrySendError::Full(ev)) => {
                diag!("event_queue_full", "dropped={ev:?}");
                tracing::warn!(?ev, "transport event queue full; dropping event");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }

    fn input_route(&self) -> std::sync::MutexGuard<'_, InputRoute> {
        // A sink that panicked leaves nothing half-done in the route.
        self.input.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn keyframe_needed(&self, cause: &str) {
        let now = Instant::now();
        let mut asked = self.keyframe_asked.lock().expect("keyframe lock");
        if asked.is_some_and(|t| now.duration_since(t) < KEYFRAME_RETRY) {
            diag!("keyframe_needed", "cause={cause} suppressed=true");
            return;
        }
        *asked = Some(now);
        drop(asked);
        diag!("keyframe_needed", "cause={cause} suppressed=false");
        self.emit(TransportEvent::KeyframeNeeded);
    }

    fn keyframe_sent(&self) {
        *self.keyframe_asked.lock().expect("keyframe lock") = None;
        self.recovered();
    }

    /// `dropped` did not go out, and the browser has every frame before
    /// `next`. Asks the encoder to skip the gap, or for a keyframe once
    /// that has taken too long (or before anything was sent).
    fn frame_dropped(&self, next: Option<u64>, dropped: &EncodedFrame) {
        let Some(from) = next else {
            return self.keyframe_needed("drop_before_first_keyframe");
        };
        let now = Instant::now();
        let mut recovering = self.recovering.lock().expect("recovery lock");
        match *recovering {
            Some((f, since)) if f == from => {
                if now.duration_since(since) >= RECOVERY_WAIT {
                    drop(recovering);
                    self.keyframe_needed("repair_timed_out");
                } else if dropped.recovers_from.is_some() {
                    // The repair itself was dropped: skip it too.
                    drop(recovering);
                    diag!("frames_lost", "from={from} why=repair_dropped");
                    self.emit(TransportEvent::FramesLost(from));
                }
            }
            _ => {
                *recovering = Some((from, now));
                drop(recovering);
                diag!(
                    "frames_lost",
                    "from={from} why=first_drop idx={}",
                    dropped.frame_index
                );
                self.emit(TransportEvent::FramesLost(from));
            }
        }
    }

    /// A frame skipping the gap was discarded before it reached the send
    /// loop: ask for another, since the frames after it build on it.
    fn repair_lost(&self) {
        let recovering = *self.recovering.lock().expect("recovery lock");
        if let Some((from, _)) = recovering {
            diag!("frames_lost", "from={from} why=repair_lost_in_queue");
            self.emit(TransportEvent::FramesLost(from));
        }
    }

    /// A frame went out after a gap without a keyframe, or a keyframe did.
    fn recovered(&self) {
        self.recovering.lock().expect("recovery lock").take();
    }

    /// The keyframe answering a request never went out: ask again now
    /// rather than after [`KEYFRAME_RETRY`], since every frame until the
    /// next keyframe is held back.
    fn keyframe_lost(&self) {
        self.keyframe_sent();
        self.keyframe_needed("keyframe_lost");
    }
}

/// Handle to the transport task. Cheap to clone.
#[derive(Clone)]
pub struct Transport {
    cmds: mpsc::Sender<Cmd>,
    shared: Arc<Shared>,
    audio: mpsc::Sender<AudioPacket>,
}

impl Transport {
    /// Bind the sockets and start the transport task. Events arrive on the
    /// returned receiver.
    pub async fn start(
        config: TransportConfig,
    ) -> Result<(Transport, mpsc::Receiver<TransportEvent>), Error> {
        let ips = if config.ips.is_empty() {
            net::lan_ips()
        } else {
            config.ips.clone()
        };
        if ips.is_empty() {
            return Err(Error::Io(std::io::Error::other(
                "no local IPv4 address to listen on",
            )));
        }
        let (pkt_tx, pkt_rx) = mpsc::channel(1024);
        let filter = SharedFilter::default();
        let net = Net::bind(
            &ips,
            config.udp_port,
            config.tcp_port,
            filter.clone(),
            pkt_tx,
        )
        .await?;
        tracing::info!(udp = ?net.udp_addrs, tcp = ?net.tcp_addrs, "transport listening");

        let (cmds, cmd_rx) = mpsc::channel(CMD_QUEUE);
        let (events_tx, events) = mpsc::channel(EVENT_QUEUE);
        // About 80 ms of 10 ms Opus packets; beyond that, old audio is useless.
        let (audio, audio_rx) = mpsc::channel(8);
        let shared = Arc::new(Shared {
            frames: Mutex::new(VecDeque::new()),
            notify: Notify::new(),
            connected: AtomicBool::new(false),
            keyframe_asked: Mutex::new(None),
            recovering: Mutex::new(None),
            trouble: TroubleCounts::default(),
            events: events_tx,
            input: Mutex::default(),
        });
        let driver = Driver {
            config,
            net,
            filter,
            confirmed: HashSet::new(),
            session: None,
            shared: shared.clone(),
            steps: Vec::new(),
        };
        tokio::spawn(driver.run(cmd_rx, pkt_rx, audio_rx));
        Ok((
            Transport {
                cmds,
                shared,
                audio,
            },
            events,
        ))
    }

    /// Start a new peer connection (replacing any current one) and return
    /// the SDP offer for the browser.
    pub async fn offer(&self) -> Result<String, Error> {
        self.offer_reachable_at(Vec::new()).await
    }

    /// Like [`Transport::offer`], also advertising `public`: addresses this
    /// browser reached the host at (say, the WAN address or DDNS name in its
    /// URL), which forward the media ports here. Without one of these (or
    /// [`TransportConfig::public_ips`]) a browser outside the LAN only sees
    /// LAN candidates and can never connect.
    pub async fn offer_reachable_at(&self, public: Vec<IpAddr>) -> Result<String, Error> {
        let (tx, rx) = oneshot::channel();
        self.cmds
            .send(Cmd::Offer(public, tx))
            .await
            .map_err(|_| Error::Stopped)?;
        rx.await.map_err(|_| Error::Stopped)?
    }

    pub async fn accept_answer(&self, sdp: String) -> Result<(), Error> {
        let (tx, rx) = oneshot::channel();
        self.cmds
            .send(Cmd::Answer(sdp, tx))
            .await
            .map_err(|_| Error::Stopped)?;
        rx.await.map_err(|_| Error::Stopped)?
    }

    /// A trickled browser candidate (optional for an ICE-lite host).
    pub fn add_remote_candidate(&self, candidate: String) {
        if self.cmds.try_send(Cmd::Candidate(candidate)).is_err() {
            tracing::debug!("transport busy; dropping remote candidate");
        }
    }

    /// End the current peer connection.
    pub fn close(&self) {
        if let Err(mpsc::error::TrySendError::Full(cmd)) = self.cmds.try_send(Cmd::Close) {
            let cmds = self.cmds.clone();
            tokio::spawn(async move {
                let _ = cmds.send(cmd).await;
            });
        }
    }

    /// Hand over the next encoded frame. Frames the send loop has not taken
    /// yet stay queued, so two arriving back to back (the encoder catching up
    /// after a stall) both go out. A keyframe supersedes the queue. Past
    /// [`VIDEO_QUEUE`] frames the oldest is discarded; the frame after it
    /// then can't go out, which raises [`TransportEvent::FramesLost`].
    pub fn send_video(&self, frame: EncodedFrame) {
        let keyframe = frame.keyframe;
        let dropped = {
            let mut frames = self.shared.frames.lock().expect("video queue lock");
            if keyframe {
                frames.clear();
            }
            frames.push_back(frame);
            if frames.len() > VIDEO_QUEUE {
                frames.pop_front()
            } else {
                None
            }
        };
        if let Some(old) = dropped {
            diag!(
                "video_queue_drop",
                "idx={} keyframe={} repair={}",
                old.frame_index,
                old.keyframe,
                old.recovers_from.is_some()
            );
            tracing::debug!(index = old.frame_index, "video queue full; frame dropped");
            if self.shared.connected.load(Ordering::Relaxed) {
                bump(&self.shared.trouble.lost_frames);
                if old.keyframe {
                    self.shared.keyframe_lost();
                } else if old.recovers_from.is_some() {
                    self.shared.repair_lost();
                }
            }
        }
        self.shared.notify.notify_one();
    }

    /// Hand input channel messages to `sink` as they arrive, starting with
    /// any that came in before it was set. Input then never waits behind
    /// other events, and a press is never dropped because the event queue
    /// is full. `transitions` is how many [`TransportEvent::Connected`] and
    /// [`TransportEvent::Disconnected`] events the caller has handled: if
    /// another has been sent since, the sink belongs to a connection that is
    /// already gone, and it is refused (false). Every such event clears the
    /// sink, so the app sets it again after each Connected.
    pub fn set_input_sink(&self, sink: InputSink, transitions: u64) -> bool {
        let mut route = self.shared.input_route();
        if route.transitions != transitions {
            return false;
        }
        if std::mem::take(&mut route.overflowed) {
            sink(&ws_proto::InputEvent::ReleaseAll.encode());
        }
        for data in std::mem::take(&mut route.pending) {
            sink(&data);
        }
        route.sink = Some(sink);
        true
    }

    /// Stop handing input to the sink; it waits for the next one.
    pub fn clear_input_sink(&self) {
        self.shared.input_route().sink = None;
    }

    /// Change the most the bandwidth estimate probes up to (see
    /// [`TransportConfig::max_bitrate_bps`]), for this and later sessions.
    pub fn set_max_bitrate(&self, bps: u64) {
        if self.cmds.try_send(Cmd::MaxBitrate(bps)).is_err() {
            tracing::debug!("transport busy; dropping bitrate ceiling change");
        }
    }

    /// Queue one Opus packet of game audio. Dropped if the queue is full.
    pub fn send_audio(&self, packet: AudioPacket) {
        let _ = self.audio.try_send(packet);
    }

    /// Write to a data channel by label (see [`ws_proto::channels`]).
    /// `Ok(false)` means its buffer is full: wait for
    /// [`TransportEvent::ChannelLow`] and retry.
    pub async fn send_data(
        &self,
        channel: &str,
        binary: bool,
        data: Vec<u8>,
    ) -> Result<bool, Error> {
        let (reply, rx) = oneshot::channel();
        self.cmds
            .send(Cmd::Data {
                channel: channel.to_string(),
                binary,
                data,
                reply,
            })
            .await
            .map_err(|_| Error::Stopped)?;
        rx.await.map_err(|_| Error::Stopped)?
    }
}

struct Driver {
    config: TransportConfig,
    net: Net,
    filter: SharedFilter,
    /// UDP destinations already reported to `filter` as confirmed.
    confirmed: HashSet<SocketAddr>,
    session: Option<Session>,
    shared: Arc<Shared>,
    steps: Vec<Step>,
}

impl Driver {
    async fn run(
        mut self,
        mut cmds: mpsc::Receiver<Cmd>,
        mut packets: mpsc::Receiver<Packet>,
        mut audio: mpsc::Receiver<AudioPacket>,
    ) {
        let shared = self.shared.clone();
        let _timer = timer::FineTimer::new();
        loop {
            let deadline = self.drain();
            // Unbiased: no source (a flood on the sockets, say) can starve
            // the others.
            tokio::select! {
                cmd = cmds.recv() => match cmd {
                    Some(cmd) => self.command(cmd),
                    None => return,
                },
                pkt = packets.recv() => {
                    let Some(pkt) = pkt else { return };
                    if let Some(s) = &mut self.session {
                        if s.accepts(pkt.proto, pkt.source, pkt.destination, &pkt.data) {
                            s.receive(Instant::now(), pkt.proto, pkt.source, pkt.destination, &pkt.data);
                        }
                    }
                }
                _ = shared.notify.notified() => {
                    let frames = std::mem::take(&mut *shared.frames.lock().expect("video queue lock"));
                    let batch = frames.len();
                    for frame in frames {
                        let Some(s) = &mut self.session else { break };
                        let next = s.next_index();
                        let now = Instant::now();
                        let queue = ws_diag::enabled().then(|| s.queue_diag(now));
                        let result = s.write_video(&frame, now);
                        if let Some((age, bytes, packets)) = queue {
                            // since_enc: encode done -> here, the hop through
                            // the host's tasks and this loop.
                            diag!(
                                "send",
                                "idx={} key={} repair={} bytes={} result={result:?} since_enc_ms={:.2} batch={batch} pacer_age_ms={:.2} pacer_bytes={bytes} pacer_pkts={packets}",
                                frame.frame_index,
                                frame.keyframe,
                                frame.recovers_from.is_some(),
                                frame.data.len(),
                                mono_now().saturating_sub(frame.encode_done_time) as f64 / 1000.0,
                                age.as_secs_f64() * 1000.0,
                            );
                        }
                        match result {
                            VideoWrite::Sent if frame.keyframe => shared.keyframe_sent(),
                            VideoWrite::Sent => {
                                if next != Some(frame.frame_index) {
                                    bump(&shared.trouble.recovered);
                                    shared.recovered();
                                }
                            }
                            VideoWrite::Skipped if frame.keyframe && s.is_connected() => {
                                bump(&shared.trouble.lost_frames);
                                shared.keyframe_lost()
                            }
                            VideoWrite::Late => {
                                bump(&shared.trouble.late_frames);
                                shared.frame_dropped(next, &frame)
                            }
                            VideoWrite::Broken => {
                                bump(&shared.trouble.lost_frames);
                                shared.frame_dropped(next, &frame)
                            }
                            VideoWrite::Skipped => {}
                        }
                    }
                }
                Some(pkt) = audio.recv() => {
                    if let Some(s) = &mut self.session {
                        s.write_audio(&pkt, Instant::now());
                    }
                }
                _ = tokio::time::sleep_until(deadline.into()) => {
                    let now = Instant::now();
                    // A timer that fires late holds up the pacer: the tokio
                    // pool was busy, or the timer is coarse.
                    let late = now.saturating_duration_since(deadline);
                    if late > Duration::from_millis(4) {
                        diag!("loop_timer_late", "ms={:.2}", late.as_secs_f64() * 1000.0);
                    }
                    if let Some(s) = &mut self.session {
                        s.timeout(now);
                    }
                }
            }
        }
    }

    /// Flush the session's output; returns when it next needs a timeout.
    fn drain(&mut self) -> Instant {
        let Some(s) = &mut self.session else {
            return Instant::now() + Duration::from_secs(3600);
        };
        let deadline = s.poll(&mut self.steps);
        let alive = s.is_alive();
        for step in self.steps.drain(..) {
            match step {
                Step::Transmit {
                    proto,
                    source,
                    destination,
                    data,
                } => {
                    if proto == Protocol::Udp && self.confirmed.insert(destination) {
                        self.filter
                            .lock()
                            .expect("filter lock")
                            .confirm(destination);
                    }
                    if !self.net.send(proto, source, destination, &data) {
                        diag!(
                            "send_drop",
                            "proto={proto:?} dst={destination} bytes={}",
                            data.len()
                        );
                        bump(&self.shared.trouble.send_drops);
                    }
                }
                Step::Event(ev) => match ev {
                    TransportEvent::Connected => {
                        self.shared.connected.store(true, Ordering::Relaxed);
                        self.shared.emit(TransportEvent::Connected);
                        // The first frame the browser sees must be decodable.
                        self.shared.keyframe_sent();
                        self.shared.keyframe_needed("connected");
                    }
                    TransportEvent::KeyframeNeeded => {
                        bump(&self.shared.trouble.browser_keyframe_requests);
                        self.shared.keyframe_needed("browser")
                    }
                    TransportEvent::Stats(mut stats) => {
                        stats.trouble = self.shared.trouble.take();
                        diag!(
                            "transport_stats",
                            "rtt_ms={:?} loss={:?} bwe={:?} bytes_tx={} trouble={:?}",
                            stats.rtt.map(|r| r.as_millis()),
                            stats.egress_loss,
                            stats.bwe,
                            stats.bytes_tx,
                            stats.trouble
                        );
                        self.shared.emit(TransportEvent::Stats(stats))
                    }
                    TransportEvent::Data { channel, data, .. }
                        if channel == channels::INPUT || channel == channels::INPUT_MOTION =>
                    {
                        self.shared.input_route().input(data)
                    }
                    ev => self.shared.emit(ev),
                },
            }
        }
        if !alive {
            self.end_session();
        } else if let Some(s) = &self.session {
            self.shared
                .connected
                .store(s.is_connected(), Ordering::Relaxed);
        }
        deadline
    }

    /// Host candidates for the bound sockets, plus server-reflexive ones for
    /// the configured public addresses and `extra` (IPv4 only, as the
    /// sockets are; local addresses are already host candidates).
    fn candidates(&self, extra: &[IpAddr]) -> Vec<str0m::Candidate> {
        let mut public = self.config.public_ips.clone();
        for ip in extra {
            let local = self.net.udp_addrs.iter().any(|a| a.ip() == *ip);
            if ip.is_ipv4()
                && !ip.is_loopback()
                && !ip.is_unspecified()
                && !local
                && !public.contains(ip)
            {
                public.push(*ip);
            }
        }
        session::candidates(&self.net.udp_addrs, &self.net.tcp_addrs, &public)
    }

    /// Drop the session, reporting Disconnected unless already reported.
    fn end_session(&mut self) {
        if let Some(mut s) = self.session.take() {
            if s.mark_down() {
                self.shared.emit(TransportEvent::Disconnected);
            }
        }
        self.shared.connected.store(false, Ordering::Relaxed);
        self.shared.input_route().reset();
        self.filter.lock().expect("filter lock").reset(None);
        self.confirmed.clear();
    }

    fn command(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Offer(extra, reply) => {
                self.end_session();
                let candidates = self.candidates(&extra);
                let result = Session::offer(&self.config, &candidates, Instant::now()).map(
                    |(mut s, sdp)| {
                        let ufrag = s.local_ufrag();
                        self.filter.lock().expect("filter lock").reset(Some(&ufrag));
                        self.session = Some(s);
                        sdp
                    },
                );
                let _ = reply.send(result);
            }
            Cmd::Answer(sdp, reply) => {
                let result = match &mut self.session {
                    Some(s) => s.accept_answer(&sdp),
                    None => Err(Error::NoSession),
                };
                let _ = reply.send(result);
            }
            Cmd::Candidate(c) => {
                if let Some(s) = &mut self.session {
                    s.add_remote_candidate(&c);
                }
            }
            Cmd::Close => self.end_session(),
            Cmd::MaxBitrate(bps) => {
                self.config.max_bitrate_bps = bps;
                if let Some(s) = &mut self.session {
                    s.set_max_bitrate(bps);
                }
            }
            Cmd::Data {
                channel,
                binary,
                data,
                reply,
            } => {
                let result = match &mut self.session {
                    Some(s) => s.write_data(&channel, binary, &data),
                    None => Err(Error::NoSession),
                };
                let _ = reply.send(result);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn input_waits_for_the_current_connections_sink_in_order() {
        let (transport, mut events) = Transport::start(TransportConfig {
            ips: vec![[127, 0, 0, 1].into()],
            udp_port: 0,
            tcp_port: None,
            ..Default::default()
        })
        .await
        .unwrap();
        let shared = &transport.shared;
        let got = Arc::new(Mutex::new(Vec::new()));
        let sink = || -> InputSink {
            let got = got.clone();
            Box::new(move |d: &[u8]| got.lock().unwrap().push(d[0]))
        };

        shared.emit(TransportEvent::Connected);
        shared.input_route().input(vec![1]);
        shared.input_route().input(vec![2]);
        // The app hasn't seen Connected yet: its sink is for no connection.
        assert!(!transport.set_input_sink(sink(), 0));
        assert!(transport.set_input_sink(sink(), 1));
        shared.input_route().input(vec![3]);
        assert_eq!(*got.lock().unwrap(), [1, 2, 3]);

        // Going down drops the sink; input waits for the next one.
        shared.emit(TransportEvent::Disconnected);
        shared.input_route().input(vec![4]);
        assert_eq!(*got.lock().unwrap(), [1, 2, 3]);
        shared.emit(TransportEvent::Connected);
        assert!(transport.set_input_sink(sink(), 3));
        // What waited from the last state was dropped with it.
        assert_eq!(*got.lock().unwrap(), [1, 2, 3]);
        assert!(matches!(
            events.recv().await,
            Some(TransportEvent::Connected)
        ));

        // A long wait keeps the newest.
        transport.clear_input_sink();
        for i in 0..INPUT_PENDING + 5 {
            shared.input_route().input(vec![(i % 256) as u8]);
        }
        assert_eq!(shared.input_route().pending.len(), INPUT_PENDING);
        got.lock().unwrap().clear();
        assert!(transport.set_input_sink(sink(), 3));
        let got = got.lock().unwrap();
        // Release everything first: a dropped release would leave a key held.
        assert_eq!(got[0], ws_proto::InputEvent::ReleaseAll.encode()[0]);
        assert_eq!(got[1..3], [5, 6]);
        assert_eq!(got.len(), INPUT_PENDING + 1);
    }
}
