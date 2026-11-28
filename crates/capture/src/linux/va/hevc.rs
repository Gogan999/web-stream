//! HEVC parameter sets, slice headers and VA-API parameter buffers: Main profile, one slice, P
//! frames (or generalized P/B where the driver needs it) referencing the previous frame, BT.709
//! limited range.

use super::bits::{BitWriter, nal};
use super::encoder::{Codec, Pic, Stream, packed};
use super::sys::*;
use super::{Buffer, Display, VAContextID};
use crate::Result;

const LOG2_MAX_POC_LSB: u32 = 16;
const NAL_TRAIL_R: u8 = 1;
const NAL_IDR_W_RADL: u8 = 19;
const NAL_VPS: u8 = 32;
const NAL_SPS: u8 = 33;
const NAL_PPS: u8 = 34;
const SLICE_B: u32 = 0;
const SLICE_P: u32 = 1;
const SLICE_I: u32 = 2;

/// What the driver can do, from `VAConfigAttribEncHEVCFeatures` and
/// `VAConfigAttribEncHEVCBlockSizes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Tools {
    pub log2_ctb: u32,
    pub log2_min_cb: u32,
    pub log2_min_tb: u32,
    pub log2_max_tb: u32,
    pub depth_inter: u32,
    pub depth_intra: u32,
    pub amp: bool,
    pub sao: bool,
    pub temporal_mvp: bool,
    pub strong_intra_smoothing: bool,
    pub sign_data_hiding: bool,
    pub constrained_intra_pred: bool,
    pub transform_skip: bool,
    pub cu_qp_delta: bool,
    /// P frames go out as B slices with both lists the same.
    pub p_as_b: bool,
}

impl Tools {
    /// The tools to use, from the raw attribute values (`None` when not reported).
    pub(crate) fn new(
        features: Option<u32>,
        block_sizes: Option<u32>,
        prediction: Option<u32>,
        cqp: bool,
    ) -> Self {
        let field = |v: u32, shift: u32, bits: u32| (v >> shift) & ((1 << bits) - 1);
        let mut t = match block_sizes {
            Some(value) => {
                // SAFETY: both union members are plain bits.
                let b = unsafe { VAConfigAttribValEncHEVCBlockSizes { value }.bits };
                let log2_ctb = b.log2_max_coding_tree_block_size_minus3() + 3;
                let log2_min_cb = (b.log2_min_luma_coding_block_size_minus3() + 3).min(log2_ctb);
                let log2_min_tb = b.log2_min_luma_transform_block_size_minus2() + 2;
                let log2_max_tb = (b.log2_max_luma_transform_block_size_minus2() + 2)
                    .clamp(log2_min_tb, log2_ctb.min(5));
                let depth_limit = log2_ctb - log2_min_tb;
                Tools {
                    log2_ctb,
                    log2_min_cb,
                    log2_min_tb,
                    log2_max_tb,
                    depth_inter: b.max_max_transform_hierarchy_depth_inter().min(depth_limit),
                    depth_intra: b.max_max_transform_hierarchy_depth_intra().min(depth_limit),
                    ..Self::fallback()
                }
            }
            None => Self::fallback(),
        };
        if let Some(f) = features {
            let get = |shift: u32| field(f, shift, 2);
            let on = |shift: u32| get(shift) >= VA_FEATURE_SUPPORTED;
            let required = |shift: u32| get(shift) == VA_FEATURE_REQUIRED;
            t.amp = on(4);
            t.sao = on(6);
            t.temporal_mvp = on(10);
            t.strong_intra_smoothing = on(12);
            t.sign_data_hiding = required(16);
            t.constrained_intra_pred = required(18);
            t.transform_skip = on(20);
            t.cu_qp_delta = required(22) || (on(22) && !cqp);
        } else {
            t.cu_qp_delta = !cqp;
        }
        t.p_as_b = prediction.is_some_and(|p| p & VA_PREDICTION_DIRECTION_BI_NOT_EMPTY != 0);
        t
    }

