//! One WebRTC peer connection to the browser, wrapping a str0m `Rtc`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use str0m::bwe::{Bitrate, BweKind};
use str0m::change::{SdpAnswer, SdpPendingOffer};
use str0m::channel::{ChannelConfig, ChannelId, Reliability};
use str0m::format::Codec;
use str0m::media::{Direction, Frequency, MediaKind, MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive, TcpType};
use str0m::rtp::Extension;
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcConfig};
use ws_proto::{AudioPacket, EncodedFrame, VideoCodec, channels};

use crate::{Error, TransportConfig, TransportEvent, TransportStats};

/// RTP header extension id for playout-delay. Any free id works; str0m's
/// standard map leaves 5 unused.
const PLAYOUT_DELAY_EXT_ID: u8 = 5;
/// The file channel may hold at most this much unsent data, so transfers
/// leave room in the shared SCTP buffer (128 KiB) for control and input.
const FILE_MAX_BUFFERED: usize = 64 * 1024;
/// When the file channel drains below this, [`TransportEvent::ChannelLow`].
const FILE_LOW: usize = 16 * 1024;

/// What happened to a frame handed to [`Session::write_video`].
#[derive(Debug, PartialEq, Eq)]
pub enum VideoWrite {
    Sent,
    /// The send queue was too far behind, so the frame was dropped; the
    /// encoder must send a keyframe.
    Late,
    /// An earlier frame this one builds on never went out, so the browser
    /// would decode it against the wrong picture (smeared, ghosted motion).
    /// Dropped; the encoder must send a keyframe.
    Broken,
    /// Not connected, or a codec the browser did not accept.
    Skipped,
}

pub struct Session {
    rtc: Rtc,
    pending: Option<SdpPendingOffer>,
    video: Mid,
    audio: Mid,
    channels: HashMap<ChannelId, &'static str>,
    /// Media can flow: DTLS is up and ICE is not disconnected.
    connected: bool,
    dtls_up: bool,
    /// Disconnected has been reported since the last Connected.
    down: bool,
    max_resend_age: Duration,
    max_send_delay: Duration,
    max_bitrate_bps: u64,
    /// When the last keyframe went into the send queue.
    keyframe_at: Option<Instant>,
    /// `frame_index` a non-keyframe must have to be sent: the one right
    /// after the last frame sent. `None` until a keyframe goes out.
    next_index: Option<u64>,
    /// Codec of the last frame we could not send, to log once.
    unsupported_codec: Option<VideoCodec>,
}

pub enum Step {
    Transmit {
        proto: Protocol,
        source: SocketAddr,
        destination: SocketAddr,
        data: Vec<u8>,
    },
    Event(TransportEvent),
}

impl Session {
    /// Build a peer connection and the offer for the browser.
    pub fn offer(
        config: &TransportConfig,
        candidates: &[Candidate],
        now: Instant,
    ) -> Result<(Session, String), Error> {
        let rtc_config = RtcConfig::new()
            .set_ice_lite(true)
            .clear_codecs()
            .enable_h264(true)
            .enable_h265(true)
            .enable_av1(true)
            .enable_opus(true, false)
            .set_extension(PLAYOUT_DELAY_EXT_ID, Extension::PlayoutDelay)
            .enable_bwe(Some(Bitrate::bps(config.initial_bitrate_bps)))
            .set_stats_interval(Some(Duration::from_secs(1)));
        let mut rtc = rtc_config.build(now);
        for c in candidates {
            rtc.add_local_candidate(c.clone());
        }

        let mut change = rtc.sdp_api();
        let video = change.add_media(MediaKind::Video, Direction::SendOnly, None, None, None);
        let audio = change.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
        let mut chans = HashMap::new();
        for (label, ordered, reliability) in [
            (channels::CONTROL, true, Reliability::Reliable),
            (channels::INPUT, true, Reliability::Reliable),
            (
                channels::INPUT_MOTION,
                false,
                Reliability::MaxRetransmits { retransmits: 0 },
            ),
            (channels::FILE, true, Reliability::Reliable),
        ] {
            let id = change.add_channel_with_config(ChannelConfig {
                label: label.to_string(),
                ordered,
                reliability,
                ..Default::default()
            });
            chans.insert(id, label);
        }
        let (offer, pending) = change
            .apply()
            .ok_or(Error::Negotiation("empty offer".into()))?;

        let session = Session {
            rtc,
            pending: Some(pending),
            video,
            audio,
            channels: chans,
            connected: false,
            dtls_up: false,
            down: false,
            max_resend_age: config.max_resend_age,
            max_send_delay: config.max_send_delay,
            max_bitrate_bps: config.max_bitrate_bps,
            keyframe_at: None,
            next_index: None,
            unsupported_codec: None,
        };
        Ok((session, offer.to_sdp_string()))
    }

