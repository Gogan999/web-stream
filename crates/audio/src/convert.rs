//! Converts whatever the audio engine hands us into interleaved stereo f32
//! at 48 kHz, the only format the Opus path uses.

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    F32,
    I16,
    /// 24-bit samples packed in 3 bytes.
    I24,
    /// 32-bit integer, also used for 24-bit-in-32 containers.
    I32,
}

impl SampleFormat {
    pub fn bytes(self) -> usize {
        match self {
            Self::F32 | Self::I32 => 4,
            Self::I16 => 2,
            Self::I24 => 3,
        }
    }

    fn read(self, b: &[u8]) -> f32 {
        match self {
            Self::F32 => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            Self::I16 => i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0,
            Self::I24 => (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0,
            Self::I32 => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputFormat {
    pub format: SampleFormat,
    pub channels: usize,
    pub rate: u32,
}

impl InputFormat {
    pub fn frame_bytes(&self) -> usize {
        self.format.bytes() * self.channels
    }

    pub fn is_native(&self) -> bool {
        self.format == SampleFormat::F32 && self.channels == CHANNELS && self.rate == SAMPLE_RATE
    }
}

const MINUS_3DB: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// Folds one interleaved frame down to stereo. 5.1 and 7.1 assume the
/// standard WAVE channel order (FL FR FC LFE BL BR [SL SR]).
fn downmix(frame: &[f32]) -> (f32, f32) {
    match frame.len() {
        0 => (0.0, 0.0),
        1 => (frame[0], frame[0]),
        6 | 8 => {
            let c = frame[2] * MINUS_3DB;
            let (mut l, mut r) = (
                frame[0] + c + frame[4] * MINUS_3DB,
                frame[1] + c + frame[5] * MINUS_3DB,
            );
            if frame.len() == 8 {
                l += frame[6] * MINUS_3DB;
                r += frame[7] * MINUS_3DB;
            }
            let norm = if frame.len() == 8 {
                1.0 / (1.0 + 3.0 * MINUS_3DB)
            } else {
                1.0 / (1.0 + 2.0 * MINUS_3DB)
            };
            (l * norm, r * norm)
        }
        _ => (frame[0], frame[1]),
    }
}

/// Streaming converter: format, channel count, then linear resampling.
pub struct Converter {
    input: InputFormat,
    /// Input frames per output frame.
    step: f64,
    /// Position of the next output frame, in input frames after `prev`.
    pos: f64,
    prev: (f32, f32),
}

impl Converter {
    pub fn new(input: InputFormat) -> Self {
        Self {
            input,
            step: input.rate as f64 / SAMPLE_RATE as f64,
            pos: 0.0,
            prev: (0.0, 0.0),
        }
    }

    pub fn input(&self) -> InputFormat {
        self.input
    }

    /// Converts whole frames from `bytes` and appends stereo samples to `out`.
    pub fn push(&mut self, bytes: &[u8], out: &mut Vec<f32>) {
        let fb = self.input.frame_bytes();
        let sb = self.input.format.bytes();
        let mut frame = [0f32; 8];
        let channels = self.input.channels.min(frame.len());
        for chunk in bytes.chunks_exact(fb) {
            for (c, s) in frame.iter_mut().take(channels).enumerate() {
                *s = self.input.format.read(&chunk[c * sb..]);
            }
            let cur = downmix(&frame[..channels]);
            if self.input.rate == SAMPLE_RATE {
                out.extend_from_slice(&[cur.0, cur.1]);
                continue;
            }
            // Emit every output frame that falls between `prev` and `cur`.
            while self.pos < 1.0 {
                let t = self.pos as f32;
                out.push(self.prev.0 + (cur.0 - self.prev.0) * t);
                out.push(self.prev.1 + (cur.1 - self.prev.1) * t);
                self.pos += self.step;
            }
            self.pos -= 1.0;
            self.prev = cur;
        }
    }

    /// Appends `frames` frames of silence, keeping the resampler continuous.
    pub fn push_silence(&mut self, frames: usize, out: &mut Vec<f32>) {
        let zeros = vec![0u8; frames * self.input.frame_bytes()];
        self.push(&zeros, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f32_bytes(samples: &[f32]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    #[test]
    fn native_format_passes_through() {
        let mut c = Converter::new(InputFormat {
            format: SampleFormat::F32,
            channels: 2,
            rate: 48_000,
        });
        let mut out = vec![];
        c.push(&f32_bytes(&[0.5, -0.5, 0.25, -0.25]), &mut out);
        assert_eq!(out, [0.5, -0.5, 0.25, -0.25]);
    }

    #[test]
    fn integer_formats_scale_to_unit_range() {
        assert_eq!(SampleFormat::I16.read(&i16::MIN.to_le_bytes()), -1.0);
        assert_eq!(SampleFormat::I16.read(&16384i16.to_le_bytes()), 0.5);
        assert_eq!(SampleFormat::I24.read(&[0x00, 0x00, 0x40]), 0.5);
        assert_eq!(SampleFormat::I24.read(&[0x00, 0x00, 0x80]), -1.0);
        assert_eq!(SampleFormat::I32.read(&(1i32 << 30).to_le_bytes()), 0.5);
    }

    #[test]
    fn mono_is_duplicated_and_surround_folds_down() {
        assert_eq!(downmix(&[0.3]), (0.3, 0.3));
        let (l, r) = downmix(&[1.0, 0.0, 0.0, 0.9, 0.0, 0.0]);
        assert!(l > 0.0 && r == 0.0, "LFE is dropped, front left stays left");
        let (l, r) = downmix(&[1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0]);
        assert!(
            (l - 1.0).abs() < 1e-6 && (r - 1.0).abs() < 1e-6,
            "full-scale 7.1 does not clip"
        );
    }

    #[test]
    fn resamples_44k1_to_48k() {
        let input = InputFormat {
            format: SampleFormat::F32,
            channels: 2,
            rate: 44_100,
        };
        let mut c = Converter::new(input);
        let mut out = vec![];
        // One second in ten pushes, as the capture loop would deliver it.
        let mut n = 0u32;
        for _ in 0..10 {
            let samples: Vec<f32> = (0..4410)
                .flat_map(|_| {
                    let v = (n as f32 * 2.0 * std::f32::consts::PI * 1000.0 / 44_100.0).sin();
                    n += 1;
                    [v, v]
                })
                .collect();
            c.push(&f32_bytes(&samples), &mut out);
        }
        let frames = out.len() / 2;
        assert!((47_995..=48_002).contains(&frames), "got {frames} frames");
        // Interpolated values stay in range and track a 1 kHz sine.
        assert!(out.iter().all(|s| s.abs() <= 1.0));
        let peak = out.iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!(peak > 0.95);
    }
}