    fn fallback() -> Self {
        Tools {
            log2_ctb: 5,
            log2_min_cb: 4,
            log2_min_tb: 2,
            log2_max_tb: 5,
            depth_inter: 3,
            depth_intra: 3,
            amp: true,
            sao: false,
            temporal_mvp: true,
            strong_intra_smoothing: false,
            sign_data_hiding: false,
            constrained_intra_pred: false,
            transform_skip: false,
            cu_qp_delta: true,
            p_as_b: false,
        }
    }
}

pub(crate) struct Hevc {
    tools: Tools,
    level_idc: u8,
    /// Coded size, a multiple of the minimum coding block.
    width: u32,
    height: u32,
    crop_right: u32,
    crop_bottom: u32,
    ctus: u32,
    packed: u32,
    qp: Option<u8>,
}

impl Hevc {
    pub(crate) fn new(stream: &Stream, tools: Tools, packed: u32) -> Self {
        let min_cb = 1 << tools.log2_min_cb;
        let ctb = 1 << tools.log2_ctb;
        let width = stream.width.next_multiple_of(min_cb);
        let height = stream.height.next_multiple_of(min_cb);
        let luma = u64::from(width) * u64::from(height);
        Self {
            tools,
            level_idc: if luma <= 8_912_896 { 153 } else { 183 },
            width,
            height,
            crop_right: width - stream.width,
            crop_bottom: height - stream.height,
            ctus: width.div_ceil(ctb) * height.div_ceil(ctb),
            packed: packed & (VA_ENC_PACKED_HEADER_SEQUENCE | VA_ENC_PACKED_HEADER_SLICE),
            qp: stream.qp,
        }
    }

    fn init_qp(&self) -> u8 {
        self.qp.unwrap_or(26)
    }

    fn profile_tier_level(&self, w: &mut BitWriter) {
        w.u(2, 0);
        w.flag(false);
        w.u(5, 1);
        // Compatible with Main and Main 10.
        w.u(32, 0x6000_0000);
        w.flag(true);
        w.flag(false);
        w.flag(false);
        w.flag(true);
        w.u(43, 0);
        w.flag(false);
        w.u(8, u64::from(self.level_idc));
    }