    pub fn accept_answer(&mut self, sdp: &str) -> Result<(), Error> {
        let pending = self
            .pending
            .take()
            .ok_or(Error::Negotiation("no offer outstanding".into()))?;
        let answer =
            SdpAnswer::from_sdp_string(sdp).map_err(|e| Error::Negotiation(e.to_string()))?;
        self.rtc
            .sdp_api()
            .accept_answer(pending, answer)
            .map_err(|e| Error::Negotiation(e.to_string()))?;
        // Answer a NACK only while the packet is fresh enough for the resend
        // to still be useful; older losses are repaired by a keyframe.
        if let Some(tx) = self.rtc.direct_api().stream_tx_by_mid(self.video, None) {
            tx.set_rtx_cache(2048, self.max_resend_age, Some(0.15));
        }
        // Probe up towards what the encoder can use, not just the start rate.
        self.set_max_bitrate(self.max_bitrate_bps);
        Ok(())
    }

    pub fn set_max_bitrate(&mut self, bps: u64) {
        self.max_bitrate_bps = bps;
        if self.pending.is_none() {
            self.rtc.bwe().set_desired_bitrate(Bitrate::bps(bps));
        }
    }

    /// The local ICE ufrag, which the browser's checks must name.
    pub fn local_ufrag(&mut self) -> String {
        self.rtc.direct_api().local_ice_credentials().ufrag
    }

    /// Mark the session down; true if that is news (Disconnected not yet
    /// reported).
    pub fn mark_down(&mut self) -> bool {
        self.connected = false;
        !std::mem::replace(&mut self.down, true)
    }

    pub fn is_connected(&self) -> bool {
        self.connected
    }

    pub fn add_remote_candidate(&mut self, candidate: &str) {
        let c = candidate.strip_prefix("a=").unwrap_or(candidate);
        match Candidate::from_sdp_string(c) {
            Ok(c) => self.rtc.add_remote_candidate(c),
            Err(e) => tracing::debug!("ignoring remote candidate {candidate:?}: {e}"),
        }
    }

    pub fn is_alive(&self) -> bool {
        self.rtc.is_alive()
    }

    pub fn accepts(
        &self,
        proto: Protocol,
        source: SocketAddr,
        destination: SocketAddr,
        data: &[u8],
    ) -> bool {
        let Ok(contents) = data.try_into() else {
            return false;
        };
        self.rtc.accepts(&Input::Receive(
            Instant::now(),
            Receive {
                proto,
                source,
                destination,
                contents,
            },
        ))
    }

    pub fn receive(
        &mut self,
        now: Instant,
        proto: Protocol,
        source: SocketAddr,
        destination: SocketAddr,
        data: &[u8],
    ) {
        let Ok(contents) = data.try_into() else {
            return;
        };
        if let Err(e) = self.rtc.handle_input(Input::Receive(
            now,
            Receive {
                proto,
                source,
                destination,
                contents,
            },
        )) {
            tracing::debug!("rtc input: {e}");
        }
    }

