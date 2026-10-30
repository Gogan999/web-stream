//! The host as two processes, for the Windows service: everything that
//! parses what comes off the network (HTTPS, the signaling socket, DTLS,
//! SRTP, SCTP) in a network process that runs with almost no rights
//! ([`serve_network`]), and capture, encoding, input, audio and the cursor in
//! a SYSTEM one ([`serve_media`]). A bug in the network-facing code then
//! hands an attacker that process, not SYSTEM.
//!
//! They talk over one byte stream (a pipe the service hands both). The
//! media side drives a [`Transport::remote`] handle; what it sends goes to
//! the real transport in the network process, and what that transport
//! reports comes back, along with signaling's news (accepted codecs, which
//! device is connected), notices and the console's pairing commands.
//!
//! The media side treats everything from the network process as untrusted,
//! as it treats a browser: messages are size-limited and read with
//! back-pressure, input and control messages go through the same parsing
//! as before, and text for the console is cleaned and labelled. The only
//! paths it sends (the data folder and web root) are shown, never opened.
//!
//! Each message is a frame: a little-endian u32 length, then a u32 header
//! length, a JSON header and the raw payload (a frame's bitstream, a data
//! channel message).

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::Context;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, watch};
use ws_proto::{
    AudioDirection, AudioPacket, DisplayId, EncodedFrame, VideoCodec, channels, mono_now,
};
use ws_signaling::Notice;
use ws_transport::{RemoteCmd, Transport, TransportEvent, TransportStats, Trouble};

use crate::files::FileCommand;
use crate::media;
use crate::options::ServeOptions;
use crate::serve::{self, Command, Event, Network};

/// The largest frame the network side accepts: well above any keyframe.
const MAX_UP: usize = 64 << 20;
/// The largest frame the media side accepts from the network process: a
/// data channel message or a mic packet, never anywhere near this.
const MAX_DOWN: usize = 4 << 20;
/// Frames read but not yet handled. Past this the reader stops reading,
/// so a flood from the other side waits in the pipe, not in memory.
const READ_QUEUE: usize = 256;
/// The longest the network side goes without checking the connection state.
/// A backlog change is sent as it happens (the encoder reads it to hold
/// frames back), so this only has to catch connection changes and keep
/// nothing depending on a message arriving. Slow, since a 5 ms tick woke the
/// process about 200 times a second even with nothing streaming; idle it
/// now wakes about 5 times a second.
const STATE_POLL: Duration = Duration::from_millis(200);
/// The least time between `State` messages sent because the backlog moved.
/// Sending on change removes the ceiling the old 5 ms tick gave, about 200
/// a second, and under load the backlog can cross [`BACKLOG_STEP`] on every
/// driver pass; each message costs the media process a pipe read, a parse
/// and a wakeup. A change after a quiet spell still goes at once, and one
/// held back goes when this has passed. Connection changes ignore it.
const BACKLOG_SPACING: Duration = Duration::from_millis(5);
/// A backlog change smaller than this isn't worth a message.
const BACKLOG_STEP: Duration = Duration::from_millis(1);
/// Video and game audio waiting for the network process past this are
/// dropped. The gap in frame numbers then gets the picture repaired, as any
/// dropped frame does.
const MAX_QUEUED: usize = 8 << 20;

/// From the media side to the network side.
#[derive(Debug, Serialize, Deserialize)]
enum Up {
    /// The payload is the bitstream. Times are ages, since each process's
    /// monotonic clock has its own zero.
    Video {
        display: DisplayId,
        codec: VideoCodec,
        frame_index: u64,
        keyframe: bool,
        recovers_from: Option<u64>,
        capture_age_us: u64,
        encode_age_us: u64,
        width: u32,
        height: u32,
    },
    /// Game audio; the payload is one Opus packet.
    Audio {
        sequence: u64,
        capture_age_us: u64,
        sample_rate: u32,
        channels: u8,
    },
    /// The payload is the message; answered with [`Down::DataSent`].
    Data {
        id: u64,
        channel: String,
        binary: bool,
    },
    MaxBitrate(u64),
    VideoSink(bool),
    Pairing(PairingCommand),
}

/// Console commands the network side carries out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum PairingCommand {
    Open,
    DropSession,
    UnpairAll,
}

/// From the network side to the media side.
#[derive(Debug, Serialize, Deserialize)]
enum Down {
    Transport(Report),
    State {
        connected: bool,
        backlog_us: u64,
    },
    DataSent {
        id: u64,
        result: Result<bool, SendError>,
    },
    Codecs(Option<Vec<VideoCodec>>),
    Peer {
        count: u64,
        device: String,
    },
    Started {
        port: u16,
        ips: Vec<IpAddr>,
        fingerprint: String,
        self_signed: bool,
        web_root: Option<PathBuf>,
        data_dir: PathBuf,
    },
    PairingOpened {
        link: String,
    },
    UnpairedAll,
    Notice(Notice),
    Error(String),
}

