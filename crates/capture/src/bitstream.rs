//! Encoded bitstream inspection and fix-ups, kept free of platform calls.

use ws_proto::VideoCodec;

/// The NAL units of an Annex-B stream, without their start codes.
pub(crate) fn nal_units(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = match find_start_code(data) {
        Some((at, len)) => &data[at + len..],
        None => &[][..],
    };
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let (unit, next) = match find_start_code(rest) {
            Some((at, len)) => (&rest[..at], &rest[at + len..]),
            None => (rest, &[][..]),
        };
        rest = next;
        Some(unit)
    })
}

/// The offset and length of the first start code (`00 00 01` or `00 00 00 01`) in `data`.
fn find_start_code(data: &[u8]) -> Option<(usize, usize)> {
    let at = data.windows(3).position(|w| w == [0, 0, 1])?;
    Some(if at > 0 && data[at - 1] == 0 {
        (at - 1, 4)
    } else {
        (at, 3)
    })
}

/// Whether `data` begins with an Annex-B start code.
pub(crate) fn is_annex_b(data: &[u8]) -> bool {
    data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1])
}

/// The NAL unit type of an H.264 or HEVC NAL unit.
fn nal_type(codec: VideoCodec, unit: &[u8]) -> Option<u8> {
    let first = *unit.first()?;
    match codec {
        VideoCodec::H264 => Some(first & 0x1f),
        VideoCodec::H265 => Some((first >> 1) & 0x3f),
        VideoCodec::Av1 => None,
    }
}

/// The NAL unit types of an Annex-B stream in order, read without scanning to each unit's end.
fn nal_types(codec: VideoCodec, data: &[u8]) -> impl Iterator<Item = u8> + '_ {
    let mut at = 0;
    std::iter::from_fn(move || {
        let (start, len) = find_start_code(&data[at..])?;
        let header = at + start + len;
        at = header;
        nal_type(codec, &data[header..])
    })
}

/// Whether `kind` is a coded slice (a VCL NAL unit).
fn is_slice(codec: VideoCodec, kind: u8) -> bool {
    match codec {
        VideoCodec::H264 => matches!(kind, 1..=5),
        VideoCodec::H265 => kind <= 31,
        VideoCodec::Av1 => false,
    }
}

/// Whether an H.264 or HEVC access unit is an IDR picture.
pub(crate) fn is_idr(codec: VideoCodec, data: &[u8]) -> bool {
    nal_types(codec, data)
        .find(|&kind| is_slice(codec, kind))
        .is_some_and(|kind| match codec {
            VideoCodec::H264 => kind == 5,
            VideoCodec::H265 => matches!(kind, 19 | 20),
            VideoCodec::Av1 => false,
        })
}

/// Whether an H.264 or HEVC access unit carries a sequence parameter set before its first slice.
fn has_parameter_sets(codec: VideoCodec, data: &[u8]) -> bool {
    nal_types(codec, data)
        .take_while(|&kind| !is_slice(codec, kind))
        .any(|kind| match codec {
            VideoCodec::H264 => kind == 7,
            VideoCodec::H265 => kind == 33,
            VideoCodec::Av1 => false,
        })
}

/// AV1 OBU types used here.
const OBU_SEQUENCE_HEADER: u8 = 1;
const OBU_TEMPORAL_DELIMITER: u8 = 2;

/// The type and total length of each OBU in a low-overhead AV1 stream; `None` if it does not parse.
fn obus(data: &[u8]) -> Option<Vec<(u8, usize)>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < data.len() {
        let header = data[at];
        if header & 0x80 != 0 || header & 0x02 == 0 {
            return None;
        }
        let obu_type = (header >> 3) & 0x0f;
        let mut len = 1 + usize::from(header & 0x04 != 0);
        let (size, leb_len) = leb128(data.get(at + len..)?)?;
        len += leb_len + usize::try_from(size).ok()?;
        if at + len > data.len() {
            return None;
        }
        out.push((obu_type, len));
        at += len;
    }
    Some(out)
}

/// A LEB128 value and how many bytes it took.
fn leb128(data: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0u64;
    for (i, byte) in data.iter().take(8).enumerate() {
        value |= u64::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            return Some((value, i + 1));
        }
    }
    None
}

/// The OBUs of an AV1 sequence header as an encoder reports it: bare OBUs, or an
/// `AV1CodecConfigurationRecord` that wraps them.
fn av1_config_obus(header: &[u8]) -> &[u8] {
    if header.len() > 4 && header[0] == 0x81 {
        &header[4..]
    } else {
        header
    }
}

