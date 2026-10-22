//! A [`Transport`] whose peer connection runs in another process.
//!
//! The Windows service keeps everything that parses what comes off the
//! network (signaling, DTLS, SRTP, SCTP) in a process with almost no rights,
//! and capture, encoding and input in a SYSTEM one. The SYSTEM side uses a
//! [`Transport::remote`] handle exactly as it would a real one; a bridge in
//! the host carries what it is asked to send ([`RemoteCmd`]) to the real
//! transport, and feeds what that transport reports back in with
//! [`Remote::event`] and [`Remote::set_state`].
//!
//! Keyframe requests, repairs and input go to the sinks set here, and are
//! held for them, just as the real transport does: the other side forwards
//! all of them, in order with the connection events, and this side decides.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use ws_proto::{AudioPacket, EncodedFrame, channels};

use crate::{Cmd, Ends, Error, Events, Shared, Transport, TransportEvent, VideoSignal};

/// What the SYSTEM side asked its [`Transport`] to do, for the real one.
#[derive(Debug)]
pub enum RemoteCmd {
    Video(EncodedFrame),
    Audio(AudioPacket),
    Data {
        channel: String,
        binary: bool,
        data: Vec<u8>,
        reply: oneshot::Sender<Result<bool, Error>>,
    },
    MaxBitrate(u64),
    /// [`Transport::set_video_sink`] (true) or
    /// [`Transport::clear_video_sink`] was called.
    VideoSink(bool),
}

/// The run loop end of a [`Transport::remote`] handle.
pub struct Remote {
    cmds: mpsc::Receiver<Cmd>,
    audio: mpsc::Receiver<AudioPacket>,
    shared: Arc<Shared>,
    /// Frames taken from the queue, not yet handed out.
    frames: VecDeque<EncodedFrame>,
    /// Dropped with this, which ends [`Transport::shutdown`]'s wait.
    _exit: oneshot::Sender<()>,
}

impl Transport {
    /// A handle for a transport in another process: what it is asked to do
    /// comes out of the returned [`Remote`], and what that process reports
    /// goes into it. Negotiation is the other process's job, so
    /// [`Transport::offer`] and [`Transport::accept_answer`] fail here.
    pub fn remote() -> (Transport, Events, Remote) {
        let (transport, events, ends) = Transport::new();
        let Ends {
            cmds,
            audio,
            shared,
            exit,
        } = ends;
        let remote = Remote {
            cmds,
            audio,
            shared,
            frames: VecDeque::new(),
            _exit: exit,
        };
        (transport, events, remote)
    }
}

impl Remote {
    /// The next thing to send to the real transport, in order; `None` once
    /// every handle is gone or [`Transport::shutdown`] was called.
    pub async fn next(&mut self) -> Option<RemoteCmd> {
        loop {
            if let Some(frame) = self.frames.pop_front() {
                return Some(RemoteCmd::Video(frame));
            }
            tokio::select! {
                cmd = self.cmds.recv() => match cmd? {
                    Cmd::Shutdown => return None,
                    Cmd::Offer(_, reply) => {
                        let _ = reply.send(Err(Error::Negotiation(NEGOTIATION_ELSEWHERE.into())));
                    }
                    Cmd::Answer(_, reply) => {
                        let _ = reply.send(Err(Error::Negotiation(NEGOTIATION_ELSEWHERE.into())));
                    }
                    // Signaling's, in the other process.
                    Cmd::Candidate(_) | Cmd::Close => {}
                    Cmd::MaxBitrate(bps) => return Some(RemoteCmd::MaxBitrate(bps)),
                    Cmd::VideoSink(set) => return Some(RemoteCmd::VideoSink(set)),
                    Cmd::Data { channel, binary, data, reply } => {
                        return Some(RemoteCmd::Data { channel, binary, data, reply });
                    }
                },
                Some(packet) = self.audio.recv() => return Some(RemoteCmd::Audio(packet)),
                _ = self.shared.notify.notified() => {
                    self.frames = std::mem::take(&mut *self.shared.frames.lock().expect("video queue lock"));
                }
            }
        }
    }

