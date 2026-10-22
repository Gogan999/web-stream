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
//! as it treats a browser: messages are size-limited, carry no paths, and
//! input and control messages go through the same parsing as before.
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

/// The largest frame either side accepts: well above any keyframe.
const MAX_FRAME: usize = 64 << 20;
/// How often the network side checks the connection state and send
/// backlog, which the encoder reads to hold frames back.
const STATE_POLL: Duration = Duration::from_millis(5);
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
    PairingOpened,
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

/// One frame, ready to write.
fn frame<H: Serialize>(header: &H, payload: &[u8]) -> Vec<u8> {
    let header = serde_json::to_vec(header).expect("bridge messages serialize");
    let len = 4 + header.len() + payload.len();
    let mut out = Vec::with_capacity(4 + len);
    out.extend_from_slice(&(len as u32).to_le_bytes());
    out.extend_from_slice(&(header.len() as u32).to_le_bytes());
    out.extend_from_slice(&header);
    out.extend_from_slice(payload);
    out
}

/// The next frame's header and payload; `None` at a clean end of stream.
async fn read_frame<H: DeserializeOwned>(
    read: &mut (impl AsyncRead + Unpin),
) -> anyhow::Result<Option<(H, Vec<u8>)>> {
    let mut len = [0u8; 4];
    match read.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_le_bytes(len) as usize;
    if !(4..=MAX_FRAME).contains(&len) {
        anyhow::bail!("bad frame length {len}");
    }
    let mut buf = vec![0u8; len];
    read.read_exact(&mut buf).await?;
    let header_len = u32::from_le_bytes(buf[..4].try_into().expect("4 bytes")) as usize;
    if header_len > len - 4 {
        anyhow::bail!("bad header length {header_len}");
    }
    let header = serde_json::from_slice(&buf[4..4 + header_len]).context("bad header")?;
    buf.drain(..4 + header_len);
    Ok(Some((header, buf)))
}

/// Reads frames into a channel until the stream ends or breaks; the
/// channel then closes. Never waits on the other direction, so neither
/// side can block the other.
fn spawn_reader<H: DeserializeOwned + Send + 'static>(
    mut read: impl AsyncRead + Unpin + Send + 'static,
) -> mpsc::UnboundedReceiver<(H, Vec<u8>)> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            match read_frame(&mut read).await {
                Ok(Some(msg)) => {
                    if tx.send(msg).is_err() {
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
    tx: mpsc::UnboundedSender<Vec<u8>>,
    queued: Arc<AtomicUsize>,
}

impl Out {
    fn send(&self, frame: Vec<u8>) {
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
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let queued = Arc::new(AtomicUsize::new(0));
    let out = Out {
        tx,
        queued: queued.clone(),
    };
    let task = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            let result = write.write_all(&frame).await;
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
    let mut incoming = spawn_reader::<Up>(read);
    let (out, writer) = spawn_writer(write);
    let send = |msg: &Down, payload: &[u8]| {
        out.send(frame(msg, payload));
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
                out.send(frame(&Down::DataSent { id, result }, &[]));
            }
        }
    });

    let mut transitions = 0u64;
    let mut state = (false, Duration::ZERO);
    let mut poll = tokio::time::interval(STATE_POLL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let result = loop {
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
                            out.send(frame(&Down::Transport(report), &[]));
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
                send(&Down::Transport(report), &payload);
                if transition {
                    // Input goes across as it arrives, after the event that
                    // says which connection it belongs to.
                    transitions += 1;
                    let out = out.clone();
                    transport.set_input_sink(
                        Box::new(move |data| {
                            out.send(frame(&Down::Transport(Report::Input), data));
                        }),
                        transitions,
                    );
                }
            }
            _ = poll.tick() => {
                let connected = transport.is_connected();
                let backlog = if connected { transport.send_backlog().get() } else { Duration::ZERO };
                if connected != state.0 || backlog.abs_diff(state.1) >= BACKLOG_STEP {
                    state = (connected, backlog);
                    send(&Down::State { connected, backlog_us: micros(backlog) }, &[]);
                }
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
                Ok(()) => Down::PairingOpened,
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
    let mut incoming = spawn_reader::<Down>(read);
    let (out, writer) = spawn_writer(write);
    let send = |msg: &Up, payload: &[u8]| {
        out.send(frame(msg, payload));
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
                    Down::PairingOpened => { let _ = events.send(Event::PairingOpened); }
                    Down::UnpairedAll => { let _ = events.send(Event::UnpairedAll); }
                    Down::Notice(n) => { let _ = events.send(Event::Notice(n)); }
                    Down::Error(e) => { let _ = events.send(Event::Error(e)); }
                }
            }
            cmd = remote.next() => match cmd {
                Some(RemoteCmd::Video(_) | RemoteCmd::Audio(_)) if out.queued() > MAX_QUEUED => {
                    tracing::debug!("the network process is behind; dropping media");
                }
                Some(RemoteCmd::Video(f)) => send(
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
                    &f.data,
                ),
                Some(RemoteCmd::Audio(p)) => send(
                    &Up::Audio {
                        sequence: p.sequence,
                        capture_age_us: age(p.capture_time),
                        sample_rate: p.sample_rate,
                        channels: p.channels,
                    },
                    &p.data,
                ),
                Some(RemoteCmd::Data { channel, binary, data, reply }) => {
                    next_id += 1;
                    replies.insert(next_id, reply);
                    send(&Up::Data { id: next_id, channel, binary }, &data);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_round_trip_and_bad_ones_are_refused() {
        let (mut a, mut b) = tokio::io::duplex(1 << 16);
        let msg = Up::Data {
            id: 7,
            channel: channels::FILE.into(),
            binary: true,
        };
        a.write_all(&frame(&msg, b"payload")).await.unwrap();
        let (got, payload) = read_frame::<Up>(&mut b).await.unwrap().unwrap();
        assert!(matches!(
            got,
            Up::Data {
                id: 7,
                binary: true,
                ..
            }
        ));
        assert_eq!(payload, b"payload");

        a.write_all(&((MAX_FRAME as u32) + 1).to_le_bytes())
            .await
            .unwrap();
        assert!(read_frame::<Up>(&mut b).await.is_err());

        let (mut a, mut b) = tokio::io::duplex(1 << 16);
        // A header longer than its frame.
        a.write_all(&8u32.to_le_bytes()).await.unwrap();
        a.write_all(&100u32.to_le_bytes()).await.unwrap();
        a.write_all(b"{}{}").await.unwrap();
        assert!(read_frame::<Up>(&mut b).await.is_err());
        drop(a);
        assert!(read_frame::<Up>(&mut b).await.unwrap().is_none());
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
            let wire = frame(&Down::Transport(report), &payload);
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