/// A [`TransportEvent`], as it crosses.
#[derive(Debug, Serialize, Deserialize)]
enum Report {
    Connected,
    Disconnected,
    KeyframeNeeded,
    FramesLost(u64),
    TargetBitrate(u64),
    ChannelOpen(String),
    ChannelClosed(String),
    ChannelLow(String),
    /// The payload is the message.
    Data {
        channel: String,
        binary: bool,
    },
    /// An input channel message (the payload), for the input sink.
    Input,
    /// One Opus packet from the browser mic (the payload). Its time is the
    /// RTP time, the same in both processes.
    Mic {
        sequence: u64,
        capture_time: u64,
        sample_rate: u32,
        channels: u8,
    },
    Stats {
        rtt_us: Option<u64>,
        egress_loss: Option<f32>,
        bwe: Option<u64>,
        bytes_tx: u64,
        trouble: [u32; 5],
    },
}

#[derive(Debug, Serialize, Deserialize)]
enum SendError {
    NoSession,
    UnknownChannel(String),
    ChannelNotOpen(String),
    Stopped,
    Other(String),
}

impl From<ws_transport::Error> for SendError {
    fn from(e: ws_transport::Error) -> Self {
        match e {
            ws_transport::Error::NoSession => Self::NoSession,
            ws_transport::Error::UnknownChannel(c) => Self::UnknownChannel(c),
            ws_transport::Error::ChannelNotOpen(c) => Self::ChannelNotOpen(c),
            ws_transport::Error::Stopped => Self::Stopped,
            e => Self::Other(e.to_string()),
        }
    }
}

impl From<SendError> for ws_transport::Error {
    fn from(e: SendError) -> Self {
        match e {
            SendError::NoSession => Self::NoSession,
            SendError::UnknownChannel(c) => Self::UnknownChannel(c),
            SendError::ChannelNotOpen(c) => Self::ChannelNotOpen(c),
            SendError::Stopped => Self::Stopped,
            SendError::Other(m) => Self::Negotiation(m),
        }
    }
}

/// One frame, ready to write: the length, header length and header in one
/// buffer, and the payload kept apart so it is never copied into it. The
/// payload is whatever the caller already holds (a `Vec`, a shared `Bytes`),
/// so a video frame goes from the encoder to the pipe without a copy.
struct Frame {
    head: Vec<u8>,
    payload: Box<dyn AsRef<[u8]> + Send>,
}

impl Frame {
    /// Bytes on the wire.
    fn len(&self) -> usize {
        self.head.len() + self.payload.as_ref().as_ref().len()
    }
}

/// The wire form is `[u32 len][u32 header_len][header][payload]`, little
/// endian, where `len` counts everything after itself.
fn frame<H: Serialize>(header: &H, payload: impl AsRef<[u8]> + Send + 'static) -> Frame {
    let header = serde_json::to_vec(header).expect("bridge messages serialize");
    let len = 4 + header.len() + payload.as_ref().len();
    let mut head = Vec::with_capacity(8 + header.len());
    head.extend_from_slice(&(len as u32).to_le_bytes());
    head.extend_from_slice(&(header.len() as u32).to_le_bytes());
    head.extend_from_slice(&header);
    Frame {
        head,
        payload: Box::new(payload),
    }
}

/// The next frame's header and payload; `None` at a clean end of stream.
/// The payload is read straight into its own buffer, so nothing is shifted
/// after the read.
async fn read_frame<H: DeserializeOwned>(
    read: &mut (impl AsyncRead + Unpin),
    max: usize,
) -> anyhow::Result<Option<(H, Vec<u8>)>> {
    let mut len = [0u8; 4];
    match read.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_le_bytes(len) as usize;
    // Checked before anything is allocated for it.
    if !(4..=max).contains(&len) {
        anyhow::bail!("bad frame length {len}");
    }
    let mut header_len = [0u8; 4];
    read.read_exact(&mut header_len).await?;
    let header_len = u32::from_le_bytes(header_len) as usize;
    if header_len > len - 4 {
        anyhow::bail!("bad header length {header_len}");
    }
    let mut header = vec![0u8; header_len];
    read.read_exact(&mut header).await?;
    let header = serde_json::from_slice(&header).context("bad header")?;
    let mut payload = vec![0u8; len - 4 - header_len];
    read.read_exact(&mut payload).await?;
    Ok(Some((header, payload)))
}

/// Reads frames of up to `max` bytes into a channel until the stream ends
/// or breaks; the channel then closes. Waits while [`READ_QUEUE`] frames
/// are unhandled. Each side handles what it reads without waiting on its
/// own writes, so neither can stall the other.
fn spawn_reader<H: DeserializeOwned + Send + 'static>(
    mut read: impl AsyncRead + Unpin + Send + 'static,
    max: usize,
) -> mpsc::Receiver<(H, Vec<u8>)> {
    let (tx, rx) = mpsc::channel(READ_QUEUE);
    tokio::spawn(async move {
        loop {
            match read_frame(&mut read, max).await {
                Ok(Some(msg)) => {
                    if tx.send(msg).await.is_err() {
                        return;
                    }
                }
                Ok(None) => return,
                Err(e) => return tracing::warn!("bridge: {e:#}"),
            }
        }
    });
    rx
}

