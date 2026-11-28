//! Which video codec a connection streams.

use ws_capture::caps::{EncoderInfo, choose_codec};
use ws_proto::{DecoderSupport, VideoCodec, VideoConfig};

/// What the browser decodes, and whether it leaves the codec to the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserDecoders {
    pub decoders: Vec<DecoderSupport>,
    pub auto: bool,
}

/// The codec to stream, and whether it was picked from the browser's decoders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodecChoice {
    pub codec: VideoCodec,
    pub auto: bool,
}

/// With `browser.auto`, the best codec some encoder makes at the config's size and the browser
/// decodes, of those its answer accepted and this PC has not refused; otherwise, or when nothing
/// fits, [`pick_codec`] on the configured codec.
pub fn stream_codec(
    config: &VideoConfig,
    accepted: Option<&[VideoCodec]>,
    refused: &[VideoCodec],
    browser: Option<&BrowserDecoders>,
    encoders: &[EncoderInfo],
) -> Option<CodecChoice> {
    if let Some(browser) = browser.filter(|b| b.auto) {
        let decoders: Vec<DecoderSupport> = browser
            .decoders
            .iter()
            .filter(|d| usable(d.codec, accepted, refused))
            .copied()
            .collect();
        if let Some(codec) = choose_codec(encoders, &decoders, config.width, config.height) {
            return Some(CodecChoice { codec, auto: true });
        }
    }
    pick_codec(config.codec, accepted, refused).map(|codec| CodecChoice { codec, auto: false })
}

fn usable(codec: VideoCodec, accepted: Option<&[VideoCodec]>, refused: &[VideoCodec]) -> bool {
    !refused.contains(&codec) && accepted.is_none_or(|a| a.contains(&codec))
}

/// The configured codec if the browser accepted it, else H.264, else anything it accepted.
pub fn pick_codec(
    wanted: VideoCodec,
    accepted: Option<&[VideoCodec]>,
    refused: &[VideoCodec],
) -> Option<VideoCodec> {
    let usable = |c: &VideoCodec| usable(*c, accepted, refused);
    [wanted, VideoCodec::H264]
        .into_iter()
        .find(usable)
        .or_else(|| accepted?.iter().copied().find(usable))
}

#[cfg(test)]
mod tests {
    use super::*;
    use VideoCodec::*;
    use ws_capture::caps::{CodecSupport, EncoderBackend};

    fn encoder(codecs: &[VideoCodec]) -> EncoderInfo {
        EncoderInfo {
            backend: EncoderBackend::Nvenc,
            adapter: "GPU".into(),
            codecs: codecs
                .iter()
                .map(|&codec| CodecSupport {
                    codec,
                    max_width: 4096,
                    max_height: 4096,
                    repair: false,
                })
                .collect(),
        }
    }

    fn browser(auto: bool, hw: &[VideoCodec], sw: &[VideoCodec]) -> BrowserDecoders {
        let dec = |power_efficient| {
            move |&codec| DecoderSupport {
                codec,
                smooth: true,
                power_efficient,
            }
        };
        BrowserDecoders {
            decoders: hw
                .iter()
                .map(dec(true))
                .chain(sw.iter().map(dec(false)))
                .collect(),
            auto,
        }
    }

    fn config(codec: VideoCodec, width: u32, height: u32) -> VideoConfig {
        VideoConfig {
            codec,
            width,
            height,
            fps: 60,
            bitrate_kbps: 20000,
            quality: Default::default(),
            keyframe_scale: ws_proto::DEFAULT_KEYFRAME_SCALE,
            recover_drops: true,
            min_scale_percent: 0,
        }
    }

    fn auto(codec: VideoCodec) -> Option<CodecChoice> {
        Some(CodecChoice { codec, auto: true })
    }

    fn manual(codec: VideoCodec) -> Option<CodecChoice> {
        Some(CodecChoice { codec, auto: false })
    }

    #[test]
    fn codec_falls_back_to_h264_then_anything() {
        assert_eq!(pick_codec(Av1, None, &[]), Some(Av1));
        assert_eq!(pick_codec(Av1, Some(&[H264, Av1]), &[]), Some(Av1));
        assert_eq!(pick_codec(H265, Some(&[Av1, H264]), &[]), Some(H264));
        assert_eq!(pick_codec(H265, Some(&[Av1]), &[]), Some(Av1));
        assert_eq!(pick_codec(H264, Some(&[]), &[]), None);
        // A codec this PC failed to encode is skipped.
        assert_eq!(pick_codec(Av1, None, &[Av1]), Some(H264));
        assert_eq!(pick_codec(Av1, Some(&[H264, Av1]), &[Av1]), Some(H264));
        assert_eq!(pick_codec(H265, Some(&[Av1, H265]), &[H265]), Some(Av1));
    }

    #[test]
    fn auto_picks_from_encoders_and_decoders() {
        let encoders = [encoder(&[H264, H265, Av1])];
        let b = browser(true, &[H264, H265], &[Av1]);
        let c = config(H264, 1920, 1080);
        assert_eq!(stream_codec(&c, None, &[], Some(&b), &encoders), auto(H265));
        let all = browser(true, &[H264, H265, Av1], &[]);
        assert_eq!(
            stream_codec(&c, None, &[], Some(&all), &encoders),
            auto(Av1)
        );
    }

    #[test]
    fn without_auto_or_decoders_the_configured_codec_streams() {
        let encoders = [encoder(&[H264, H265, Av1])];
        let c = config(H264, 1920, 1080);
        assert_eq!(stream_codec(&c, None, &[], None, &encoders), manual(H264));
        let b = browser(false, &[H265, Av1], &[]);
        assert_eq!(
            stream_codec(&c, None, &[], Some(&b), &encoders),
            manual(H264)
        );
        let c = config(H265, 1920, 1080);
        assert_eq!(
            stream_codec(&c, Some(&[H264]), &[], Some(&b), &encoders),
            manual(H264)
        );
    }

    #[test]
    fn unknown_encoders_or_no_fit_keep_todays_choice() {
        let b = browser(true, &[H264, H265, Av1], &[]);
        let c = config(H265, 1920, 1080);
        assert_eq!(stream_codec(&c, None, &[], Some(&b), &[]), manual(H265));
        let small = [encoder(&[Av1])];
        let big = config(H265, 7680, 4320);
        assert_eq!(
            stream_codec(&big, None, &[], Some(&b), &small),
            manual(H265)
        );
        let none = browser(true, &[], &[]);
        assert_eq!(
            stream_codec(&c, None, &[], Some(&none), &small),
            manual(H265)
        );
    }

    #[test]
    fn auto_only_picks_what_the_answer_accepted_and_this_pc_did_not_refuse() {
        let encoders = [encoder(&[H264, H265, Av1])];
        let b = browser(true, &[H264, H265, Av1], &[]);
        let c = config(H264, 1920, 1080);
        assert_eq!(
            stream_codec(&c, Some(&[H264, H265]), &[], Some(&b), &encoders),
            auto(H265)
        );
        assert_eq!(
            stream_codec(&c, Some(&[H264, H265, Av1]), &[Av1], Some(&b), &encoders),
            auto(H265)
        );
        assert_eq!(
            stream_codec(&c, Some(&[H264]), &[], Some(&b), &encoders),
            auto(H264)
        );
        let av1_only = browser(true, &[Av1], &[]);
        assert_eq!(
            stream_codec(&c, Some(&[H264, H265]), &[], Some(&av1_only), &encoders),
            manual(H264)
        );
    }
}
