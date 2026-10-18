//! Slices 48 kHz stereo into 10 ms (or 20 ms) frames and encodes each one
//! with Opus.

use std::time::{Duration, Instant};

use bytes::Bytes;
use opus::{Application, Bitrate, Channels, Encoder};
use ws_proto::{AudioDirection, AudioPacket, MicrosMono};

use crate::AudioError;
use crate::convert::{CHANNELS, SAMPLE_RATE};

/// Samples per channel in one 10 ms frame at 48 kHz.
pub const FRAME_SAMPLES: usize = 480;
pub const FRAME_DURATION: Duration = Duration::from_millis(10);
pub const DEFAULT_BITRATE: u32 = 128_000;
/// Frame lengths the encoder accepts, in milliseconds. 20 ms sounds better
/// at the same bitrate; 10 ms is 10 ms sooner.
pub const FRAME_MS: [u32; 2] = [10, 20];
/// Packet timestamps follow the sample count, not the jittery time each
/// buffer was read; they jump forward to the wall clock only when it gets
/// this far ahead (a stall, a reopened device), never back.
const RESYNC_MICROS: u64 = 80_000;

/// Converts an [`Instant`] to [`MicrosMono`]. The epoch is the first call in
/// the process.
// TODO: move to ws-proto once the capture crate needs the same clock.
pub fn micros_mono(at: Instant) -> MicrosMono {
    static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let epoch = *EPOCH.get_or_init(|| at);
    at.saturating_duration_since(epoch).as_micros() as MicrosMono
}

pub struct OpusFrameEncoder {
    encoder: Encoder,
    /// Samples per channel in a frame, and what it becomes after the frame
    /// being filled now.
    frame_samples: usize,
    next_frame_samples: usize,
    pending: Vec<f32>,
    pending_start: Option<Instant>,
    next_sequence: u64,
    /// Capture time of the first sample of the next packet, when known.
    next_capture: Option<MicrosMono>,
    packet: [u8; 1500],
}

impl OpusFrameEncoder {
    pub fn new(bitrate: u32) -> Result<Self, AudioError> {
        // Restricted low delay is CELT-only: 2.5 ms algorithmic delay instead
        // of SILK's 6.5 ms, and it suits game audio and music better than speech mode.
        let mut encoder = Encoder::new(SAMPLE_RATE, Channels::Stereo, Application::LowDelay)?;
        encoder.set_bitrate(Bitrate::Bits(bitrate as i32))?;
        encoder.set_vbr(true)?;
        // Encoding stereo audio at full complexity costs well under 1% of a core.
        encoder.set_complexity(10)?;
        Ok(Self {
            encoder,
            frame_samples: FRAME_SAMPLES,
            next_frame_samples: FRAME_SAMPLES,
            pending: Vec::with_capacity(FRAME_SAMPLES * CHANNELS * 4),
            pending_start: None,
            next_sequence: 0,
            next_capture: None,
            packet: [0; 1500],
        })
    }

    /// Changes the frame length (one of [`FRAME_MS`]) from the next frame on.
    pub fn set_frame_ms(&mut self, ms: u32) -> Result<(), AudioError> {
        if !FRAME_MS.contains(&ms) {
            return Err(AudioError::Platform(format!(
                "unsupported Opus frame {ms} ms"
            )));
        }
        self.next_frame_samples = SAMPLE_RATE as usize / 1000 * ms as usize;
        if self.pending.is_empty() {
            self.frame_samples = self.next_frame_samples;
        }
        Ok(())
    }

    /// Where a packet starting at wall-clock `wall` sits on the sample clock:
    /// right after the previous packet, unless that has drifted too far.
    fn stamp(&mut self, wall: MicrosMono, samples: usize) -> MicrosMono {
        let at = match self.next_capture {
            // Only forward: a device clock running a little fast puts the
            // sample clock ahead of the wall clock, and jumping back would
            // overlap packets already sent.
            Some(t) if wall <= t + RESYNC_MICROS => t,
            _ => wall,
        };
        self.next_capture = Some(at + samples as u64 * 1_000_000 / u64::from(SAMPLE_RATE));
        at
    }

    pub fn set_bitrate(&mut self, bitrate: u32) -> Result<(), AudioError> {
        Ok(self.encoder.set_bitrate(Bitrate::Bits(bitrate as i32))?)
    }

    /// Adds interleaved stereo samples whose first frame was captured at
    /// `captured_at`, and calls `emit` for every complete packet.
    ///
    /// Packets are numbered gap-free from 0. Consecutive capture times are
    /// exactly one packet's duration apart (whole RTP ticks at 48 kHz), so
    /// the receiver never sees a gap or overlap to conceal.
    pub fn push(
        &mut self,
        samples: &[f32],
        captured_at: Instant,
        mut emit: impl FnMut(AudioPacket),
    ) -> Result<(), AudioError> {
        let samples = &samples[..samples.len() - samples.len() % CHANNELS];
        let mut at = 0;
        while at < samples.len() {
            let frame_len = self.frame_samples * CHANNELS;
            if self.pending.is_empty() {
                let offset = Duration::from_secs_f64((at / CHANNELS) as f64 / 48_000.0);
                self.pending_start = Some(captured_at + offset);
            }
            let take = (frame_len - self.pending.len()).min(samples.len() - at);
            self.pending.extend_from_slice(&samples[at..at + take]);
            at += take;
            if self.pending.len() == frame_len {
                let len = self.encoder.encode_float(&self.pending, &mut self.packet)?;
                let wall = micros_mono(self.pending_start.take().unwrap_or(captured_at));
                let capture_time = self.stamp(wall, self.frame_samples);
                emit(AudioPacket {
                    direction: AudioDirection::Playback,
                    sequence: self.next_sequence,
                    capture_time,
                    sample_rate: SAMPLE_RATE,
                    channels: CHANNELS as u8,
                    data: Bytes::copy_from_slice(&self.packet[..len]),
                });
                self.next_sequence += 1;
                self.pending.clear();
                self.frame_samples = self.next_frame_samples;
            }
        }
        Ok(())
    }

