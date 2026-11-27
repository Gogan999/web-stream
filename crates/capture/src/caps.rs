//! Encoder capabilities and codec selection, kept free of platform calls.

use serde::{Deserialize, Serialize};
use ws_proto::{DecoderSupport, VideoCodec};

/// A hardware encoder API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EncoderBackend {
    Nvenc,
    MediaFoundation,
    Vaapi,
}

impl EncoderBackend {
    pub fn name(self) -> &'static str {
        match self {
            Self::Nvenc => "NVENC",
            Self::MediaFoundation => "Media Foundation",
            Self::Vaapi => "VA-API",
        }
    }
}

/// One codec an encoder produces, and the largest size it takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodecSupport {
    pub codec: VideoCodec,
    pub max_width: u32,
    pub max_height: u32,
    /// Stops referencing a lost frame instead of needing a keyframe.
    pub repair: bool,
}

/// An encoder found on this PC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EncoderInfo {
    pub backend: EncoderBackend,
    /// The GPU it runs on, as the system names it.
    pub adapter: String,
    pub codecs: Vec<CodecSupport>,
}

impl EncoderInfo {
    /// Whether this encoder produces `codec` at `width` x `height`.
    pub fn encodes(&self, codec: VideoCodec, width: u32, height: u32) -> bool {
        self.codecs
            .iter()
            .any(|c| c.codec == codec && width <= c.max_width && height <= c.max_height)
    }
}

/// Codecs by bits needed for the same picture, fewest first.
const EFFICIENCY: [VideoCodec; 3] = [VideoCodec::Av1, VideoCodec::H265, VideoCodec::H264];

/// Whether any encoder produces `codec` at `width` x `height`.
pub fn encodable(encoders: &[EncoderInfo], codec: VideoCodec, width: u32, height: u32) -> bool {
    encoders.iter().any(|e| e.encodes(codec, width, height))
}

/// The codec to stream when the browser leaves the choice to the host: one some encoder produces
/// at this size and the browser decodes, preferring a power-efficient (hardware) decoder, then a
/// smooth one, then the codec that needs the fewest bits. `None` when nothing fits.
pub fn choose_codec(
    encoders: &[EncoderInfo],
    decoders: &[DecoderSupport],
    width: u32,
    height: u32,
) -> Option<VideoCodec> {
    EFFICIENCY
        .iter()
        .enumerate()
        .filter(|(_, codec)| encodable(encoders, **codec, width, height))
        .filter_map(|(rank, codec)| {
            let d = decoders.iter().find(|d| d.codec == *codec)?;
            Some((
                (d.power_efficient, d.smooth, std::cmp::Reverse(rank)),
                *codec,
            ))
        })
        .max_by_key(|(score, _)| *score)
        .map(|(_, codec)| codec)
}

/// Backends to try for `codec` at `width` x `height`, in order of preference.
pub fn backends_for(
    encoders: &[EncoderInfo],
    codec: VideoCodec,
    width: u32,
    height: u32,
) -> Vec<EncoderBackend> {
    let mut out: Vec<EncoderBackend> = Vec::new();
    for e in encoders {
        if e.encodes(codec, width, height) && !out.contains(&e.backend) {
            out.push(e.backend);
        }
    }
    out.sort_by_key(|&b| preference(b));
    out
}

/// Backends to try on the GPU named `adapter`, in order of preference: those the probe found there
/// for `codec` at this size, and each of `fallback` the probe did not find there at all.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn backends_on(
    encoders: &[EncoderInfo],
    adapter: &str,
    codec: VideoCodec,
    width: u32,
    height: u32,
    fallback: &[EncoderBackend],
) -> Vec<EncoderBackend> {
    let here: Vec<EncoderInfo> = encoders
        .iter()
        .filter(|e| e.adapter == adapter)
        .cloned()
        .collect();
    let mut out = backends_for(&here, codec, width, height);
    for &b in fallback {
        if !here.iter().any(|e| e.backend == b) && !out.contains(&b) {
            out.push(b);
        }
    }
    out.sort_by_key(|&b| preference(b));
    out
}

