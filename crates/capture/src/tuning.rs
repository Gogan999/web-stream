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
//!
//! A keyframe may take [`VideoConfig::keyframe_scale`] times the VBV, but
//! never more than 200 ms on the wire: the frames queued behind it are only
//! exempt from the late-frame cutoff for 250 ms (the transport's
//! `KEYFRAME_DRAIN_MAX`), so a longer one would cost the frames after it.

use ws_proto::{VideoConfig, VideoQuality};

const P1080_60: u64 = 1920 * 1080 * 60;
const P1440_60: u64 = 2560 * 1440 * 60;
const P2160_60: u64 = 3840 * 2160 * 60;
/// Longest a single frame may take to send at the target bitrate.
const MAX_BURST_SECS: f64 = 0.020;
/// Longest a keyframe may take to send at the target bitrate.
const MAX_KEYFRAME_SECS: f64 = 0.200;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Tuning {
    /// NVENC preset number, 1..=7.
    pub preset: u8,
    /// VBV size in frames: the most one frame may take is `bitrate / fps *
    /// vbv_frames` bits.
    pub vbv_frames: f64,
    /// A keyframe's VBV is this many times `vbv_frames`, 1..=4.
    pub keyframe_scale: u8,
}

pub(crate) fn tuning(config: &VideoConfig) -> Tuning {
    let (preset, vbv_frames) = effort(config);
    let keyframe_frames = MAX_KEYFRAME_SECS * f64::from(config.fps.max(1));
    let fits = (keyframe_frames / vbv_frames).floor().clamp(1.0, 4.0) as u8;
    Tuning {
        preset,
        vbv_frames,
        keyframe_scale: config.keyframe_scale.clamp(1, fits),
    }
}

/// The preset and VBV for the config's quality.
fn effort(config: &VideoConfig) -> (u8, f64) {
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
        VideoQuality::Speed => (1, 1.0),
        VideoQuality::Balanced => (
            if rate <= P1080_60 {
                4
            } else {
                by_rate(3, 2, 1)
            },
            1.0,
        ),
        VideoQuality::Clarity => (
            by_rate(6, 4, 2),
            (MAX_BURST_SECS * f64::from(config.fps.max(1))).clamp(1.0, 2.0),
        ),
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
    fn keyframes_stay_under_200ms() {
        let scale = |fps, keyframe_scale, quality| {
            tuning(&VideoConfig {
                fps,
                keyframe_scale,
                quality,
                ..Default::default()
            })
            .keyframe_scale
        };
        use VideoQuality::*;
        assert_eq!(scale(60, 4, Balanced), 4);
        assert_eq!(scale(60, 4, Clarity), 4);
        // 15 fps: 3 frames is 200 ms.
        assert_eq!(scale(15, 4, Balanced), 3);
        assert_eq!(scale(10, 4, Balanced), 2);
        // Always allowed one frame's worth, and never more than asked.
        assert_eq!(scale(1, 4, Balanced), 1);
        assert_eq!(scale(60, 2, Speed), 2);
        for fps in [1, 5, 10, 15, 30, 60, 144] {
            for q in [Speed, Balanced, Clarity] {
                let t = tuning(&cfg(1920, 1080, fps, q));
                let secs = t.vbv_frames * f64::from(t.keyframe_scale) / f64::from(fps);
                assert!(
                    secs <= 0.2 || t.keyframe_scale == 1,
                    "{fps} fps {q:?}: {secs} s"
                );
            }
        }
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