    /// Samples waiting for a full frame, per channel.
    pub fn pending_samples(&self) -> usize {
        self.pending.len() / CHANNELS
    }

    pub fn next_sequence(&self) -> u64 {
        self.next_sequence
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_one_packet_per_10ms_with_running_timestamps() {
        let mut enc = OpusFrameEncoder::new(DEFAULT_BITRATE).unwrap();
        // Pin the clock's epoch before `start`; another test may set it first.
        let _ = micros_mono(Instant::now());
        let start = Instant::now();
        let mut frames = vec![];
        // 25 ms arriving as 7 ms + 18 ms chunks.
        let chunk = |n: usize| vec![0.1f32; n * CHANNELS];
        enc.push(&chunk(336), start, |f| frames.push(f)).unwrap();
        assert!(frames.is_empty());
        enc.push(&chunk(864), start + Duration::from_millis(7), |f| {
            frames.push(f)
        })
        .unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(enc.pending_samples(), 240);
        assert_eq!(frames[0].sequence, 0);
        assert_eq!(frames[1].sequence, 1);
        assert_eq!(frames[0].capture_time, micros_mono(start));
        let gap = frames[1].capture_time - frames[0].capture_time;
        assert!((9_999..=10_001).contains(&gap), "{gap} us");
        assert!(
            frames
                .iter()
                .all(|f| !f.data.is_empty() && f.data.len() < 1275)
        );
    }

    #[test]
    fn packets_decode_back_to_a_frame_of_audio() {
        let mut enc = OpusFrameEncoder::new(DEFAULT_BITRATE).unwrap();
        let tone: Vec<f32> = (0..FRAME_SAMPLES * 4)
            .flat_map(|i| {
                let v = 0.5 * (i as f32 * 2.0 * std::f32::consts::PI * 440.0 / 48_000.0).sin();
                [v, v]
            })
            .collect();
        let mut packets = vec![];
        enc.push(&tone, Instant::now(), |f| packets.push(f.data))
            .unwrap();
        assert!(packets.iter().all(|p| p.len() < 1275));
        assert_eq!(packets.len(), 4);
        let mut dec = opus::Decoder::new(48_000, opus::Channels::Stereo).unwrap();
        let mut out = vec![0f32; FRAME_SAMPLES * CHANNELS];
        let mut energy = 0.0;
        for p in &packets {
            assert_eq!(dec.decode_float(p, &mut out, false).unwrap(), FRAME_SAMPLES);
            energy += out.iter().map(|s| s * s).sum::<f32>();
        }
        assert!(energy > 100.0, "decoded audio is not silent ({energy})");
    }

    #[test]
    fn timestamps_follow_samples_not_read_jitter() {
        let mut enc = OpusFrameEncoder::new(DEFAULT_BITRATE).unwrap();
        let start = Instant::now();
        let _ = micros_mono(start);
        let mut frames = vec![];
        // 10 ms buffers read 0-3 ms late, as a woken thread does.
        for (i, late) in [0u64, 3, 1, 2, 0, 3].iter().enumerate() {
            let at = start + Duration::from_millis(i as u64 * 10 + late);
            enc.push(&vec![0.1f32; FRAME_SAMPLES * CHANNELS], at, |f| {
                frames.push(f)
            })
            .unwrap();
        }
        let gaps: Vec<_> = frames
            .windows(2)
            .map(|w| w[1].capture_time - w[0].capture_time)
            .collect();
        assert!(gaps.iter().all(|&g| g == 10_000), "{gaps:?}");

        // A real gap (a stall) puts timestamps back on the wall clock.
        let late = start + Duration::from_millis(500);
        enc.push(&vec![0.1f32; FRAME_SAMPLES * CHANNELS], late, |f| {
            frames.push(f)
        })
        .unwrap();
        assert_eq!(frames.last().unwrap().capture_time, micros_mono(late));

        // Audio arriving early (a fast device clock) never steps back.
        let before = frames.last().unwrap().capture_time;
        enc.push(&vec![0.1f32; FRAME_SAMPLES * CHANNELS], start, |f| {
            frames.push(f)
        })
        .unwrap();
        assert_eq!(frames.last().unwrap().capture_time, before + 10_000);
    }

    #[test]
    fn switches_to_20ms_frames_at_a_frame_boundary() {
        let mut enc = OpusFrameEncoder::new(DEFAULT_BITRATE).unwrap();
        let start = Instant::now();
        let _ = micros_mono(start);
        let mut packets = vec![];
        enc.push(&vec![0.1f32; 240 * CHANNELS], start, |f| packets.push(f))
            .unwrap();
        enc.set_frame_ms(20).unwrap();
        assert!(enc.set_frame_ms(15).is_err());
        // Finishes the 10 ms frame it started, then makes 20 ms ones.
        enc.push(&vec![0.1f32; 2160 * CHANNELS], start, |f| packets.push(f))
            .unwrap();
        assert_eq!(packets.len(), 3);
        assert_eq!(enc.pending_samples(), 0);
        assert_eq!(packets[1].capture_time - packets[0].capture_time, 10_000);
        assert_eq!(packets[2].capture_time - packets[1].capture_time, 20_000);
        let mut dec = opus::Decoder::new(48_000, opus::Channels::Stereo).unwrap();
        let mut out = vec![0f32; 960 * CHANNELS];
        assert_eq!(
            dec.decode_float(&packets[2].data, &mut out, false).unwrap(),
            960
        );
    }
}