/// Frames waiting to be written, and how many bytes they hold.
#[derive(Clone)]
struct Out {
    tx: mpsc::UnboundedSender<Frame>,
    queued: Arc<AtomicUsize>,
}

impl Out {
    fn send(&self, frame: Frame) {
        let len = frame.len();
        self.queued.fetch_add(len, Ordering::Relaxed);
        if self.tx.send(frame).is_err() {
            self.queued.fetch_sub(len, Ordering::Relaxed);
        }
    }

    /// Bytes not yet written: a network process that stops reading must not
    /// make this one queue video without bound.
    fn queued(&self) -> usize {
        self.queued.load(Ordering::Relaxed)
    }
}

/// Writes queued frames until the queue closes or the stream breaks.
fn spawn_writer(
    mut write: impl AsyncWrite + Unpin + Send + 'static,
) -> (Out, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::unbounded_channel::<Frame>();
    let queued = Arc::new(AtomicUsize::new(0));
    let out = Out {
        tx,
        queued: queued.clone(),
    };
    let task = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            // Only this task writes, so the two writes stay one run of bytes.
            let mut result = write.write_all(&frame.head).await;
            let payload = frame.payload.as_ref().as_ref();
            if result.is_ok() && !payload.is_empty() {
                result = write.write_all(payload).await;
            }
            queued.fetch_sub(frame.len(), Ordering::Relaxed);
            if let Err(e) = result {
                return tracing::debug!("bridge: {e}");
            }
        }
        let _ = write.shutdown().await;
    });
    (out, task)
}

/// How long ago `t` was, on this process's clock.
fn age(t: u64) -> u64 {
    mono_now().saturating_sub(t)
}

/// The time that was `age` ago, on this process's clock.
fn ago(age: u64) -> u64 {
    mono_now().saturating_sub(age)
}

/// The state to send now, if it differs enough from the last one sent: the
/// connection changed, or the backlog moved by at least [`BACKLOG_STEP`].
/// Anything less, including a steady backlog, sends nothing.
fn state_change(
    sent: (bool, Duration),
    connected: bool,
    backlog: Duration,
) -> Option<(bool, Duration)> {
    let backlog = if connected { backlog } else { Duration::ZERO };
    (connected != sent.0 || backlog.abs_diff(sent.1) >= BACKLOG_STEP)
        .then_some((connected, backlog))
}

/// What the network side last told the media side, and when, so backlog
/// changes can be sent promptly without exceeding [`BACKLOG_SPACING`].
struct StateReporter {
    sent: (bool, Duration),
    last_send: Option<tokio::time::Instant>,
    /// A step-sized backlog change arrived inside the spacing and has not
    /// been sent. Nothing is stored of its value: it is read again when the
    /// spacing ends, so what goes out is current, and a change that has since
    /// reversed sends nothing.
    held: bool,
}

impl StateReporter {
    fn new() -> Self {
        Self {
            sent: (false, Duration::ZERO),
            last_send: None,
            held: false,
        }
    }

    /// When a held change may be sent, if one is held.
    fn due(&self) -> Option<tokio::time::Instant> {
        self.held.then(|| {
            self.last_send
                .map_or_else(tokio::time::Instant::now, |t| t + BACKLOG_SPACING)
        })
    }

    /// Look at the current state; return what to send, if anything. The
    /// periodic floor passes `paced = false`, since it is already far
    /// slower than the spacing. A backlog wakeup passes `true`.
    fn check(
        &mut self,
        now: tokio::time::Instant,
        connected: bool,
        backlog: Duration,
        paced: bool,
    ) -> Option<(bool, Duration)> {
        let Some(next) = state_change(self.sent, connected, backlog) else {
            self.held = false;
            return None;
        };
        let spaced = self.last_send.is_none_or(|t| now >= t + BACKLOG_SPACING);
        if paced && next.0 == self.sent.0 && !spaced {
            self.held = true;
            return None;
        }
        self.held = false;
        self.sent = next;
        self.last_send = Some(now);
        Some(next)
    }
}

fn micros(d: Duration) -> u64 {
    d.as_micros().min(u128::from(u64::MAX)) as u64
}

