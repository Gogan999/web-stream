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
mod remote;
mod session;
mod timer;

pub use net::lan_ips;
pub use remote::{Remote, RemoteCmd};

use std::collections::{HashSet, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use str0m::net::Protocol;
use tokio::sync::{Notify, mpsc, oneshot};
use ws_diag::diag;
use ws_proto::{AudioPacket, EncodedFrame, SendBacklog, channels, mono_now};

use crate::net::{Net, Packet, SharedFilter};
use crate::session::{Session, Step, VideoWrite};

/// Frames the encoder may get ahead of the send loop (about 50 ms at 60 fps)
/// before the oldest is dropped. The late-frame drop still bounds latency.
const VIDEO_QUEUE: usize = 3;
/// Data channel messages and mic packets waiting for the host app. If it
/// falls this far behind, more are dropped rather than queued without
/// bound. Connection, channel and bitrate events are never dropped.
const DATA_QUEUE: usize = 1024;
const CMD_QUEUE: usize = 64;
/// A keyframe request is repeated only if no keyframe went out this long
/// after the last one.
const KEYFRAME_RETRY: Duration = Duration::from_millis(300);
/// How long after a host-side drop a frame that skips the gap must go out
/// before a keyframe is asked for instead.
const RECOVERY_WAIT: Duration = Duration::from_millis(150);
/// A new peer connection this soon after the last estimate (a restart after
/// a network blip, say) starts from that estimate rather than from
/// [`TransportConfig::initial_bitrate_bps`], so the stream doesn't spend
/// seconds climbing back. Unless [`Transport::forget_estimate`] said the
/// next one is for another device, whose path the estimate says nothing
/// about.
pub const ESTIMATE_KEPT_FOR: Duration = Duration::from_secs(60);
/// It starts a little under it: the path may have changed.
pub const KEPT_ESTIMATE_SHARE: f64 = 0.8;

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
    /// been sent (or a short retry interval passes). Goes to the
    /// [`VideoSink`] instead while one is set.
    KeyframeNeeded,
    /// The transport dropped frames from this `frame_index` on (sent too
    /// late, or built on a dropped frame); the browser never saw them. The
    /// next frame must build only on earlier ones
    /// ([`ws_proto::EncodedFrame::recovers_from`]), or be a keyframe.
    /// Raised again if that frame is dropped too; if none has gone out a
    /// short while later, [`TransportEvent::KeyframeNeeded`] follows. Goes
    /// to the [`VideoSink`] instead while one is set.
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

/// What the encoder must do so the browser can keep decoding: the
/// [`TransportEvent::KeyframeNeeded`] and [`TransportEvent::FramesLost`]
/// events, delivered straight from the run loop (see
/// [`Transport::set_video_sink`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoSignal {
    Keyframe,
    FramesLost(u64),
}

/// Takes [`VideoSignal`]s, called straight from the transport's run loop.
/// Must return at once (hand the signal to the encoder's thread).
pub type VideoSink = Box<dyn Fn(VideoSignal) + Send + Sync>;

/// The transport's events for the host app, in order. Connection, channel
/// and bitrate events are never dropped; data channel messages and mic
/// packets are once [`DATA_QUEUE`] of them are waiting.
pub struct Events {
    rx: mpsc::UnboundedReceiver<TransportEvent>,
    queued_data: Arc<AtomicUsize>,
}

impl Events {
    pub async fn recv(&mut self) -> Option<TransportEvent> {
        let ev = self.rx.recv().await?;
        self.took(&ev);
        Some(ev)
    }

    pub fn try_recv(&mut self) -> Result<TransportEvent, mpsc::error::TryRecvError> {
        let ev = self.rx.try_recv()?;
        self.took(&ev);
        Ok(ev)
    }

