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
//! asks the encoder for a keyframe. NACKs are answered only for packets
//! younger than [`TransportConfig::max_resend_age`].
//!
//! The host is ICE-lite and reachable directly: one UDP port (and optionally
//! one ICE-TCP port for networks that block UDP) on each LAN address, plus any
//! public addresses that forward to them.

mod net;
mod session;

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use str0m::net::Protocol;
use tokio::sync::{Notify, mpsc, oneshot};
use ws_proto::{AudioPacket, EncodedFrame};

use crate::net::{Net, Packet, SharedFilter};
use crate::session::{Session, Step, VideoWrite};

/// Events waiting for the host app. If it falls this far behind, events are
/// dropped rather than stalling media.
const EVENT_QUEUE: usize = 1024;
const CMD_QUEUE: usize = 64;
/// A keyframe request is repeated only if no keyframe went out this long
/// after the last one.
const KEYFRAME_RETRY: Duration = Duration::from_millis(300);

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
    /// instead.
    pub max_resend_age: Duration,
    /// Drop a (non-key) frame instead of queueing it when the oldest unsent
    /// video packet has waited longer than this.
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
            max_resend_age: Duration::from_millis(50),
            max_send_delay: Duration::from_millis(25),
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
    /// Bandwidth estimate, in bits per second; drive the encoder bitrate.
    TargetBitrate(u64),
    ChannelOpen(&'static str),
    ChannelClosed(&'static str),
    /// The channel's send buffer drained; more data can be written.
    ChannelLow(&'static str),
    Data {
        channel: &'static str,
        binary: bool,
        data: Vec<u8>,
    },
    /// One Opus packet from the browser microphone.
    MicAudio(AudioPacket),
    Stats(TransportStats),
}

#[derive(Debug, Clone, PartialEq)]
pub struct TransportStats {
    pub rtt: Option<Duration>,
    pub egress_loss: Option<f32>,
    pub bwe: Option<u64>,
    pub bytes_tx: u64,
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
    Offer(oneshot::Sender<Result<String, Error>>),
    Answer(String, oneshot::Sender<Result<(), Error>>),
    Candidate(String),
    Close,
    Data {
        channel: String,
        binary: bool,
        data: Vec<u8>,
        reply: oneshot::Sender<Result<bool, Error>>,
    },
}

/// State shared by the handle and the run loop.
struct Shared {
    /// Newest-only slot between the encoder and the send loop.
    frame: Mutex<Option<EncodedFrame>>,
    notify: Notify,
    connected: AtomicBool,
    /// When the outstanding keyframe request was made.
    keyframe_asked: Mutex<Option<Instant>>,
    events: mpsc::Sender<TransportEvent>,
}

impl Shared {
    fn emit(&self, ev: TransportEvent) {
        if let Err(mpsc::error::TrySendError::Full(ev)) = self.events.try_send(ev) {
            tracing::warn!(?ev, "transport event queue full; dropping event");
        }
    }

    fn keyframe_needed(&self) {
        let now = Instant::now();
        let mut asked = self.keyframe_asked.lock().expect("keyframe lock");
        if asked.is_some_and(|t| now.duration_since(t) < KEYFRAME_RETRY) {
            return;
        }
        *asked = Some(now);
        drop(asked);
        self.emit(TransportEvent::KeyframeNeeded);
    }

    fn keyframe_sent(&self) {
        *self.keyframe_asked.lock().expect("keyframe lock") = None;
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
        let candidates = session::candidates(&net.udp_addrs, &net.tcp_addrs, &config.public_ips);

        let (cmds, cmd_rx) = mpsc::channel(CMD_QUEUE);
        let (events_tx, events) = mpsc::channel(EVENT_QUEUE);
        // About 80 ms of 10 ms Opus packets; beyond that, old audio is useless.
        let (audio, audio_rx) = mpsc::channel(8);
        let shared = Arc::new(Shared {
            frame: Mutex::new(None),
            notify: Notify::new(),
            connected: AtomicBool::new(false),
            keyframe_asked: Mutex::new(None),
            events: events_tx,
        });
        let driver = Driver {
            config,
            net,
            filter,
            confirmed: HashSet::new(),
            candidates,
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
        let (tx, rx) = oneshot::channel();
        self.cmds
            .send(Cmd::Offer(tx))
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

    /// Hand over the newest encoded frame. If the previous one has not been
    /// sent yet it is discarded and [`TransportEvent::KeyframeNeeded`] is
    /// raised (while connected), because later frames reference it.
    pub fn send_video(&self, frame: EncodedFrame) {
        let replaced = self
            .shared
            .frame
            .lock()
            .expect("video slot lock")
            .replace(frame);
        if replaced.is_some() && self.shared.connected.load(Ordering::Relaxed) {
            self.shared.keyframe_needed();
        }
        self.shared.notify.notify_one();
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
    candidates: Vec<str0m::Candidate>,
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
                    let frame = shared.frame.lock().expect("video slot lock").take();
                    if let (Some(frame), Some(s)) = (frame, &mut self.session) {
                        match s.write_video(&frame, Instant::now()) {
                            VideoWrite::Sent if frame.keyframe => shared.keyframe_sent(),
                            VideoWrite::Late => shared.keyframe_needed(),
                            _ => {}
                        }
                    }
                }
                Some(pkt) = audio.recv() => {
                    if let Some(s) = &mut self.session {
                        s.write_audio(&pkt, Instant::now());
                    }
                }
                _ = tokio::time::sleep_until(deadline.into()) => {
                    if let Some(s) = &mut self.session {
                        s.timeout(Instant::now());
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
                    self.net.send(proto, source, destination, &data);
                }
                Step::Event(ev) => match ev {
                    TransportEvent::Connected => {
                        self.shared.connected.store(true, Ordering::Relaxed);
                        self.shared.emit(TransportEvent::Connected);
                        // The first frame the browser sees must be decodable.
                        self.shared.keyframe_sent();
                        self.shared.keyframe_needed();
                    }
                    TransportEvent::KeyframeNeeded => self.shared.keyframe_needed(),
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

    /// Drop the session, reporting Disconnected unless already reported.
    fn end_session(&mut self) {
        if let Some(mut s) = self.session.take() {
            if s.mark_down() {
                self.shared.emit(TransportEvent::Disconnected);
            }
        }
        self.shared.connected.store(false, Ordering::Relaxed);
        self.filter.lock().expect("filter lock").reset(None);
        self.confirmed.clear();
    }

    fn command(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Offer(reply) => {
                self.end_session();
                let result = Session::offer(&self.config, &self.candidates, Instant::now()).map(
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