/// The network process: serves signaling and runs the real transport, for
/// the media process at the other end of `link`. Returns when the media
/// process goes away (or the signaling server fails), after ending the
/// session and closing the sockets.
pub async fn serve_network(
    opts: ServeOptions,
    link: impl AsyncRead + AsyncWrite + Send + 'static,
) -> anyhow::Result<()> {
    // Fix this process's clock zero now, well before any frame's age is
    // turned back into a time.
    let _ = mono_now();
    let (read, write) = tokio::io::split(link);
    let mut incoming = spawn_reader::<Up>(read, MAX_UP);
    let (out, writer) = spawn_writer(write);
    let send = |msg: &Down, payload: &[u8]| {
        out.send(frame(msg, payload.to_vec()));
    };

    let mut net = Network::start(&opts).await?;
    let transport = net.transport.clone();
    let mut events = net.transport_events.take().expect("events not taken yet");
    let mut notices = net.notices.take().expect("notices not taken yet");
    let mut codecs = net.codecs.clone();
    let mut peers = net.peers.clone();
    if let Event::Started {
        port,
        ips,
        fingerprint,
        self_signed,
        web_root,
        data_dir,
    } = net.started.clone()
    {
        send(
            &Down::Started {
                port,
                ips,
                fingerprint,
                self_signed,
                web_root,
                data_dir,
            },
            &[],
        );
    }
    if opts.pair_at_start {
        pairing(&net, PairingCommand::Open, &send).await;
    }

    // Data channel writes, one at a time so they keep their order.
    let (data_tx, mut data_rx) = mpsc::unbounded_channel::<(u64, String, bool, Vec<u8>)>();
    let data_task = tokio::spawn({
        let transport = transport.clone();
        let out = out.clone();
        async move {
            while let Some((id, channel, binary, data)) = data_rx.recv().await {
                let result = transport
                    .send_data(&channel, binary, data)
                    .await
                    .map_err(SendError::from);
                out.send(frame(&Down::DataSent { id, result }, []));
            }
        }
    });

    let mut transitions = 0u64;
    let mut reporter = StateReporter::new();
    // Read the live state and send it if it is worth a message.
    let report_state = |reporter: &mut StateReporter, paced: bool| {
        let connected = transport.is_connected();
        let backlog = transport.send_backlog().get();
        let now = tokio::time::Instant::now();
        if let Some((connected, backlog)) = reporter.check(now, connected, backlog, paced) {
            send(
                &Down::State {
                    connected,
                    backlog_us: micros(backlog),
                },
                &[],
            );
        }
    };
    let mut poll = tokio::time::interval(STATE_POLL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let result = loop {
        let hold_end = reporter.due();
        tokio::select! {
            r = &mut net.server_task => break serve::server_stopped(r),
            msg = incoming.recv() => {
                let Some((msg, payload)) = msg else {
                    tracing::info!("the media process is gone; stopping");
                    break Ok(());
                };
                match msg {
                    Up::Video { display, codec, frame_index, keyframe, recovers_from, capture_age_us, encode_age_us, width, height } => {
                        transport.send_video(EncodedFrame {
                            display,
                            codec,
                            frame_index,
                            keyframe,
                            recovers_from,
                            capture_time: ago(capture_age_us),
                            encode_done_time: ago(encode_age_us),
                            width,
                            height,
                            data: payload.into(),
                        });
                    }
                    Up::Audio { sequence, capture_age_us, sample_rate, channels } => {
                        transport.send_audio(AudioPacket {
                            direction: AudioDirection::Playback,
                            sequence,
                            capture_time: ago(capture_age_us),
                            sample_rate,
                            channels,
                            data: payload.into(),
                        });
                    }
                    Up::Data { id, channel, binary } => {
                        let _ = data_tx.send((id, channel, binary, payload));
                    }
                    Up::MaxBitrate(bps) => transport.set_max_bitrate(bps),
                    Up::VideoSink(true) => {
                        let out = out.clone();
                        transport.set_video_sink(Box::new(move |signal| {
                            let report = match signal {
                                ws_transport::VideoSignal::Keyframe => Report::KeyframeNeeded,
                                ws_transport::VideoSignal::FramesLost(from) => Report::FramesLost(from),
                            };
                            out.send(frame(&Down::Transport(report), []));
                        }));
                    }
                    Up::VideoSink(false) => transport.clear_video_sink(),
                    Up::Pairing(cmd) => pairing(&net, cmd, &send).await,
                }
            }
            ev = events.recv() => {
                let Some(ev) = ev else { break Err(anyhow::anyhow!("transport stopped")) };
                let transition = matches!(ev, TransportEvent::Connected | TransportEvent::Disconnected);
                let (report, payload) = report(ev);
                out.send(frame(&Down::Transport(report), payload));
                if transition {
                    // Input goes across as it arrives, after the event that
                    // says which connection it belongs to.
                    transitions += 1;
                    let out = out.clone();
                    transport.set_input_sink(
                        Box::new(move |data| {
                            out.send(frame(&Down::Transport(Report::Input), data.to_vec()));
                        }),
                        transitions,
                    );
                }
            }
            _ = poll.tick() => report_state(&mut reporter, false),
            () = transport.backlog_changed() => report_state(&mut reporter, true),
            () = tokio::time::sleep_until(hold_end.unwrap_or_else(tokio::time::Instant::now)), if hold_end.is_some() => {
                report_state(&mut reporter, true);
            }
            Ok(()) = codecs.changed() => {
                let accepted = codecs.borrow_and_update().clone();
                send(&Down::Codecs(accepted), &[]);
            }
            Ok(()) = peers.changed() => {
                let peer = peers.borrow_and_update().clone();
                send(&Down::Peer { count: peer.count, device: peer.device }, &[]);
            }
            Some(notice) = notices.recv() => send(&Down::Notice(notice), &[]),
        }
    };
    net.close().await;
    data_task.abort();
    net.shutdown().await;
    drop(out);
    let _ = tokio::time::timeout(Duration::from_secs(1), writer).await;
    result
}

async fn pairing(net: &Network, cmd: PairingCommand, send: &impl Fn(&Down, &[u8])) {
    match cmd {
        PairingCommand::Open => send(
            &match net.signaling.open_pairing().await {
                Ok(link) => Down::PairingOpened { link },
                Err(e) => Down::Error(format!("could not open pairing: {e}")),
            },
            &[],
        ),
        PairingCommand::DropSession => net.signaling.drop_session().await,
        PairingCommand::UnpairAll => send(
            &match net.signaling.revoke_all().await {
                Ok(()) => Down::UnpairedAll,
                Err(e) => Down::Error(format!("could not remove paired browsers: {e}")),
            },
            &[],
        ),
    }
}

/// A transport event as it crosses, and its payload.
fn report(ev: TransportEvent) -> (Report, Vec<u8>) {
    let report = match ev {
        TransportEvent::Connected => Report::Connected,
        TransportEvent::Disconnected => Report::Disconnected,
        TransportEvent::KeyframeNeeded => Report::KeyframeNeeded,
        TransportEvent::FramesLost(from) => Report::FramesLost(from),
        TransportEvent::TargetBitrate(bps) => Report::TargetBitrate(bps),
        TransportEvent::ChannelOpen(c) => Report::ChannelOpen(c.into()),
        TransportEvent::ChannelClosed(c) => Report::ChannelClosed(c.into()),
        TransportEvent::ChannelLow(c) => Report::ChannelLow(c.into()),
        TransportEvent::Data {
            channel,
            binary,
            data,
        } => {
            return (
                Report::Data {
                    channel: channel.into(),
                    binary,
                },
                data,
            );
        }
        TransportEvent::MicAudio(p) => {
            return (
                Report::Mic {
                    sequence: p.sequence,
                    capture_time: p.capture_time,
                    sample_rate: p.sample_rate,
                    channels: p.channels,
                },
                p.data.to_vec(),
            );
        }
        TransportEvent::Stats(s) => Report::Stats {
            rtt_us: s.rtt.map(micros),
            egress_loss: s.egress_loss,
            bwe: s.bwe,
            bytes_tx: s.bytes_tx,
            trouble: [
                s.trouble.browser_keyframe_requests,
                s.trouble.late_frames,
                s.trouble.lost_frames,
                s.trouble.send_drops,
                s.trouble.recovered,
            ],
        },
    };
    (report, Vec::new())
}

/// The event a report stands for on the media side; `None` for a channel
/// this host doesn't have.
fn event(report: Report, payload: Vec<u8>) -> Option<TransportEvent> {
    Some(match report {
        Report::Connected => TransportEvent::Connected,
        Report::Disconnected => TransportEvent::Disconnected,
        Report::KeyframeNeeded => TransportEvent::KeyframeNeeded,
        Report::FramesLost(from) => TransportEvent::FramesLost(from),
        Report::TargetBitrate(bps) => TransportEvent::TargetBitrate(bps),
        Report::ChannelOpen(c) => TransportEvent::ChannelOpen(channels::known(&c)?),
        Report::ChannelClosed(c) => TransportEvent::ChannelClosed(channels::known(&c)?),
        Report::ChannelLow(c) => TransportEvent::ChannelLow(channels::known(&c)?),
        Report::Data { channel, binary } => TransportEvent::Data {
            channel: channels::known(&channel)?,
            binary,
            data: payload,
        },
        Report::Input => TransportEvent::Data {
            channel: channels::INPUT,
            binary: true,
            data: payload,
        },
        Report::Mic {
            sequence,
            capture_time,
            sample_rate,
            channels,
        } => TransportEvent::MicAudio(AudioPacket {
            direction: AudioDirection::Capture,
            sequence,
            capture_time,
            sample_rate,
            channels,
            data: payload.into(),
        }),
        Report::Stats {
            rtt_us,
            egress_loss,
            bwe,
            bytes_tx,
            trouble,
        } => TransportEvent::Stats(TransportStats {
            rtt: rtt_us.map(Duration::from_micros),
            egress_loss,
            bwe,
            bytes_tx,
            trouble: Trouble {
                browser_keyframe_requests: trouble[0],
                late_frames: trouble[1],
                lost_frames: trouble[2],
                send_drops: trouble[3],
                recovered: trouble[4],
            },
        }),
    })
}

/// The media process: capture, encoding, input, audio and the cursor, for
/// the browser the network process at the other end of `link` connects.
/// Driven like [`serve::serve`]; returns on [`Command::Quit`], or with an
/// error once the network process is gone.
pub async fn serve_media(
    opts: ServeOptions,
    link: impl AsyncRead + AsyncWrite + Send + 'static,
    mut commands: mpsc::Receiver<Command>,
    events: mpsc::UnboundedSender<Event>,
) -> anyhow::Result<()> {
    let (read, write) = tokio::io::split(link);
    let mut incoming = spawn_reader::<Down>(read, MAX_DOWN);
    let (out, writer) = spawn_writer(write);
    let send = |msg: &Up, payload: &[u8]| {
        out.send(frame(msg, payload.to_vec()));
    };

    let (transport, transport_events, mut remote) = Transport::remote();
    let (codecs_tx, codecs) = watch::channel(None);
    let (peers_tx, peers) = watch::channel(media::Peer::default());
    let (media_stop, media_stop_rx) = oneshot::channel();
    let (files, files_rx) = mpsc::channel(8);
    let media_task = tokio::spawn(media::run(
        transport.clone(),
        transport_events,
        opts.media.clone(),
        media::Signaled { codecs, peers },
        files_rx,
        events.clone(),
        media_stop_rx,
    ));

    let mut next_id = 0u64;
    let mut replies = std::collections::HashMap::new();
    let result = loop {
        tokio::select! {
            msg = incoming.recv() => {
                let Some((msg, payload)) = msg else {
                    break Err(anyhow::anyhow!("the network process stopped"));
                };
                match msg {
                    Down::Transport(report) => {
                        if let Some(ev) = event(report, payload) {
                            remote.event(ev);
                        }
                    }
                    Down::State { connected, backlog_us } => {
                        remote.set_state(connected, Duration::from_micros(backlog_us));
                    }
                    Down::DataSent { id, result } => {
                        if let Some(reply) = replies.remove(&id) {
                            let _: Result<_, _> = oneshot::Sender::send(reply, result.map_err(Into::into));
                        }
                    }
                    Down::Codecs(accepted) => {
                        codecs_tx.send_replace(accepted);
                    }
                    Down::Peer { count, device } => {
                        peers_tx.send_replace(media::Peer { count, device });
                    }
                    Down::Started { port, ips, fingerprint, self_signed, web_root, data_dir } => {
                        let _ = events.send(Event::Started {
                            port,
                            ips,
                            fingerprint,
                            self_signed,
                            web_root,
                            data_dir,
                        });
                    }
                    Down::PairingOpened { link } => { let _ = events.send(Event::PairingOpened { link: clean_link(&link) }); }
                    Down::UnpairedAll => { let _ = events.send(Event::UnpairedAll); }
                    Down::Notice(n) => { let _ = events.send(Event::Notice(clean_notice(n))); }
                    Down::Error(e) => { let _ = events.send(Event::Error(format!("network host: {}", clean(&e)))); }
                }
            }
            cmd = remote.next() => match cmd {
                Some(RemoteCmd::Video(_) | RemoteCmd::Audio(_)) if out.queued() > MAX_QUEUED => {
                    tracing::debug!("the network process is behind; dropping media");
                }
                Some(RemoteCmd::Video(f)) => out.send(frame(
                    &Up::Video {
                        display: f.display,
                        codec: f.codec,
                        frame_index: f.frame_index,
                        keyframe: f.keyframe,
                        recovers_from: f.recovers_from,
                        capture_age_us: age(f.capture_time),
                        encode_age_us: age(f.encode_done_time),
                        width: f.width,
                        height: f.height,
                    },
                    f.data,
                )),
                Some(RemoteCmd::Audio(p)) => out.send(frame(
                    &Up::Audio {
                        sequence: p.sequence,
                        capture_age_us: age(p.capture_time),
                        sample_rate: p.sample_rate,
                        channels: p.channels,
                    },
                    p.data,
                )),
                Some(RemoteCmd::Data { channel, binary, data, reply }) => {
                    next_id += 1;
                    replies.insert(next_id, reply);
                    out.send(frame(&Up::Data { id: next_id, channel, binary }, data));
                }
                Some(RemoteCmd::MaxBitrate(bps)) => send(&Up::MaxBitrate(bps), &[]),
                Some(RemoteCmd::VideoSink(set)) => send(&Up::VideoSink(set), &[]),
                None => break Ok(()),
            },
            cmd = commands.recv() => match cmd {
                Some(Command::OpenPairing) => send(&Up::Pairing(PairingCommand::Open), &[]),
                Some(Command::DropSession) => send(&Up::Pairing(PairingCommand::DropSession), &[]),
                Some(Command::UnpairAll) => send(&Up::Pairing(PairingCommand::UnpairAll), &[]),
                Some(Command::ShareFiles(paths)) => {
                    let _ = files.send(FileCommand::Share(paths)).await;
                }
                Some(Command::StopSharing(id)) => {
                    let _ = files.send(FileCommand::Stop(id)).await;
                }
                Some(Command::Quit) | None => break Ok(()),
            },
        }
    };
    // Media first (it puts back a changed resolution), then the link: the
    // network process ends the session and stops once it closes.
    let _ = media_stop.send(());
    let _ = media_task.await;
    drop(remote);
    drop(transport);
    drop(out);
    let _ = tokio::time::timeout(Duration::from_secs(1), writer).await;
    result
}

/// Text from the network process, fit for the administrator's console: no
/// control or invisible characters (which could rewrite what the console
/// shows), and not too long.
/// The pairing link's secret goes into a URL the host shows: base64url
/// only, whatever the network host sent.
fn clean_link(link: &str) -> String {
    link.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .take(64)
        .collect()
}

fn clean(text: &str) -> String {
    const MAX: usize = 300;
    let mut out: String = text
        .chars()
        .filter(|c| ws_proto::names::printable(*c))
        .take(MAX)
        .collect();
    if text.chars().count() > MAX {
        out.push('…');
    }
    out
}

fn clean_notice(n: Notice) -> Notice {
    match n {
        Notice::PairingCode {
            code,
            device_name,
            expires_in_secs,
        } => Notice::PairingCode {
            code: clean(&code),
            device_name: clean(&device_name),
            expires_in_secs,
        },
        Notice::Paired { device_name } => Notice::Paired {
            device_name: clean(&device_name),
        },
        Notice::SessionStarted { device_name } => Notice::SessionStarted {
            device_name: clean(&device_name),
        },
        Notice::SessionEnded { device_name } => Notice::SessionEnded {
            device_name: clean(&device_name),
        },
        Notice::Tunnel { url } => Notice::Tunnel {
            url: url.map(|u| clean(&u)),
        },
        Notice::Warning(w) => Notice::Warning(format!("network host: {}", clean(&w))),
        n @ (Notice::PairingFailed { .. } | Notice::PairingLocked) => n,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backlog_sends_are_spaced_and_never_stale() {
        use tokio::time::Instant;
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut r = StateReporter::new();
        // Connect: sent at once.
        assert_eq!(r.check(t0, true, ms(0), true), Some((true, ms(0))));
        // A change after a quiet spell: at once.
        let t1 = t0 + ms(100);
        assert_eq!(r.check(t1, true, ms(10), true), Some((true, ms(10))));
        // Another inside the spacing is held, not sent, and due at the end.
        assert_eq!(r.check(t1 + ms(1), true, ms(20), true), None);
        assert_eq!(r.due(), Some(t1 + BACKLOG_SPACING));
        // Rapid oscillation stays held; the send carries the current value.
        assert_eq!(r.check(t1 + ms(2), true, ms(30), true), None);
        assert_eq!(
            r.check(t1 + BACKLOG_SPACING, true, ms(40), true),
            Some((true, ms(40)))
        );
        assert_eq!(r.due(), None);
        // A held change that reversed sends nothing and clears the hold.
        let t2 = t1 + ms(50);
        assert_eq!(r.check(t2, true, ms(60), true), Some((true, ms(60))));
        assert_eq!(r.check(t2 + ms(1), true, ms(70), true), None);
        assert_eq!(r.check(t2 + BACKLOG_SPACING, true, ms(60), true), None);
        assert_eq!(r.due(), None);
        // A connection change inside the spacing is not held.
        assert_eq!(
            r.check(t2 + BACKLOG_SPACING + ms(1), true, ms(70), true),
            Some((true, ms(70)))
        );
        assert_eq!(
            r.check(t2 + BACKLOG_SPACING + ms(2), false, ms(70), true),
            Some((false, Duration::ZERO))
        );
    }

    #[test]
    fn only_a_connection_change_or_a_full_step_is_sent() {
        let base = Duration::from_millis(10);
        let live = (true, base);
        // Steady, and moves smaller than the step: nothing.
        assert_eq!(state_change(live, true, base), None);
        assert_eq!(state_change(live, true, base + BACKLOG_STEP / 2), None);
        assert_eq!(state_change(live, true, base - BACKLOG_STEP / 2), None);
        // A full step either way is sent, carrying the new value.
        let up = base + BACKLOG_STEP;
        assert_eq!(state_change(live, true, up), Some((true, up)));
        let down = base - BACKLOG_STEP;
        assert_eq!(state_change(live, true, down), Some((true, down)));
        // A connection change is sent even at the same backlog, and a
        // disconnected link reports no backlog.
        assert_eq!(
            state_change(live, false, base),
            Some((false, Duration::ZERO))
        );
        assert_eq!(state_change((false, Duration::ZERO), false, base), None);
    }

    /// A frame as `spawn_writer` puts it on the wire.
    fn wire(f: &Frame) -> Vec<u8> {
        let mut out = f.head.clone();
        out.extend_from_slice(f.payload.as_ref().as_ref());
        out
    }

    fn data_msg() -> Up {
        Up::Data {
            id: 7,
            channel: channels::FILE.into(),
            binary: true,
        }
    }

    #[tokio::test]
    async fn frames_have_the_documented_bytes_and_round_trip() {
        // Written out by hand, not derived from the code under test: the two
        // sides of the link can be different builds, so this layout must not
        // drift. len 23 = 4 + 16 header bytes + 3 payload bytes.
        let expected: &[u8] = b"\x17\0\0\0\x10\0\0\0{\"MaxBitrate\":5}abc";
        assert_eq!(expected.len(), 4 + 23);
        let f = frame(&Up::MaxBitrate(5), b"abc");
        assert_eq!(wire(&f), expected);
        assert_eq!(f.len(), expected.len());

        let (got, payload) = read_frame::<Up>(&mut std::io::Cursor::new(expected), MAX_DOWN)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(got, Up::MaxBitrate(5)));
        assert_eq!(payload, b"abc");
    }

    #[tokio::test]
    async fn empty_and_large_payloads_round_trip() {
        let (mut a, mut b) = tokio::io::duplex(1 << 10);
        let big: Vec<u8> = (0..300_000u32).map(|i| i as u8).collect();
        let writer = {
            let big = big.clone();
            tokio::spawn(async move {
                a.write_all(&wire(&frame(&data_msg(), Vec::new())))
                    .await
                    .unwrap();
                a.write_all(&wire(&frame(&data_msg(), big))).await.unwrap();
            })
        };
        let (_, empty) = read_frame::<Up>(&mut b, MAX_DOWN).await.unwrap().unwrap();
        assert!(empty.is_empty());
        let (_, payload) = read_frame::<Up>(&mut b, MAX_DOWN).await.unwrap().unwrap();
        assert_eq!(payload, big);
        writer.await.unwrap();
    }

    async fn read(bytes: &[u8], max: usize) -> anyhow::Result<Option<(Up, Vec<u8>)>> {
        read_frame::<Up>(&mut std::io::Cursor::new(bytes.to_vec()), max).await
    }

    #[tokio::test]
    async fn bad_frames_are_refused() {
        let err = |r: anyhow::Result<Option<(Up, Vec<u8>)>>| format!("{:#}", r.unwrap_err());
        assert_eq!(
            err(read(&3u32.to_le_bytes(), 100).await),
            "bad frame length 3"
        );
        assert_eq!(
            err(read(&((MAX_DOWN as u32) + 1).to_le_bytes(), MAX_DOWN).await),
            format!("bad frame length {}", MAX_DOWN + 1)
        );
        // A header longer than its frame.
        let mut bytes = 8u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(&100u32.to_le_bytes());
        bytes.extend_from_slice(b"{}{}");
        assert_eq!(err(read(&bytes, 100).await), "bad header length 100");
        // A header that is not JSON for the type.
        let mut bytes = 8u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(&4u32.to_le_bytes());
        bytes.extend_from_slice(b"{}{}");
        assert!(err(read(&bytes, 100).await).starts_with("bad header:"));
    }

    #[tokio::test]
    async fn end_of_stream_is_clean_only_between_frames() {
        assert!(read(&[], 100).await.unwrap().is_none());
        // Stopping inside the length itself is the same end of stream.
        assert!(read(&[0, 1], 100).await.unwrap().is_none());
        let bytes = wire(&frame(&data_msg(), b"payload"));
        for cut in [5, 9, bytes.len() - 10, bytes.len() - 1] {
            assert!(read(&bytes[..cut], MAX_DOWN).await.is_err(), "cut at {cut}");
        }
    }

    #[test]
    fn reports_become_the_same_events() {
        let stats = TransportStats {
            rtt: Some(Duration::from_millis(12)),
            egress_loss: Some(0.5),
            bwe: Some(9_000_000),
            bytes_tx: 77,
            trouble: Trouble {
                browser_keyframe_requests: 1,
                late_frames: 2,
                lost_frames: 3,
                send_drops: 4,
                recovered: 5,
            },
        };
        let events = [
            TransportEvent::Connected,
            TransportEvent::FramesLost(9),
            TransportEvent::ChannelOpen(channels::FILE),
            TransportEvent::Data {
                channel: channels::CONTROL,
                binary: false,
                data: b"{}".to_vec(),
            },
            TransportEvent::Stats(stats),
        ];
        for ev in events {
            let (report, payload) = report(ev.clone());
            let wire = wire(&frame(&Down::Transport(report), payload));
            let header_len = u32::from_le_bytes(wire[4..8].try_into().unwrap()) as usize;
            let Down::Transport(back) = serde_json::from_slice(&wire[8..8 + header_len]).unwrap()
            else {
                panic!()
            };
            assert_eq!(event(back, wire[8 + header_len..].to_vec()), Some(ev));
        }
        assert_eq!(event(Report::ChannelOpen("other".into()), Vec::new()), None);
    }
}