    fn took(&self, ev: &TransportEvent) {
        if is_data(ev) {
            self.queued_data.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// Events that are many and bulky (uploads, mic audio): bounded, dropped
/// when the app is behind. Messages on `control` are few, and losing one
/// (a keyframe request, a size change) would go unnoticed, so they are
/// never dropped.
fn is_data(ev: &TransportEvent) -> bool {
    match ev {
        TransportEvent::Data { channel, .. } => *channel != channels::CONTROL,
        TransportEvent::MicAudio(_) => true,
        _ => false,
    }
}

/// A short name for an event, for logs (data events carry bulk payloads).
fn event_kind(ev: &TransportEvent) -> &'static str {
    match ev {
        TransportEvent::Data { channel, .. } => channel,
        TransportEvent::MicAudio(_) => "mic_audio",
        _ => "other",
    }
}

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
    /// End the run loop and the thread, whatever handles remain.
    Shutdown,
    MaxBitrate(u64),
    /// A video sink was set (true) or cleared. Only a [`Remote`] run loop
    /// needs telling: the real one reads the sink from [`Shared`].
    VideoSink(bool),
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
    events: mpsc::UnboundedSender<TransportEvent>,
    /// Data events sent and not yet taken by [`Events`].
    queued_data: Arc<AtomicUsize>,
    input: Mutex<InputRoute>,
    /// Where keyframe requests and repairs go, if not to `events`.
    video: Mutex<Option<VideoSink>>,
    /// Age of the oldest unsent video packet, for the encoder.
    backlog: SendBacklog,
    /// Woken when `backlog` changes, so a watcher need not poll it.
    backlog_changed: Notify,
    /// The next offer starts from the configured estimate.
    forget_estimate: AtomicBool,
    /// Fires once the transport thread has dropped its runtime, and with it
    /// every socket.
    exited: Mutex<Option<oneshot::Receiver<()>>>,
}

impl Shared {
    /// Store the backlog and wake the watcher if it moved. `notify_one`
    /// keeps a permit when nobody is waiting, so a change between the
    /// watcher's awaits still wakes its next one. Unchanged values (the
    /// usual case while idle or steady) wake nothing.
    fn set_backlog(&self, age: Duration) {
        if self.backlog.get() != age {
            self.backlog.set(age);
            self.backlog_changed.notify_one();
        }
    }

    fn emit(&self, ev: TransportEvent) {
        let transition = matches!(ev, TransportEvent::Connected | TransportEvent::Disconnected);
        // Held across the send, so a sink set meanwhile is checked against
        // the count this event makes.
        let mut route = transition.then(|| self.input_route());
        let data = is_data(&ev);
        if data && self.queued_data.fetch_add(1, Ordering::Relaxed) >= DATA_QUEUE {
            self.queued_data.fetch_sub(1, Ordering::Relaxed);
            diag!("event_queue_full", "dropped={}", event_kind(&ev));
            tracing::warn!("transport event queue full; dropping a data channel message");
            return;
        }
        match self.events.send(ev) {
            Ok(()) => {
                if let Some(route) = &mut route {
                    route.transitions += 1;
                    route.reset();
                }
            }
            Err(_) if data => {
                self.queued_data.fetch_sub(1, Ordering::Relaxed);
            }
            Err(_) => {}
        }
    }

    /// Hands a keyframe request or repair to the encoder: straight to the
    /// video sink if one is set, else as an event.
    fn signal_video(&self, signal: VideoSignal) {
        let video = self.video.lock().unwrap_or_else(|e| e.into_inner());
        match &*video {
            Some(sink) => sink(signal),
            None => {
                drop(video);
                self.emit(match signal {
                    VideoSignal::Keyframe => TransportEvent::KeyframeNeeded,
                    VideoSignal::FramesLost(from) => TransportEvent::FramesLost(from),
                });
            }
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
        self.signal_video(VideoSignal::Keyframe);
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
                    self.signal_video(VideoSignal::FramesLost(from));
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
                self.signal_video(VideoSignal::FramesLost(from));
            }
        }
    }