    /// Something the real transport reported (input and keyframe requests
    /// included: send those as the events and channel messages they would
    /// be on a [`Transport`] with no sinks).
    pub fn event(&self, ev: TransportEvent) {
        let shared = &self.shared;
        match ev {
            TransportEvent::Connected => {
                shared.connected.store(true, Ordering::Relaxed);
                shared.emit(TransportEvent::Connected);
            }
            TransportEvent::Disconnected => {
                shared.connected.store(false, Ordering::Relaxed);
                shared.backlog.set(Duration::ZERO);
                shared.emit(TransportEvent::Disconnected);
            }
            TransportEvent::KeyframeNeeded => shared.signal_video(VideoSignal::Keyframe),
            TransportEvent::FramesLost(from) => shared.signal_video(VideoSignal::FramesLost(from)),
            TransportEvent::Data { channel, data, .. }
                if channel == channels::INPUT || channel == channels::INPUT_MOTION =>
            {
                shared.input_route().input(data)
            }
            ev => shared.emit(ev),
        }
    }

    /// Whether the peer connection is up, and how far behind its video send
    /// queue is, as the real transport last said.
    pub fn set_state(&self, connected: bool, backlog: Duration) {
        self.shared.connected.store(connected, Ordering::Relaxed);
        self.shared
            .backlog
            .set(if connected { backlog } else { Duration::ZERO });
    }
}

const NEGOTIATION_ELSEWHERE: &str = "the peer connection is negotiated in the network process";

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[tokio::test]
    async fn sends_come_out_in_order_and_reports_go_in_like_the_real_ones() {
        let (transport, mut events, mut remote) = Transport::remote();
        transport.set_max_bitrate(5_000_000);
        let frame = |i| EncodedFrame {
            display: ws_proto::DisplayId(0),
            codec: ws_proto::VideoCodec::H264,
            frame_index: i,
            keyframe: i == 0,
            recovers_from: None,
            capture_time: 0,
            encode_done_time: 0,
            width: 1,
            height: 1,
            data: vec![1, 2, 3].into(),
        };
        transport.send_video(frame(0));
        transport.send_video(frame(1));
        // Frames in order; commands among them.
        let mut frames = Vec::new();
        for _ in 0..3 {
            match remote.next().await {
                Some(RemoteCmd::Video(f)) => frames.push(f.frame_index),
                Some(RemoteCmd::MaxBitrate(bps)) => assert_eq!(bps, 5_000_000),
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(frames, [0, 1]);
        let send = tokio::spawn({
            let transport = transport.clone();
            async move {
                transport
                    .send_data(channels::CONTROL, false, b"{}".to_vec())
                    .await
            }
        });
        match remote.next().await {
            Some(RemoteCmd::Data { channel, reply, .. }) => {
                assert_eq!(channel, channels::CONTROL);
                reply.send(Ok(true)).unwrap();
            }
            other => panic!("{other:?}"),
        }
        assert!(send.await.unwrap().unwrap());
        // Negotiation is the other process's.
        let (offer, _) = tokio::join!(
            transport.offer(),
            tokio::time::timeout(Duration::from_millis(100), remote.next())
        );
        assert!(offer.is_err());

        // Input waits for this connection's sink; repairs go to the video sink.
        let got = Arc::new(Mutex::new(Vec::new()));
        remote.event(TransportEvent::Connected);
        assert!(transport.is_connected());
        let input = |d: u8| TransportEvent::Data {
            channel: channels::INPUT,
            binary: true,
            data: vec![d],
        };
        remote.event(input(7));
        let sink = got.clone();
        assert!(transport.set_input_sink(Box::new(move |d| sink.lock().unwrap().push(d[0])), 1));
        remote.event(input(8));
        assert_eq!(*got.lock().unwrap(), [7, 8]);
        let signals = Arc::new(Mutex::new(Vec::new()));
        let sink = signals.clone();
        transport.set_video_sink(Box::new(move |s| sink.lock().unwrap().push(s)));
        assert!(matches!(
            remote.next().await,
            Some(RemoteCmd::VideoSink(true))
        ));
        remote.event(TransportEvent::FramesLost(4));
        assert_eq!(*signals.lock().unwrap(), [VideoSignal::FramesLost(4)]);
        remote.set_state(true, Duration::from_millis(30));
        assert_eq!(transport.send_backlog().get(), Duration::from_millis(30));
        remote.event(TransportEvent::Disconnected);
        assert!(!transport.is_connected());
        assert_eq!(transport.send_backlog().get(), Duration::ZERO);
        assert!(matches!(
            events.recv().await,
            Some(TransportEvent::Connected)
        ));
        assert!(matches!(
            events.recv().await,
            Some(TransportEvent::Disconnected)
        ));

        // Shutting down ends the run loop end, and waits for it to go.
        let stop = tokio::spawn(async move { transport.shutdown().await });
        assert!(remote.next().await.is_none());
        drop(remote);
        stop.await.unwrap();
    }
}