    pub fn timeout(&mut self, now: Instant) {
        if let Err(e) = self.rtc.handle_input(Input::Timeout(now)) {
            tracing::debug!("rtc timeout: {e}");
        }
    }

    /// How long the oldest unsent video packet has waited in the pacer, if
    /// that backlog is from frames after the last keyframe. A keyframe is
    /// big and is allowed to drain; the frames right behind it need it.
    fn video_backlog(&mut self, now: Instant) -> Duration {
        let keyframe_at = self.keyframe_at;
        let mut api = self.rtc.direct_api();
        let Some(tx) = api.stream_tx_by_mid(self.video, None) else {
            return Duration::ZERO;
        };
        let Some(first_unsent) = tx.queue_info().and_then(|q| q.first_unsent()) else {
            return Duration::ZERO;
        };
        if keyframe_at.is_some_and(|k| first_unsent <= k) {
            return Duration::ZERO;
        }
        now.saturating_duration_since(first_unsent)
    }

    pub fn write_video(&mut self, frame: &EncodedFrame, now: Instant) -> VideoWrite {
        if !self.connected {
            return VideoWrite::Skipped;
        }
        // Every frame references the one before it, and the browser can't
        // tell one went missing (RTP sequence numbers stay continuous), so
        // after any gap only a keyframe may go out.
        if !follows(self.next_index, frame) {
            return VideoWrite::Broken;
        }
        // Drop rather than queue: a frame that would wait behind older ones
        // arrives late, and every frame after it would too.
        if !frame.keyframe && self.video_backlog(now) > self.max_send_delay {
            return VideoWrite::Late;
        }
        let Some(writer) = self.rtc.writer(self.video) else {
            return VideoWrite::Skipped;
        };
        let Some(pt) = video_pt(writer.payload_params(), frame.codec) else {
            if self.unsupported_codec != Some(frame.codec) {
                tracing::warn!(codec = ?frame.codec, "browser did not negotiate this codec; dropping video");
                self.unsupported_codec = Some(frame.codec);
            }
            return VideoWrite::Skipped;
        };
        let rtp_time = MediaTime::from_micros(frame.capture_time).rebase(Frequency::NINETY_KHZ);
        // Wallclock is when the image was captured: now, minus the time the
        // frame spent in the encoder.
        let in_pipeline =
            Duration::from_micros(frame.encode_done_time.saturating_sub(frame.capture_time));
        let wallclock = now.checked_sub(in_pipeline).unwrap_or(now);
        let zero = MediaTime::from_millis(0);
        match writer
            .playout_delay(zero, zero)
            .write(pt, wallclock, rtp_time, frame.data.to_vec())
        {
            Ok(()) => {
                if frame.keyframe {
                    self.keyframe_at = Some(now);
                }
                self.next_index = Some(frame.frame_index + 1);
                VideoWrite::Sent
            }
            Err(e) => {
                tracing::debug!("video write: {e}");
                VideoWrite::Skipped
            }
        }
    }

    pub fn write_audio(&mut self, packet: &AudioPacket, now: Instant) {
        if !self.connected {
            return;
        }
        let Some(writer) = self.rtc.writer(self.audio) else {
            return;
        };
        let Some(pt) = writer
            .payload_params()
            .find(|p| p.spec().codec == Codec::Opus)
            .map(|p| p.pt())
        else {
            return;
        };
        let rtp_time =
            MediaTime::from_micros(packet.capture_time).rebase(Frequency::FORTY_EIGHT_KHZ);
        if let Err(e) = writer.write(pt, now, rtp_time, packet.data.to_vec()) {
            tracing::debug!("audio write: {e}");
        }
    }