    /// A frame skipping the gap was discarded before it reached the send
    /// loop: ask for another, since the frames after it build on it.
    fn repair_lost(&self) {
        let recovering = *self.recovering.lock().expect("recovery lock");
        if let Some((from, _)) = recovering {
            diag!("frames_lost", "from={from} why=repair_lost_in_queue");
            self.signal_video(VideoSignal::FramesLost(from));
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

/// What a run loop holds: the real one ([`Transport::start`]) or a
/// [`Remote`] one.
struct Ends {
    cmds: mpsc::Receiver<Cmd>,
    audio: mpsc::Receiver<AudioPacket>,
    shared: Arc<Shared>,
    /// Dropped once the run loop is gone.
    exit: oneshot::Sender<()>,
}

impl Transport {
    /// Bind the sockets and start the transport on its own thread. Events
    /// arrive on the returned receiver.
    ///
    /// The run loop paces every video packet out, so it gets an OS thread
    /// and a runtime of its own (with the MMCSS "Games" class on Windows)
    /// rather than sharing the host's tokio workers with HTTPS, file
    /// transfer and disk writes, any of which could make a pacer wakeup
    /// late. The sockets' reader tasks run there too.
    pub async fn start(config: TransportConfig) -> Result<(Transport, Events), Error> {
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
        let (transport, events, ends) = Transport::new();
        let Ends {
            cmds: cmd_rx,
            audio: audio_rx,
            shared,
            exit: exit_tx,
        } = ends;

        let (ready_tx, ready_rx) = oneshot::channel();
        let thread_shared = shared.clone();
        std::thread::Builder::new()
            .name("ws-transport".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = ready_tx.send(Err(Error::Io(e)));
                        return;
                    }
                };
                let _mmcss = timer::Mmcss::enter();
                runtime.block_on(async move {
                    let (pkt_tx, pkt_rx) = mpsc::channel(1024);
                    let filter = SharedFilter::default();
                    let net = match Net::bind(
                        &ips,
                        config.udp_port,
                        config.tcp_port,
                        filter.clone(),
                        pkt_tx,
                    )
                    .await
                    {
                        Ok(net) => net,
                        Err(e) => {
                            let _ = ready_tx.send(Err(e.into()));
                            return;
                        }
                    };
                    tracing::info!(udp = ?net.udp_addrs, tcp = ?net.tcp_addrs, "transport listening");
                    let driver = Driver {
                        config,
                        net,
                        filter,
                        confirmed: HashSet::new(),
                        session: None,
                        shared: thread_shared,
                        last_estimate: None,
                    };
                    if ready_tx.send(Ok(())).is_err() {
                        return;
                    }
                    // Ends on Transport::shutdown, or once every Transport
                    // handle is gone; dropping the runtime then stops the
                    // socket tasks.
                    driver.run(cmd_rx, pkt_rx, audio_rx).await;
                });
                drop(runtime);
                let _ = exit_tx.send(());
            })?;
        ready_rx.await.map_err(|_| Error::Stopped)??;
        Ok((transport, events))
    }