    pub(crate) fn vps(&self) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.u(4, 0);
        w.flag(true);
        w.flag(true);
        w.u(6, 0);
        w.u(3, 0);
        w.flag(true);
        w.u(16, 0xffff);
        self.profile_tier_level(&mut w);
        w.flag(false);
        w.ue(1);
        w.ue(0);
        w.ue(0);
        w.u(6, 0);
        w.ue(0);
        w.flag(false);
        w.flag(false);
        w.trailing();
        nal(&nal_header(NAL_VPS), w.bytes())
    }

    pub(crate) fn sps(&self) -> Vec<u8> {
        let t = &self.tools;
        let mut w = BitWriter::new();
        w.u(4, 0);
        w.u(3, 0);
        w.flag(true);
        self.profile_tier_level(&mut w);
        w.ue(0);
        w.ue(1);
        w.ue(self.width);
        w.ue(self.height);
        let crop = self.crop_right != 0 || self.crop_bottom != 0;
        w.flag(crop);
        if crop {
            w.ue(0);
            w.ue(self.crop_right / 2);
            w.ue(0);
            w.ue(self.crop_bottom / 2);
        }
        w.ue(0);
        w.ue(0);
        w.ue(LOG2_MAX_POC_LSB - 4);
        w.flag(false);
        w.ue(1);
        w.ue(0);
        w.ue(0);
        w.ue(t.log2_min_cb - 3);
        w.ue(t.log2_ctb - t.log2_min_cb);
        w.ue(t.log2_min_tb - 2);
        w.ue(t.log2_max_tb - t.log2_min_tb);
        w.ue(t.depth_inter);
        w.ue(t.depth_intra);
        w.flag(false);
        w.flag(t.amp);
        w.flag(t.sao);
        w.flag(false);
        w.ue(0);
        w.flag(false);
        w.flag(t.temporal_mvp);
        w.flag(t.strong_intra_smoothing);
        w.flag(true);
        // VUI: square pixels, BT.709 limited range.
        w.flag(true);
        w.u(8, 1);
        w.flag(false);
        w.flag(true);
        w.u(3, 5);
        w.flag(false);
        w.flag(true);
        w.u(8, 1);
        w.u(8, 1);
        w.u(8, 1);
        w.flag(false);
        w.flag(false);
        w.flag(false);
        w.flag(false);
        w.flag(false);
        w.flag(false);
        w.flag(true);
        w.flag(false);
        w.flag(true);
        w.flag(true);
        w.ue(0);
        w.ue(0);
        w.ue(0);
        w.ue(15);
        w.ue(15);
        w.flag(false);
        w.trailing();
        nal(&nal_header(NAL_SPS), w.bytes())
    }

    pub(crate) fn pps(&self) -> Vec<u8> {
        let t = &self.tools;
        let mut w = BitWriter::new();
        w.ue(0);
        w.ue(0);
        w.flag(false);
        w.flag(false);
        w.u(3, 0);
        w.flag(t.sign_data_hiding);
        w.flag(false);
        w.ue(0);
        w.ue(0);
        w.se(i32::from(self.init_qp()) - 26);
        w.flag(t.constrained_intra_pred);
        w.flag(t.transform_skip);
        w.flag(t.cu_qp_delta);
        if t.cu_qp_delta {
            w.ue(0);
        }
        w.se(0);
        w.se(0);
        w.flag(false);
        w.flag(false);
        w.flag(false);
        w.flag(false);
        w.flag(false);
        w.flag(false);
        w.flag(false);
        w.flag(false);
        w.flag(false);
        w.flag(false);
        w.ue(0);
        w.flag(false);
        w.flag(false);
        w.trailing();
        nal(&nal_header(NAL_PPS), w.bytes())
    }

    /// `slice_segment_header()` up to and including `byte_alignment()`, as a NAL unit, and its
    /// length in bits.
    pub(crate) fn slice_header(&self, idr: bool, poc: u32) -> (Vec<u8>, usize) {
        let t = &self.tools;
        let slice_type = self.slice_type(idr);
        let mut w = BitWriter::new();
        w.flag(true);
        if idr {
            w.flag(false);
        }
        w.ue(0);
        w.ue(slice_type);
        if !idr {
            w.u(
                LOG2_MAX_POC_LSB,
                u64::from(poc) & ((1 << LOG2_MAX_POC_LSB) - 1),
            );
            w.flag(false);
            w.ue(1);
            w.ue(0);
            w.ue(0);
            w.flag(true);
            if t.temporal_mvp {
                w.flag(true);
            }
        }
        if t.sao {
            w.flag(true);
            w.flag(true);
        }
        if slice_type != SLICE_I {
            w.flag(false);
            if slice_type == SLICE_B {
                w.flag(false);
            }
            if t.temporal_mvp && slice_type == SLICE_B {
                w.flag(true);
            }
            w.ue(0);
        }
        w.se(0);
        w.trailing();
        let data = nal(
            &nal_header(if idr { NAL_IDR_W_RADL } else { NAL_TRAIL_R }),
            w.bytes(),
        );
        let bits = data.len() * 8;
        (data, bits)
    }

    fn slice_type(&self, idr: bool) -> u32 {
        if idr {
            SLICE_I
        } else if self.tools.p_as_b {
            SLICE_B
        } else {
            SLICE_P
        }
    }

    fn sequence(&self, stream: &Stream) -> VAEncSequenceParameterBufferHEVC {
        let t = &self.tools;
        // SAFETY: plain data; zero is valid for every field.
        let mut s: VAEncSequenceParameterBufferHEVC = unsafe { std::mem::zeroed() };
        s.general_profile_idc = 1;
        s.general_level_idc = self.level_idc;
        s.intra_period = 0x7fff;
        s.intra_idr_period = 0x7fff;
        s.ip_period = 1;
        s.bits_per_second = stream.bits_per_second();
        s.pic_width_in_luma_samples = self.width as u16;
        s.pic_height_in_luma_samples = self.height as u16;
        unsafe {
            let f = &mut s.seq_fields.bits;
            f.set_chroma_format_idc(1);
            f.set_amp_enabled_flag(u32::from(t.amp));
            f.set_sample_adaptive_offset_enabled_flag(u32::from(t.sao));
            f.set_sps_temporal_mvp_enabled_flag(u32::from(t.temporal_mvp));
            f.set_strong_intra_smoothing_enabled_flag(u32::from(t.strong_intra_smoothing));
            f.set_low_delay_seq(1);
        }
        s.log2_min_luma_coding_block_size_minus3 = (t.log2_min_cb - 3) as u8;
        s.log2_diff_max_min_luma_coding_block_size = (t.log2_ctb - t.log2_min_cb) as u8;
        s.log2_min_transform_block_size_minus2 = (t.log2_min_tb - 2) as u8;
        s.log2_diff_max_min_transform_block_size = (t.log2_max_tb - t.log2_min_tb) as u8;
        s.max_transform_hierarchy_depth_inter = t.depth_inter as u8;
        s.max_transform_hierarchy_depth_intra = t.depth_intra as u8;
        s.vui_parameters_present_flag = 1;
        unsafe {
            let v = &mut s.vui_fields.bits;
            v.set_aspect_ratio_info_present_flag(1);
            v.set_bitstream_restriction_flag(1);
            v.set_motion_vectors_over_pic_boundaries_flag(1);
            v.set_restricted_ref_pic_lists_flag(1);
            v.set_log2_max_mv_length_horizontal(15);
            v.set_log2_max_mv_length_vertical(15);
        }
        s.aspect_ratio_idc = 1;
        s
    }
}