/// A keyframe made decodable on its own: when it lacks the stream's parameter sets (sequence header
/// for AV1), returns it with `header` put in front (after the temporal delimiter for AV1). `None`
/// when nothing needs adding or `header` is unusable.
pub(crate) fn with_parameter_sets(
    codec: VideoCodec,
    frame: &[u8],
    header: &[u8],
) -> Option<Vec<u8>> {
    match codec {
        VideoCodec::H264 | VideoCodec::H265 => {
            if has_parameter_sets(codec, frame)
                || !is_annex_b(header)
                || !has_parameter_sets(codec, header)
            {
                return None;
            }
            Some([header, frame].concat())
        }
        VideoCodec::Av1 => {
            let units = obus(frame)?;
            if units.iter().any(|(t, _)| *t == OBU_SEQUENCE_HEADER) {
                return None;
            }
            let header = av1_config_obus(header);
            let parsed = obus(header)?;
            if !parsed.iter().any(|(t, _)| *t == OBU_SEQUENCE_HEADER) {
                return None;
            }
            let split = match units.first() {
                Some((OBU_TEMPORAL_DELIMITER, len)) => *len,
                _ => 0,
            };
            Some([&frame[..split], header, &frame[split..]].concat())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: &[u8] = &[0, 0, 0, 1, 0x67, 0x64, 0x00, 0x28];
    const PPS: &[u8] = &[0, 0, 0, 1, 0x68, 0xee, 0x3c, 0x80];
    const IDR: &[u8] = &[0, 0, 1, 0x65, 0x88, 0x84];
    const P: &[u8] = &[0, 0, 0, 1, 0x41, 0x9a, 0x02];

    #[test]
    fn splits_annex_b_with_both_start_code_lengths() {
        let data = [SPS, PPS, IDR].concat();
        let units: Vec<&[u8]> = nal_units(&data).collect();
        assert_eq!(units.len(), 3);
        assert_eq!(units[0], &SPS[4..]);
        assert_eq!(units[2], &IDR[3..]);
        assert_eq!(nal_units(&[1, 2, 3]).count(), 0);
    }

    #[test]
    fn finds_h264_idr() {
        assert!(is_idr(VideoCodec::H264, &[SPS, PPS, IDR].concat()));
        assert!(!is_idr(VideoCodec::H264, P));
        assert!(!is_idr(VideoCodec::H264, &[]));
    }

    #[test]
    fn finds_hevc_idr() {
        let vps = [0, 0, 0, 1, 0x40, 0x01];
        let idr = [0, 0, 0, 1, 0x26, 0x01, 0xaf];
        let trail = [0, 0, 0, 1, 0x02, 0x01, 0xd0];
        assert!(is_idr(VideoCodec::H265, &[&vps[..], &idr].concat()));
        assert!(!is_idr(VideoCodec::H265, &trail));
    }

    #[test]
    fn h264_keyframe_gains_missing_parameter_sets() {
        let header = [SPS, PPS].concat();
        assert_eq!(
            with_parameter_sets(VideoCodec::H264, IDR, &header),
            Some([SPS, PPS, IDR].concat())
        );
        let complete = [SPS, PPS, IDR].concat();
        assert_eq!(
            with_parameter_sets(VideoCodec::H264, &complete, &header),
            None
        );
        assert_eq!(with_parameter_sets(VideoCodec::H264, IDR, &[]), None);
        assert_eq!(with_parameter_sets(VideoCodec::H264, IDR, &[1, 2, 3]), None);
    }

    #[test]
    fn hevc_keyframe_gains_missing_parameter_sets() {
        let header = [
            &[0, 0, 0, 1, 0x40, 0x01, 0x0c][..],
            &[0, 0, 0, 1, 0x42, 0x01, 0x01],
            &[0, 0, 0, 1, 0x44, 0x01, 0xc1],
        ]
        .concat();
        let idr = [0, 0, 0, 1, 0x26, 0x01, 0xaf];
        let fixed = with_parameter_sets(VideoCodec::H265, &idr, &header).unwrap();
        assert!(fixed.starts_with(&header));
        assert!(fixed.ends_with(&idr));
    }

    const TD: &[u8] = &[0x12, 0x00];
    const SEQ: &[u8] = &[0x0a, 0x03, 0x00, 0x00, 0x00];
    const FRAME: &[u8] = &[0x32, 0x02, 0xaa, 0xbb];

    #[test]
    fn parses_low_overhead_obus() {
        let data = [TD, SEQ, FRAME].concat();
        assert_eq!(obus(&data), Some(vec![(2, 2), (1, 5), (6, 4)]));
        assert_eq!(obus(&[0x32, 0x05, 0xaa]), None);
        assert_eq!(obus(&[0x30, 0x00]), None);
    }

    #[test]
    fn av1_keyframe_gains_sequence_header_after_temporal_delimiter() {
        let frame = [TD, FRAME].concat();
        assert_eq!(
            with_parameter_sets(VideoCodec::Av1, &frame, SEQ),
            Some([TD, SEQ, FRAME].concat())
        );
        let av1c = [&[0x81, 0x00, 0x0c, 0x00][..], SEQ].concat();
        assert_eq!(
            with_parameter_sets(VideoCodec::Av1, FRAME, &av1c),
            Some([SEQ, FRAME].concat())
        );
        let complete = [TD, SEQ, FRAME].concat();
        assert_eq!(with_parameter_sets(VideoCodec::Av1, &complete, SEQ), None);
        assert_eq!(with_parameter_sets(VideoCodec::Av1, FRAME, FRAME), None);
    }

    #[test]
    fn leb128_values() {
        assert_eq!(leb128(&[0x05]), Some((5, 1)));
        assert_eq!(leb128(&[0x80, 0x01]), Some((128, 2)));
        assert_eq!(leb128(&[0x80]), None);
    }

    #[test]
    #[ignore = "timing; run with --ignored --nocapture"]
    fn idr_scan_cost() {
        let mut frame = vec![0, 0, 0, 1, 0x09, 0xf0, 0, 0, 1, 0x41];
        frame.extend((0..200_000u32).map(|i| (i % 251) as u8 | 1));
        let runs = 2000;
        let t = std::time::Instant::now();
        let mut n = 0;
        for _ in 0..runs {
            n += usize::from(std::hint::black_box(is_idr(VideoCodec::H264, &frame)));
        }
        let per = t.elapsed() / runs;
        eprintln!("is_idr on a 200 KB P-frame: {per:?} per frame ({n})");
    }
}