    /// A handle, its events, and the ends a run loop drives.
    fn new() -> (Transport, Events, Ends) {
        let (cmds, cmd_rx) = mpsc::channel(CMD_QUEUE);
        let (events_tx, events) = mpsc::unbounded_channel();
        let queued_data = Arc::new(AtomicUsize::new(0));
        // About 80 ms of 10 ms Opus packets; beyond that, old audio is useless.
        let (audio, audio_rx) = mpsc::channel(8);
        let (exit_tx, exit_rx) = oneshot::channel();
        let shared = Arc::new(Shared {
            frames: Mutex::new(VecDeque::new()),
            notify: Notify::new(),
            connected: AtomicBool::new(false),
            keyframe_asked: Mutex::new(None),
            recovering: Mutex::new(None),
            trouble: TroubleCounts::default(),
            events: events_tx,
            queued_data: queued_data.clone(),
            input: Mutex::default(),
            video: Mutex::default(),
            backlog: SendBacklog::default(),
            backlog_changed: Notify::new(),
            forget_estimate: AtomicBool::new(false),
            exited: Mutex::new(Some(exit_rx)),
        });
        (
            Transport {
                cmds,
                shared: shared.clone(),
                audio,
            },
            Events {
                rx: events,
                queued_data,
            },
            Ends {
                cmds: cmd_rx,
                audio: audio_rx,
                shared,
                exit: exit_tx,
            },
        )
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

    /// Whether the current peer connection is up (ICE and DTLS).
    pub fn is_connected(&self) -> bool {
        self.shared.connected.load(Ordering::Relaxed)
    }

    /// End the current peer connection.
    /// Stops the transport for good, and returns once its thread has closed
    /// every socket, so another transport can bind the same ports. Every
    /// handle fails with [`Error::Stopped`] from then on.
    pub async fn shutdown(&self) {
        let _ = self.cmds.send(Cmd::Shutdown).await;
        let exited = self
            .shared
            .exited
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(exited) = exited {
            let _ = exited.await;
        }
    }

    pub fn close(&self) {
        self.send_cmd(Cmd::Close);
    }

    /// Sends a command that must not be lost, without waiting.
    fn send_cmd(&self, cmd: Cmd) {
        if let Err(mpsc::error::TrySendError::Full(cmd)) = self.cmds.try_send(cmd) {
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

    /// Send keyframe requests and repairs straight to `sink` (the encoder)
    /// from the run loop, instead of as events. A repair then never waits
    /// behind the app's other work (a pipeline being rebuilt, say).
    ///
    /// Call it with each new encoder, before its first frame: frames still
    /// queued from the last one are discarded, and any repair in progress
    /// is forgotten, since the new encoder starts with a keyframe and its
    /// frame numbers start again.
    pub fn set_video_sink(&self, sink: VideoSink) {
        let mut video = self.shared.video.lock().unwrap_or_else(|e| e.into_inner());
        self.shared.frames.lock().expect("video queue lock").clear();
        self.shared.recovering.lock().expect("recovery lock").take();
        *video = Some(sink);
        drop(video);
        self.send_cmd(Cmd::VideoSink(true));
    }

    /// The next offer is for another device: its estimate starts from
    /// [`TransportConfig::initial_bitrate_bps`], not from the last
    /// connection's (see [`ESTIMATE_KEPT_FOR`]). Call before
    /// [`Transport::offer`].
    pub fn forget_estimate(&self) {
        self.shared.forget_estimate.store(true, Ordering::Relaxed);
    }

    /// Keyframe requests and repairs go back to being events.
    pub fn clear_video_sink(&self) {
        self.shared
            .video
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        self.send_cmd(Cmd::VideoSink(false));
    }

    /// Stop handing input to the sink; it waits for the next one.
    pub fn clear_input_sink(&self) {
        self.shared.input_route().sink = None;
    }

    /// How far behind the video send queue is, updated as it sends. The
    /// encoder skips frames while it is behind rather than send frames that
    /// would only be dropped as late.
    pub fn send_backlog(&self) -> SendBacklog {
        self.shared.backlog.clone()
    }

    /// Resolve when the send backlog has changed since the last time this
    /// returned. A change made while nobody was waiting is not lost: the
    /// next call returns at once. It says only that the value moved (read it
    /// with [`Transport::send_backlog`]), and the move can be tiny, so the
    /// caller decides what is worth acting on. Meant for one watcher.
    pub async fn backlog_changed(&self) {
        self.shared.backlog_changed.notified().await;
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
    /// The last bandwidth estimate, and when.
    last_estimate: Option<(u64, Instant)>,
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
                    Some(Cmd::Shutdown) => {
                        self.end_session();
                        return;
                    }
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
        // Each step is acted on as str0m yields it, so a packet goes to the socket
        // straight from str0m's buffer, in order, and an event still takes effect
        // after the packets before it and before those after it.
        let deadline = s.poll(|step| {
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
                    if !self.net.send(proto, source, destination, data) {
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
                    TransportEvent::TargetBitrate(bps) => {
                        self.last_estimate = Some((bps, Instant::now()));
                        self.shared.emit(TransportEvent::TargetBitrate(bps))
                    }
                    TransportEvent::Data { channel, data, .. }
                        if channel == channels::INPUT || channel == channels::INPUT_MOTION =>
                    {
                        self.shared.input_route().input(data)
                    }
                    ev => self.shared.emit(ev),
                },
            }
        });
        let alive = s.is_alive();
        if !alive {
            self.end_session();
        } else if let Some(s) = &mut self.session {
            let connected = s.is_connected();
            self.shared.connected.store(connected, Ordering::Relaxed);
            let age = if connected {
                s.queue_diag(Instant::now()).0
            } else {
                Duration::ZERO
            };
            self.shared.set_backlog(age);
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

    /// Where a new peer connection's bandwidth estimate starts.
    fn start_estimate(&self, now: Instant) -> u64 {
        match self.last_estimate {
            Some((bps, at)) if now.saturating_duration_since(at) < ESTIMATE_KEPT_FOR => {
                ((bps as f64 * KEPT_ESTIMATE_SHARE) as u64).min(self.config.max_bitrate_bps)
            }
            _ => self.config.initial_bitrate_bps,
        }
    }

    /// Drop the session, reporting Disconnected unless already reported.
    fn end_session(&mut self) {
        if let Some(mut s) = self.session.take() {
            if s.mark_down() {
                self.shared.emit(TransportEvent::Disconnected);
            }
        }
        self.shared.connected.store(false, Ordering::Relaxed);
        self.shared.set_backlog(Duration::ZERO);
        self.shared.input_route().reset();
        self.filter.lock().expect("filter lock").reset(None);
        self.confirmed.clear();
    }

    fn command(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Offer(extra, reply) => {
                self.end_session();
                if self.shared.forget_estimate.swap(false, Ordering::Relaxed) {
                    self.last_estimate = None;
                }
                let candidates = self.candidates(&extra);
                let now = Instant::now();
                let config = TransportConfig {
                    initial_bitrate_bps: self.start_estimate(now),
                    ..self.config.clone()
                };
                let result = Session::offer(&config, &candidates, now).map(|(mut s, sdp)| {
                    let ufrag = s.local_ufrag();
                    self.filter.lock().expect("filter lock").reset(Some(&ufrag));
                    self.session = Some(s);
                    sdp
                });
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
            Cmd::Shutdown => unreachable!("handled by the run loop"),
            Cmd::VideoSink(_) => {}
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
    async fn a_backlog_change_between_waits_is_not_lost() {
        let (transport, _events, _ends) = Transport::new();
        transport.shared.set_backlog(Duration::from_millis(5));
        tokio::time::timeout(Duration::from_secs(1), transport.backlog_changed())
            .await
            .expect("the change made before waiting wakes the wait");
        // Several changes before one wait coalesce into a single wakeup.
        for ms in [6, 7, 8] {
            transport.shared.set_backlog(Duration::from_millis(ms));
        }
        tokio::time::timeout(Duration::from_secs(1), transport.backlog_changed())
            .await
            .expect("the changes wake one wait");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), transport.backlog_changed())
                .await
                .is_err()
        );
        // Setting the same value again is no change, so nothing wakes.
        transport.shared.set_backlog(Duration::from_millis(8));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), transport.backlog_changed())
                .await
                .is_err()
        );
    }

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

