//! Client microphone path: a small jitter buffer keyed by RTP sequence number
//! and an Opus decoder that the render thread pulls PCM from.
//!
//! Late packets are discarded rather than played late, and a backlog beyond
//! `max_packets` drops the oldest audio so the mic never drifts behind.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use opus::{Channels, Decoder};
use tracing::debug;

use crate::AudioError;
use crate::convert::CHANNELS;

#[derive(Debug, Clone, Copy)]
pub struct JitterConfig {
    /// Packets to collect before playback starts or resumes after a gap.
    pub target_packets: usize,
    /// Backlog beyond this drops the oldest packets.
    pub max_packets: usize,
}

impl Default for JitterConfig {
    fn default() -> Self {
        // Browsers send 20 ms mic packets, so this is 40 ms of cushion.
        Self {
            target_packets: 2,
            max_packets: 8,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JitterStats {
    pub received: u64,
    pub late: u64,
    pub duplicate: u64,
    pub overflow_dropped: u64,
    pub concealed: u64,
    pub underruns: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Pull {
    Packet(Vec<u8>),
    /// The next packet is missing but later ones arrived: conceal one packet.
    Lost,
    /// Nothing to play yet.
    Empty,
}

pub struct JitterBuffer {
    config: JitterConfig,
    packets: BTreeMap<u64, Vec<u8>>,
    /// Extended sequence number of the next packet to play.
    next: Option<u64>,
    highest: Option<u64>,
    playing: bool,
    stats: JitterStats,
}

impl JitterBuffer {
    pub fn new(config: JitterConfig) -> Self {
        Self {
            config,
            packets: BTreeMap::new(),
            next: None,
            highest: None,
            playing: false,
            stats: Default::default(),
        }
    }

    /// Unwraps a 16-bit RTP sequence number against the highest one seen.
    fn extend(&self, seq: u16) -> u64 {
        let Some(highest) = self.highest else {
            // Leave room below the first packet for reordered earlier ones.
            return (1 << 32) + seq as u64;
        };
        let delta = seq.wrapping_sub(highest as u16) as i16 as i64;
        (highest as i64 + delta) as u64
    }

    pub fn push(&mut self, seq: u16, payload: Vec<u8>) {
        self.stats.received += 1;
        let ext = self.extend(seq);
        if self.next.is_some_and(|next| ext < next) {
            self.stats.late += 1;
            return;
        }
        if self.packets.insert(ext, payload).is_some() {
            self.stats.duplicate += 1;
        }
        self.highest = Some(self.highest.map_or(ext, |h| h.max(ext)));
        while self.packets.len() > self.config.max_packets {
            self.packets.pop_first();
            self.stats.overflow_dropped += 1;
            self.next = self.packets.keys().next().copied();
        }
    }

    pub fn pull(&mut self) -> Pull {
        if !self.playing {
            if self.packets.len() < self.config.target_packets {
                return Pull::Empty;
            }
            self.playing = true;
            self.next = self.packets.keys().next().copied();
        }
        let Some(&first) = self.packets.keys().next() else {
            self.playing = false;
            self.stats.underruns += 1;
            return Pull::Empty;
        };
        let next = self.next.unwrap_or(first);
        // A gap longer than the buffer means the stream restarted or a burst
        // was lost; skip ahead instead of concealing it packet by packet.
        let next = if first - next.min(first) > self.config.max_packets as u64 {
            first
        } else {
            next
        };
        self.next = Some(next + 1);
        match self.packets.remove(&next) {
            Some(p) => Pull::Packet(p),
            None => {
                self.stats.concealed += 1;
                Pull::Lost
            }
        }
    }

    pub fn stats(&self) -> JitterStats {
        self.stats
    }
}

/// Network side: hand it every Opus payload from the client's mic track.
#[derive(Clone)]
pub struct MicReceiver {
    jitter: Arc<Mutex<JitterBuffer>>,
}

impl MicReceiver {
    pub fn push(&self, rtp_seq: u16, opus_payload: &[u8]) {
        self.jitter
            .lock()
            .unwrap()
            .push(rtp_seq, opus_payload.to_vec());
    }

    pub fn stats(&self) -> JitterStats {
        self.jitter.lock().unwrap().stats()
    }
}

/// Render side: decodes on demand into interleaved 48 kHz stereo.
pub struct MicSource {
    jitter: Arc<Mutex<JitterBuffer>>,
    decoder: Decoder,
    pcm: VecDeque<f32>,
    scratch: Vec<f32>,
    /// Samples per channel of the last decoded packet, used to size concealment.
    last_frame: usize,
}

pub fn mic_channel(config: JitterConfig) -> Result<(MicReceiver, MicSource), AudioError> {
    let jitter = Arc::new(Mutex::new(JitterBuffer::new(config)));
    let source = MicSource {
        jitter: jitter.clone(),
        decoder: Decoder::new(48_000, Channels::Stereo)?,
        pcm: VecDeque::new(),
        // 120 ms is Opus's longest packet.
        scratch: vec![0.0; 5760 * CHANNELS],
        last_frame: 960,
    };
    Ok((MicReceiver { jitter }, source))
}

impl MicSource {
    /// Fills `out` (interleaved stereo); plays silence where no audio is ready.
    pub fn fill(&mut self, out: &mut [f32]) {
        while self.pcm.len() < out.len() {
            let pull = self.jitter.lock().unwrap().pull();
            let decoded = match &pull {
                Pull::Packet(p) => self.decoder.decode_float(p, &mut self.scratch, false),
                Pull::Lost => {
                    let n = self.last_frame * CHANNELS;
                    self.decoder
                        .decode_float(&[], &mut self.scratch[..n], false)
                }
                Pull::Empty => break,
            };
            match decoded {
                Ok(frames) => {
                    if matches!(pull, Pull::Packet(_)) {
                        self.last_frame = frames;
                    }
                    self.pcm.extend(&self.scratch[..frames * CHANNELS]);
                }
                Err(e) => debug!("mic packet failed to decode: {e}"),
            }
        }
        let ready = self.pcm.len().min(out.len());
        for (o, s) in out.iter_mut().zip(self.pcm.drain(..ready)) {
            *o = s;
        }
        out[ready..].fill(0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jb() -> JitterBuffer {
        JitterBuffer::new(JitterConfig {
            target_packets: 2,
            max_packets: 4,
        })
    }

    fn p(n: u8) -> Vec<u8> {
        vec![n]
    }

    #[test]
    fn waits_for_target_then_plays_in_order() {
        let mut j = jb();
        j.push(11, p(11));
        assert_eq!(j.pull(), Pull::Empty);
        j.push(10, p(10));
        assert_eq!(j.pull(), Pull::Packet(p(10)));
        assert_eq!(j.pull(), Pull::Packet(p(11)));
        assert_eq!(j.pull(), Pull::Empty);
        assert_eq!(j.stats().underruns, 1);
    }

    #[test]
    fn conceals_gaps_and_drops_late_packets() {
        let mut j = jb();
        j.push(1, p(1));
        j.push(3, p(3));
        assert_eq!(j.pull(), Pull::Packet(p(1)));
        assert_eq!(j.pull(), Pull::Lost);
        j.push(2, p(2)); // arrived after its slot was concealed
        assert_eq!(j.pull(), Pull::Packet(p(3)));
        assert_eq!(j.stats().late, 1);
        assert_eq!(j.stats().concealed, 1);
    }

    #[test]
    fn handles_sequence_wraparound() {
        let mut j = jb();
        j.push(65535, p(1));
        j.push(0, p(2));
        j.push(1, p(3));
        assert_eq!(j.pull(), Pull::Packet(p(1)));
        assert_eq!(j.pull(), Pull::Packet(p(2)));
        assert_eq!(j.pull(), Pull::Packet(p(3)));
    }

    #[test]
    fn backlog_drops_oldest_to_stay_current() {
        let mut j = jb();
        for s in 0..7u16 {
            j.push(s, p(s as u8));
        }
        assert_eq!(j.stats().overflow_dropped, 3);
        assert_eq!(j.pull(), Pull::Packet(p(3)));
    }

    #[test]
    fn long_gap_skips_ahead_instead_of_concealing() {
        let mut j = jb();
        j.push(0, p(0));
        j.push(1, p(1));
        assert_eq!(j.pull(), Pull::Packet(p(0)));
        assert_eq!(j.pull(), Pull::Packet(p(1)));
        j.push(500, p(5));
        j.push(501, p(6));
        assert_eq!(j.pull(), Pull::Packet(p(5)));
        assert_eq!(j.stats().concealed, 0);
    }

    #[test]
    fn source_decodes_and_pads_with_silence() {
        let (rx, mut src) = mic_channel(JitterConfig::default()).unwrap();
        let mut enc = opus::Encoder::new(48_000, Channels::Mono, opus::Application::Voip).unwrap();
        let tone: Vec<f32> = (0..960).map(|i| 0.5 * (i as f32 * 0.05).sin()).collect();
        let mut buf = [0u8; 1500];
        for seq in 0..3u16 {
            let n = enc.encode_float(&tone, &mut buf).unwrap();
            rx.push(seq, &buf[..n]);
        }
        // 3 packets of 20 ms mono decode to 2880 stereo frames; ask for more.
        let mut out = vec![1.0f32; 4000 * CHANNELS];
        src.fill(&mut out);
        assert!(out[..2880 * CHANNELS].iter().any(|s| s.abs() > 0.01));
        assert!(out[2880 * CHANNELS..].iter().all(|&s| s == 0.0));
    }

    #[test]
    fn source_conceals_a_lost_packet() {
        let (rx, mut src) = mic_channel(JitterConfig::default()).unwrap();
        let mut enc = opus::Encoder::new(48_000, Channels::Mono, opus::Application::Voip).unwrap();
        let tone = vec![0.2f32; 960];
        let mut buf = [0u8; 1500];
        for seq in [0u16, 2] {
            let n = enc.encode_float(&tone, &mut buf).unwrap();
            rx.push(seq, &buf[..n]);
        }
        let mut out = vec![0f32; 960 * 3 * CHANNELS];
        src.fill(&mut out);
        assert_eq!(rx.stats().concealed, 1);
        assert_eq!(src.pcm.len(), 0, "all three slots were produced and played");
    }
}