fn nal_header(kind: u8) -> [u8; 2] {
    [kind << 1, 1]
}

fn invalid_pic() -> VAPictureHEVC {
    VAPictureHEVC {
        picture_id: VA_INVALID_SURFACE,
        flags: VA_PICTURE_HEVC_INVALID,
        ..Default::default()
    }
}

impl Codec for Hevc {
    fn packed_headers(&self) -> u32 {
        self.packed
    }

    fn picture<'d>(
        &mut self,
        display: &'d Display,
        context: VAContextID,
        stream: &Stream,
        pic: &Pic,
        out: &mut Vec<Buffer<'d>>,
    ) -> Result<()> {
        let t = self.tools;
        let poc = pic.since_idr as u32;
        if pic.idr {
            out.push(display.buffer(
                context,
                VAEncSequenceParameterBufferType,
                &self.sequence(stream),
            )?);
            if self.packed & VA_ENC_PACKED_HEADER_SEQUENCE != 0 {
                let mut data = self.vps();
                data.extend_from_slice(&self.sps());
                data.extend_from_slice(&self.pps());
                packed(
                    display,
                    context,
                    VAEncPackedHeaderSequence,
                    &data,
                    data.len() * 8,
                    out,
                )?;
            }
        }
        let slice_type = self.slice_type(pic.idr);
        let reference = pic.reference.filter(|_| !pic.idr).map(|r| VAPictureHEVC {
            picture_id: r,
            pic_order_cnt: poc as i32 - 1,
            flags: 0,
            ..Default::default()
        });

        // SAFETY: plain data; zero is valid for every field.
        let mut p: VAEncPictureParameterBufferHEVC = unsafe { std::mem::zeroed() };
        p.decoded_curr_pic = VAPictureHEVC {
            picture_id: pic.recon,
            pic_order_cnt: poc as i32,
            flags: 0,
            ..Default::default()
        };
        p.reference_frames = [invalid_pic(); 15];
        if let Some(r) = reference {
            p.reference_frames[0] = r;
        }
        p.coded_buf = pic.coded;
        p.collocated_ref_pic_index = if t.temporal_mvp && reference.is_some() {
            0
        } else {
            0xff
        };
        p.pic_init_qp = self.init_qp();
        p.nal_unit_type = if pic.idr { NAL_IDR_W_RADL } else { NAL_TRAIL_R };
        unsafe {
            let f = &mut p.pic_fields.bits;
            f.set_idr_pic_flag(u32::from(pic.idr));
            f.set_coding_type(if pic.idr { 1 } else { 2 });
            f.set_reference_pic_flag(1);
            f.set_sign_data_hiding_enabled_flag(u32::from(t.sign_data_hiding));
            f.set_constrained_intra_pred_flag(u32::from(t.constrained_intra_pred));
            f.set_transform_skip_enabled_flag(u32::from(t.transform_skip));
            f.set_cu_qp_delta_enabled_flag(u32::from(t.cu_qp_delta));
        }
        out.push(display.buffer(context, VAEncPictureParameterBufferType, &p)?);

        if self.packed & VA_ENC_PACKED_HEADER_SLICE != 0 {
            let (data, bits) = self.slice_header(pic.idr, poc);
            packed(display, context, VAEncPackedHeaderSlice, &data, bits, out)?;
        }

        // SAFETY: plain data; zero is valid for every field.
        let mut s: VAEncSliceParameterBufferHEVC = unsafe { std::mem::zeroed() };
        s.num_ctu_in_slice = self.ctus;
        s.slice_type = slice_type as u8;
        s.ref_pic_list0 = [invalid_pic(); 15];
        s.ref_pic_list1 = [invalid_pic(); 15];
        if let Some(r) = reference {
            s.ref_pic_list0[0] = r;
            if slice_type == SLICE_B {
                s.ref_pic_list1[0] = r;
            }
        }
        s.max_num_merge_cand = 5;
        unsafe {
            let f = &mut s.slice_fields.bits;
            f.set_last_slice_of_pic_flag(1);
            f.set_slice_sao_luma_flag(u32::from(t.sao));
            f.set_slice_sao_chroma_flag(u32::from(t.sao));
            f.set_slice_temporal_mvp_enabled_flag(u32::from(t.temporal_mvp && !pic.idr));
            f.set_collocated_from_l0_flag(1);
        }
        out.push(display.buffer(context, VAEncSliceParameterBufferType, &s)?);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::h264::tests::stream;
    use super::*;

    #[test]
    fn tools_from_attributes() {
        let t = Tools::new(None, None, None, false);
        assert_eq!(
            (t.log2_ctb, t.log2_min_cb, t.cu_qp_delta, t.p_as_b),
            (5, 4, true, false)
        );
        // amp, sao, temporal mvp supported; cu_qp_delta required.
        let features = (1 << 4) | (1 << 6) | (1 << 10) | (2 << 22);
        // CTB 64, min CB 8, TB 4..32, inter and intra depth up to 2.
        let sizes = 3 | (3 << 6) | (2 << 10) | (2 << 14);
        let t = Tools::new(
            Some(features),
            Some(sizes),
            Some(VA_PREDICTION_DIRECTION_BI_NOT_EMPTY),
            true,
        );
        assert!(t.amp && t.sao && t.temporal_mvp && t.cu_qp_delta && t.p_as_b);
        assert!(!t.transform_skip && !t.sign_data_hiding);
        assert_eq!(
            (t.log2_ctb, t.log2_min_cb, t.log2_min_tb, t.log2_max_tb),
            (6, 3, 2, 5)
        );
        assert_eq!((t.depth_inter, t.depth_intra), (2, 2));
    }

    #[test]
    fn sizes_and_level() {
        let h = Hevc::new(&stream(1920, 1080), Tools::fallback(), 0);
        assert_eq!(
            (h.width, h.height, h.crop_bottom, h.ctus),
            (1920, 1088, 8, 60 * 34)
        );
        assert_eq!(h.level_idc, 153);
        assert_eq!(
            Hevc::new(&stream(7680, 4320), Tools::fallback(), 0).level_idc,
            183
        );
        assert_eq!(&h.vps()[..6], &[0, 0, 0, 1, 0x40, 0x01]);
        assert_eq!(&h.sps()[..6], &[0, 0, 0, 1, 0x42, 0x01]);
        assert_eq!(&h.pps()[..6], &[0, 0, 0, 1, 0x44, 0x01]);
    }
}