fn preference(backend: EncoderBackend) -> u8 {
    match backend {
        EncoderBackend::Nvenc => 0,
        EncoderBackend::Vaapi => 1,
        EncoderBackend::MediaFoundation => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(backend: EncoderBackend, codecs: &[(VideoCodec, u32, u32)]) -> EncoderInfo {
        EncoderInfo {
            backend,
            adapter: "GPU".into(),
            codecs: codecs
                .iter()
                .map(|&(codec, max_width, max_height)| CodecSupport {
                    codec,
                    max_width,
                    max_height,
                    repair: false,
                })
                .collect(),
        }
    }

    fn dec(codec: VideoCodec, smooth: bool, power_efficient: bool) -> DecoderSupport {
        DecoderSupport {
            codec,
            smooth,
            power_efficient,
        }
    }

    use VideoCodec::*;

    #[test]
    fn hardware_decode_beats_fewer_bits() {
        let e = [enc(
            EncoderBackend::Nvenc,
            &[(H264, 4096, 4096), (H265, 8192, 8192), (Av1, 8192, 8192)],
        )];
        let phone = [
            dec(H264, true, true),
            dec(H265, true, true),
            dec(Av1, true, false),
        ];
        assert_eq!(choose_codec(&e, &phone, 1920, 1080), Some(H265));
        let desktop = [
            dec(H264, true, true),
            dec(H265, true, true),
            dec(Av1, true, true),
        ];
        assert_eq!(choose_codec(&e, &desktop, 1920, 1080), Some(Av1));
    }

    #[test]
    fn only_what_some_encoder_makes_at_this_size() {
        let e = [enc(
            EncoderBackend::MediaFoundation,
            &[(H264, 4096, 2304), (H265, 4096, 2304)],
        )];
        let all = [
            dec(H264, true, true),
            dec(H265, true, true),
            dec(Av1, true, true),
        ];
        assert_eq!(choose_codec(&e, &all, 1920, 1080), Some(H265));
        assert_eq!(choose_codec(&e, &all, 7680, 4320), None);
        assert_eq!(choose_codec(&e, &[], 1920, 1080), None);
    }

    #[test]
    fn smooth_software_decode_beats_choppy() {
        let e = [enc(
            EncoderBackend::Vaapi,
            &[(H264, 4096, 4096), (Av1, 4096, 4096)],
        )];
        let d = [dec(H264, true, false), dec(Av1, false, false)];
        assert_eq!(choose_codec(&e, &d, 1920, 1080), Some(H264));
    }

    #[test]
    fn backend_order_prefers_nvenc_and_skips_what_cannot() {
        let e = [
            enc(EncoderBackend::MediaFoundation, &[(H264, 4096, 4096)]),
            enc(EncoderBackend::Nvenc, &[(H264, 8192, 8192)]),
            enc(EncoderBackend::Vaapi, &[(H265, 4096, 4096)]),
        ];
        assert_eq!(
            backends_for(&e, H264, 1920, 1080),
            [EncoderBackend::Nvenc, EncoderBackend::MediaFoundation]
        );
        assert_eq!(backends_for(&e, H264, 6000, 3000), [EncoderBackend::Nvenc]);
        assert!(backends_for(&e, Av1, 1920, 1080).is_empty());
    }

    #[test]
    fn backends_on_one_adapter_or_the_fallback() {
        let mut nv = enc(EncoderBackend::Nvenc, &[(H264, 4096, 4096)]);
        nv.adapter = "RTX".into();
        let mut mf = enc(
            EncoderBackend::MediaFoundation,
            &[(H264, 4096, 4096), (Av1, 8192, 8192)],
        );
        mf.adapter = "Arc".into();
        let e = [nv.clone(), mf];
        let nvidia = [EncoderBackend::Nvenc, EncoderBackend::MediaFoundation];
        let other = [EncoderBackend::MediaFoundation];
        assert_eq!(
            backends_on(&e, "Arc", H264, 1920, 1080, &other),
            [EncoderBackend::MediaFoundation]
        );
        assert!(backends_on(&e, "Arc", H265, 1920, 1080, &other).is_empty());
        assert_eq!(
            backends_on(&e, "RTX", H264, 1920, 1080, &nvidia),
            [EncoderBackend::Nvenc, EncoderBackend::MediaFoundation]
        );
        assert_eq!(
            backends_on(&e, "RTX", Av1, 1920, 1080, &nvidia),
            [EncoderBackend::MediaFoundation]
        );
        assert_eq!(backends_on(&e, "Other", Av1, 1920, 1080, &nvidia), nvidia);
        assert_eq!(backends_on(&[], "RTX", H264, 1920, 1080, &nvidia), nvidia);
        // An NVENC probe that failed does not hand an NVIDIA GPU to Media Foundation.
        let mut nv_mf = enc(EncoderBackend::MediaFoundation, &[(H264, 4096, 4096)]);
        nv_mf.adapter = "RTX".into();
        assert_eq!(
            backends_on(&[nv_mf.clone()], "RTX", H264, 1920, 1080, &nvidia),
            nvidia
        );
        assert_eq!(
            backends_on(&[nv, nv_mf], "RTX", H264, 1920, 1080, &nvidia),
            nvidia
        );
    }
}