    /// `Ok(false)` when the channel's buffer is full; wait for
    /// [`TransportEvent::ChannelLow`].
    pub fn write_data(&mut self, label: &str, binary: bool, data: &[u8]) -> Result<bool, Error> {
        let id = self
            .channels
            .iter()
            .find_map(|(id, l)| (*l == label).then_some(*id))
            .ok_or_else(|| Error::UnknownChannel(label.to_string()))?;
        let mut ch = self
            .rtc
            .channel(id)
            .ok_or(Error::ChannelNotOpen(label.to_string()))?;
        if label == channels::FILE && ch.buffered_amount() + data.len() > FILE_MAX_BUFFERED {
            return Ok(false);
        }
        ch.write(binary, data)
            .map_err(|e| Error::Negotiation(e.to_string()))
    }

    /// Drain everything str0m has to say until it only has a timeout left.
    pub fn poll(&mut self, out: &mut Vec<Step>) -> Instant {
        loop {
            match self.rtc.poll_output() {
                Ok(Output::Timeout(t)) => return t,
                Ok(Output::Transmit(t)) => out.push(Step::Transmit {
                    proto: t.proto,
                    source: t.source,
                    destination: t.destination,
                    data: t.contents.to_vec(),
                }),
                Ok(Output::Event(e)) => {
                    if let Some(ev) = self.translate(e) {
                        out.push(Step::Event(ev));
                    }
                }
                Err(e) => {
                    tracing::warn!("rtc failed: {e}");
                    self.rtc.disconnect();
                    if self.mark_down() {
                        out.push(Step::Event(TransportEvent::Disconnected));
                    }
                    return Instant::now() + Duration::from_secs(1);
                }
            }
        }
    }

    fn translate(&mut self, event: Event) -> Option<TransportEvent> {
        Some(match event {
            Event::Connected => {
                self.dtls_up = true;
                self.connected = true;
                self.down = false;
                TransportEvent::Connected
            }
            Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                if !self.mark_down() {
                    return None;
                }
                TransportEvent::Disconnected
            }
            // ICE came back (consent restored, or a new pair) after a
            // disconnect; DTLS state survived it.
            Event::IceConnectionStateChange(
                IceConnectionState::Connected | IceConnectionState::Completed,
            ) if self.dtls_up && !self.connected => {
                self.connected = true;
                self.down = false;
                TransportEvent::Connected
            }
            Event::Closed => {
                if !self.mark_down() {
                    return None;
                }
                TransportEvent::Disconnected
            }
            Event::KeyframeRequest(req) if req.mid == self.video => TransportEvent::KeyframeNeeded,
            Event::EgressBitrateEstimate(BweKind::Twcc { estimate, .. })
            | Event::EgressBitrateEstimate(BweKind::Remb { estimate, .. }) => {
                TransportEvent::TargetBitrate(estimate.as_u64())
            }
            Event::ChannelOpen(id, _) => {
                let label = self.label(id)?;
                if label == channels::FILE {
                    if let Some(mut ch) = self.rtc.channel(id) {
                        ch.set_buffered_amount_low_threshold(FILE_LOW);
                    }
                }
                TransportEvent::ChannelOpen(label)
            }
            Event::ChannelClose(id) => TransportEvent::ChannelClosed(self.label(id)?),
            Event::ChannelBufferedAmountLow(id) => TransportEvent::ChannelLow(self.label(id)?),
            Event::ChannelData(d) => TransportEvent::Data {
                channel: self.label(d.id)?,
                binary: d.binary,
                data: d.data,
            },
            Event::MediaData(d) if d.mid == self.audio => TransportEvent::MicAudio(AudioPacket {
                direction: ws_proto::AudioDirection::Capture,
                sequence: **d.seq_range.end(),
                capture_time: d.time.rebase(Frequency::MICROS).numer(),
                sample_rate: 48_000,
                channels: d.params.spec().channels.unwrap_or(2),
                data: bytes::Bytes::copy_from_slice(&d.data),
            }),
            Event::PeerStats(s) => TransportEvent::Stats(TransportStats {
                rtt: s.rtt,
                egress_loss: s.egress_loss_fraction,
                bwe: s.bwe_tx.map(|b| b.as_u64()),
                bytes_tx: s.bytes_tx,
            }),
            _ => return None,
        })
    }

    fn label(&self, id: ChannelId) -> Option<&'static str> {
        self.channels.get(&id).copied()
    }
}

