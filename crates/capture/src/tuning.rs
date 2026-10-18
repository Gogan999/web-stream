//! How hard the encoder works for a [`VideoQuality`], kept free of GPU calls
//! so it can be tested.
//!
//! NVENC presets run from P1 (fastest) to P7 (best picture per bit). Slower
//! presets cost encode time per frame, which grows with the pixel rate, so
//! each quality steps down a preset or two as resolution and frame rate go
//! up. The VBV (how many bits one frame may take) stays at one frame time
//! except for Clarity, which lets a large frame (fast motion, a keyframe)
//! take a little longer but never more than 20 ms on the wire, so the
//! transport's late-frame cutoff (50 ms) is not hit by design.

use ws_proto::{VideoConfig, VideoQuality};

const P1080_60: u64 = 1920 * 1080 * 60;
const P1440_60: u64 = 2560 * 1440 * 60;
const P2160_60: u64 = 3840 * 2160 * 60;
/// Longest a single frame may take to send at the target bitrate.
const MAX_BURST_SECS: f64 = 0.020;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Tuning {
    /// NVENC preset number, 1..=7.
    pub preset: u8,
    /// VBV size in frames: the most one frame may take is `bitrate / fps *
    /// vbv_frames` bits.
    pub vbv_frames: f64,
}

pub(crate) fn tuning(config: &VideoConfig) -> Tuning {
    let rate = u64::from(config.width) * u64::from(config.height) * u64::from(config.fps);
    let by_rate = |up_to_1440p60: u8, up_to_4k60: u8, above: u8| {
        if rate <= P1440_60 {
            up_to_1440p60
        } else if rate <= P2160_60 {
            up_to_4k60
        } else {
            above
        }
    };
    match config.quality {
        VideoQuality::Speed => Tuning {
            preset: 1,
            vbv_frames: 1.0,
        },
        VideoQuality::Balanced => Tuning {
            preset: if rate <= P1080_60 {
                4
            } else {
                by_rate(3, 2, 1)
            },
            vbv_frames: 1.0,
        },
        VideoQuality::Clarity => Tuning {
            preset: by_rate(6, 4, 2),
            vbv_frames: (MAX_BURST_SECS * f64::from(config.fps.max(1))).clamp(1.0, 2.0),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(width: u32, height: u32, fps: u32, quality: VideoQuality) -> VideoConfig {
        VideoConfig {
            width,
            height,
            fps,
            quality,
            ..Default::default()
        }
    }

    #[test]
    fn speed_is_the_fastest_preset() {
        let t = tuning(&cfg(1280, 720, 30, VideoQuality::Speed));
        assert_eq!((t.preset, t.vbv_frames), (1, 1.0));
    }

    #[test]
    fn presets_step_down_as_pixel_rate_grows() {
        use VideoQuality::*;
        assert_eq!(tuning(&cfg(1920, 1080, 60, Balanced)).preset, 4);
        assert_eq!(tuning(&cfg(2560, 1440, 60, Balanced)).preset, 3);
        assert_eq!(tuning(&cfg(3840, 2160, 60, Balanced)).preset, 2);
        assert_eq!(tuning(&cfg(3840, 2160, 120, Balanced)).preset, 1);
        assert_eq!(tuning(&cfg(1920, 1080, 60, Clarity)).preset, 6);
        assert_eq!(tuning(&cfg(3840, 2160, 60, Clarity)).preset, 4);
        assert_eq!(tuning(&cfg(3840, 2160, 144, Clarity)).preset, 2);
    }

    #[test]
    fn clarity_bursts_stay_under_20ms() {
        let vbv = |fps| tuning(&cfg(1920, 1080, fps, VideoQuality::Clarity)).vbv_frames;
        assert_eq!(vbv(30), 1.0);
        assert!((vbv(60) - 1.2).abs() < 1e-9);
        assert_eq!(vbv(144), 2.0);
        for fps in [30, 60, 90, 120, 144, 240] {
            assert!(vbv(fps) / f64::from(fps) <= MAX_BURST_SECS.max(1.0 / f64::from(fps)));
        }
    }
}
