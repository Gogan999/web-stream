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

pub struct Session {
    rtc: Rtc,
    pending: Option<SdpPendingOffer>,
    video: Mid,
    audio: Mid,
    channels: HashMap<ChannelId, &'static str>,
    connected: bool,
    max_resend_age: Duration,
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
            max_resend_age: config.max_resend_age,
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
        Ok(())
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

    /// Returns false if the frame could not be sent (not connected yet, or a
    /// codec the browser did not accept).
    pub fn write_video(&mut self, frame: &EncodedFrame, now: Instant) -> bool {
        if !self.connected {
            return false;
        }
        let Some(writer) = self.rtc.writer(self.video) else {
            return false;
        };
        let Some(pt) = video_pt(writer.payload_params(), frame.codec) else {
            if self.unsupported_codec != Some(frame.codec) {
                tracing::warn!(codec = ?frame.codec, "browser did not negotiate this codec; dropping video");
                self.unsupported_codec = Some(frame.codec);
            }
            return false;
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
            Ok(()) => true,
            Err(e) => {
                tracing::debug!("video write: {e}");
                false
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
                    out.push(Step::Event(TransportEvent::Disconnected));
                    return Instant::now() + Duration::from_secs(1);
                }
            }
        }
    }

    fn translate(&mut self, event: Event) -> Option<TransportEvent> {
        Some(match event {
            Event::Connected => {
                self.connected = true;
                // The first frame the browser sees must be decodable.
                return Some(TransportEvent::Connected);
            }
            Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                self.connected = false;
                TransportEvent::Disconnected
            }
            Event::KeyframeRequest(req) if req.mid == self.video => TransportEvent::KeyframeNeeded,
            Event::EgressBitrateEstimate(BweKind::Twcc { estimate, .. })
            | Event::EgressBitrateEstimate(BweKind::Remb { estimate, .. }) => {
                TransportEvent::TargetBitrate(estimate.as_u64())
            }
            Event::ChannelOpen(id, _) => TransportEvent::ChannelOpen(self.label(id)?),
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

/// Local ICE candidates for the bound sockets, plus server-reflexive ones for
/// public addresses that port-forward to them.
pub fn candidates(
    udp: &[SocketAddr],
    tcp: &[SocketAddr],
    public: &[std::net::IpAddr],
) -> Vec<Candidate> {
    let mut out = Vec::new();
    for addr in udp {
        if let Ok(c) = Candidate::host(*addr, "udp") {
            out.push(c);
        }
    }
    for addr in tcp {
        if let Ok(c) = Candidate::builder()
            .tcp()
            .host(*addr)
            .tcptype(TcpType::Passive)
            .build()
        {
            out.push(c);
        }
    }
    if let Some(base) = udp.first() {
        for ip in public {
            let addr = SocketAddr::new(*ip, base.port());
            if let Ok(c) = Candidate::server_reflexive(addr, *base, "udp") {
                out.push(c);
            }
        }
    }
    if let Some(base) = tcp.first() {
        for ip in public {
            let addr = SocketAddr::new(*ip, base.port());
            if let Ok(c) = Candidate::builder()
                .tcp()
                .server_reflexive(addr, *base)
                .tcptype(TcpType::Passive)
                .build()
            {
                out.push(c);
            }
        }
    }
    out
}