/// Pick the payload type for the encoder's codec. For H.264 prefer
/// packetization-mode 1, which is what NVENC/AMF/QSV slices need.
/// Whether `frame` can be decoded after the frames sent so far.
fn follows(next_index: Option<u64>, frame: &EncodedFrame) -> bool {
    frame.keyframe || next_index == Some(frame.frame_index)
}

fn video_pt<'a>(
    params: impl Iterator<Item = &'a str0m::format::PayloadParams>,
    codec: VideoCodec,
) -> Option<Pt> {
    let want = match codec {
        VideoCodec::H264 => Codec::H264,
        VideoCodec::H265 => Codec::H265,
        VideoCodec::Av1 => Codec::Av1,
    };
    let mut best: Option<(Pt, bool)> = None;
    for p in params {
        let spec = p.spec();
        if spec.codec != want {
            continue;
        }
        let preferred = want != Codec::H264 || spec.format.packetization_mode == Some(1);
        if best.is_none_or(|(_, b)| preferred && !b) {
            best = Some((p.pt(), preferred));
        }
    }
    best.map(|(pt, _)| pt)
}

/// Local ICE candidates for the bound sockets, plus candidates for public
/// addresses that port-forward to them.
///
/// The public ones are host candidates, not server-reflexive: str0m drops
/// non-host local candidates in ICE-lite mode, so a srflx one never reached
/// the offer. As host candidates they are only advertised; checks sent to
/// them arrive (after the router's port forward) on the bound address and
/// match its candidate there.
pub fn candidates(
    udp: &[SocketAddr],
    tcp: &[SocketAddr],
    public: &[std::net::IpAddr],
) -> Vec<Candidate> {
    let udp_host = |addr| Candidate::host(addr, "udp").ok();
    let tcp_host = |addr| {
        Candidate::builder()
            .tcp()
            .host(addr)
            .tcptype(TcpType::Passive)
            .build()
            .ok()
    };
    let mut out: Vec<Candidate> = udp.iter().filter_map(|a| udp_host(*a)).collect();
    out.extend(tcp.iter().filter_map(|a| tcp_host(*a)));
    if let Some(base) = udp.first() {
        out.extend(
            public
                .iter()
                .filter_map(|ip| udp_host(SocketAddr::new(*ip, base.port()))),
        );
    }
    if let Some(base) = tcp.first() {
        out.extend(
            public
                .iter()
                .filter_map(|ip| tcp_host(SocketAddr::new(*ip, base.port()))),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ws_proto::DisplayId;

    fn frame(frame_index: u64, keyframe: bool) -> EncodedFrame {
        EncodedFrame {
            display: DisplayId(0),
            codec: VideoCodec::H264,
            frame_index,
            keyframe,
            capture_time: 0,
            encode_done_time: 0,
            width: 1920,
            height: 1080,
            data: Default::default(),
        }
    }

    #[test]
    fn only_a_keyframe_follows_a_gap() {
        // Nothing sent yet: the browser needs a keyframe first.
        assert!(!follows(None, &frame(0, false)));
        assert!(follows(None, &frame(0, true)));
        // Frame 4 went out; 5 may follow, 6 (5 was dropped) may not.
        assert!(follows(Some(5), &frame(5, false)));
        assert!(!follows(Some(5), &frame(6, false)));
        assert!(follows(Some(5), &frame(9, true)));
        // A new encoder session starts over at 0 with a keyframe.
        assert!(follows(Some(500), &frame(0, true)));
    }
}
