//! Slices 48 kHz stereo into 10 ms frames and encodes each one with Opus.

use std::time::{Duration, Instant};

use bytes::Bytes;
use opus::{Application, Bitrate, Channels, Encoder};
use ws_proto::{AudioDirection, AudioPacket, micros_mono};

use crate::AudioError;
use crate::convert::{CHANNELS, SAMPLE_RATE};

/// Samples per channel in one 10 ms frame at 48 kHz.
pub const FRAME_SAMPLES: usize = 480;
pub const FRAME_DURATION: Duration = Duration::from_millis(10);
pub const DEFAULT_BITRATE: u32 = 128_000;

pub struct OpusFrameEncoder {
    encoder: Encoder,
    pending: Vec<f32>,
    pending_start: Option<Instant>,
    next_sequence: u64,
    packet: [u8; 1500],
}

impl OpusFrameEncoder {
    pub fn new(bitrate: u32) -> Result<Self, AudioError> {
        // Restricted low delay is CELT-only: 2.5 ms algorithmic delay instead
        // of SILK's 6.5 ms, and it suits game audio and music better than speech mode.
        let mut encoder = Encoder::new(SAMPLE_RATE, Channels::Stereo, Application::LowDelay)?;
        encoder.set_bitrate(Bitrate::Bits(bitrate as i32))?;
        encoder.set_vbr(true)?;
        encoder.set_complexity(8)?;
        Ok(Self {
            encoder,
            pending: Vec::with_capacity(FRAME_SAMPLES * CHANNELS * 2),
            pending_start: None,
            next_sequence: 0,
            packet: [0; 1500],
        })
    }

    pub fn set_bitrate(&mut self, bitrate: u32) -> Result<(), AudioError> {
        Ok(self.encoder.set_bitrate(Bitrate::Bits(bitrate as i32))?)
    }

    /// Adds interleaved stereo samples whose first frame was captured at
    /// `captured_at`, and calls `emit` for every complete 10 ms packet.
    ///
    /// Packets are numbered gap-free from 0, so `sequence * 480` is the RTP
    /// timestamp offset.
    pub fn push(
        &mut self,
        samples: &[f32],
        captured_at: Instant,
        mut emit: impl FnMut(AudioPacket),
    ) -> Result<(), AudioError> {
        let frame_len = FRAME_SAMPLES * CHANNELS;
        let samples = &samples[..samples.len() - samples.len() % CHANNELS];
        let mut at = 0;
        while at < samples.len() {
            if self.pending.is_empty() {
                let offset = Duration::from_secs_f64((at / CHANNELS) as f64 / 48_000.0);
                self.pending_start = Some(captured_at + offset);
            }
            let take = (frame_len - self.pending.len()).min(samples.len() - at);
            self.pending.extend_from_slice(&samples[at..at + take]);
            at += take;
            if self.pending.len() == frame_len {
                let len = self.encoder.encode_float(&self.pending, &mut self.packet)?;
                emit(AudioPacket {
                    direction: AudioDirection::Playback,
                    sequence: self.next_sequence,
                    capture_time: micros_mono(self.pending_start.take().unwrap_or(captured_at)),
                    sample_rate: SAMPLE_RATE,
                    channels: CHANNELS as u8,
                    data: Bytes::copy_from_slice(&self.packet[..len]),
                });
                self.next_sequence += 1;
                self.pending.clear();
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
}