    #[tokio::test]
    async fn data_events_are_bounded_but_connection_events_never_drop() {
        let (transport, mut events) = Transport::start(TransportConfig {
            ips: vec![[127, 0, 0, 1].into()],
            udp_port: 0,
            tcp_port: None,
            ..Default::default()
        })
        .await
        .unwrap();
        let shared = &transport.shared;
        let data = || TransportEvent::Data {
            channel: channels::FILE,
            binary: true,
            data: vec![0],
        };
        for _ in 0..DATA_QUEUE + 10 {
            shared.emit(data());
        }
        let control = TransportEvent::Data {
            channel: channels::CONTROL,
            binary: false,
            data: b"{}".to_vec(),
        };
        shared.emit(control.clone());
        shared.emit(TransportEvent::Connected);
        let mut got = Vec::new();
        while let Ok(ev) = events.try_recv() {
            got.push(ev);
        }
        assert_eq!(got.len(), DATA_QUEUE + 2);
        assert_eq!(got[DATA_QUEUE], control);
        assert_eq!(got.last(), Some(&TransportEvent::Connected));
        // Taking them makes room again.
        shared.emit(data());
        assert_eq!(events.try_recv().ok(), Some(data()));
    }

    #[tokio::test]
    async fn shutdown_frees_the_port() {
        let port = std::net::UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let (transport, _events) = Transport::start(TransportConfig {
            ips: vec![[127, 0, 0, 1].into()],
            udp_port: port,
            tcp_port: None,
            ..Default::default()
        })
        .await
        .unwrap();
        let other = transport.clone();
        transport.shutdown().await;
        // Even with a handle left, the sockets are gone.
        std::net::UdpSocket::bind(("127.0.0.1", port)).unwrap();
        assert!(matches!(other.offer().await, Err(Error::Stopped)));
    }

    #[tokio::test]
    async fn video_signals_go_to_the_sink_while_one_is_set() {
        let (transport, mut events) = Transport::start(TransportConfig {
            ips: vec![[127, 0, 0, 1].into()],
            udp_port: 0,
            tcp_port: None,
            ..Default::default()
        })
        .await
        .unwrap();
        let got = Arc::new(Mutex::new(Vec::new()));
        let sink_got = got.clone();
        transport.set_video_sink(Box::new(move |s| sink_got.lock().unwrap().push(s)));
        transport.shared.keyframe_needed("test");
        transport.shared.signal_video(VideoSignal::FramesLost(7));
        assert_eq!(
            *got.lock().unwrap(),
            [VideoSignal::Keyframe, VideoSignal::FramesLost(7)]
        );
        assert!(events.try_recv().is_err());

        transport.clear_video_sink();
        transport.shared.signal_video(VideoSignal::FramesLost(9));
        assert_eq!(events.try_recv().ok(), Some(TransportEvent::FramesLost(9)));
    }
}
