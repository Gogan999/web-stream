//! H.264 parameter sets and VA-API parameter buffers: one slice, P frames referencing the previous
//! frame, POC type 2, BT.709 limited range.

use super::bits::{BitWriter, nal};
use super::encoder::{Codec, Pic, Stream};
use super::sys::*;
use super::{Buffer, Display, VAContextID};
use crate::Result;

const LOG2_MAX_FRAME_NUM: u32 = 16;

pub(crate) struct H264 {
    profile_idc: u8,
    constraints: u8,
    level_idc: u8,
    width_mbs: u32,
    height_mbs: u32,
    crop_right: u32,
    crop_bottom: u32,
    cabac: bool,
    transform_8x8: bool,
    packed_sequence: bool,
    qp: Option<u8>,
    frame_num: u32,
    idr_id: u16,
}

impl H264 {
    pub(crate) fn new(profile: VAProfile, stream: &Stream, packed: u32) -> Self {
        let (profile_idc, constraints, high) = match profile {
            VAProfileH264High => (100, 0x0c, true),
            VAProfileH264Main => (77, 0x00, false),
            _ => (66, 0xc0, false),
        };
        let width_mbs = stream.width.div_ceil(16);
        let height_mbs = stream.height.div_ceil(16);
        Self {
            profile_idc,
            constraints,
            level_idc: if width_mbs * height_mbs <= 36_864 {
                52
            } else {
                62
            },
            width_mbs,
            height_mbs,
            crop_right: width_mbs * 16 - stream.width,
            crop_bottom: height_mbs * 16 - stream.height,
            cabac: profile_idc != 66,
            transform_8x8: high,
            packed_sequence: packed & VA_ENC_PACKED_HEADER_SEQUENCE != 0,
            qp: stream.qp,
            frame_num: 0,
            idr_id: 0,
        }
    }

    /// `seq_parameter_set_rbsp()` as an Annex-B NAL unit.
    pub(crate) fn sps(&self) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.u(8, u64::from(self.profile_idc));
        w.u(8, u64::from(self.constraints));
        w.u(8, u64::from(self.level_idc));
        w.ue(0);
        if self.profile_idc == 100 {
            w.ue(1);
            w.ue(0);
            w.ue(0);
            w.flag(false);
            w.flag(false);
        }
        w.ue(LOG2_MAX_FRAME_NUM - 4);
        w.ue(2);
        w.ue(1);
        w.flag(false);
        w.ue(self.width_mbs - 1);
        w.ue(self.height_mbs - 1);
        w.flag(true);
        w.flag(true);
        let crop = self.crop_right != 0 || self.crop_bottom != 0;
        w.flag(crop);
        if crop {
            w.ue(0);
            w.ue(self.crop_right / 2);
            w.ue(0);
            w.ue(self.crop_bottom / 2);
        }
        w.flag(true);
        // VUI: square pixels, BT.709 limited range, no reordering.
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
        w.flag(true);
        w.flag(true);
        w.ue(0);
        w.ue(0);
        w.ue(15);
        w.ue(15);
        w.ue(0);
        w.ue(1);
        w.trailing();
        nal(&[0x67], w.bytes())
    }

    /// `pic_parameter_set_rbsp()` as an Annex-B NAL unit.
    pub(crate) fn pps(&self) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.ue(0);
        w.ue(0);
        w.flag(self.cabac);
        w.flag(false);
        w.ue(0);
        w.ue(0);
        w.ue(0);
        w.flag(false);
        w.u(2, 0);
        w.se(i32::from(self.init_qp()) - 26);
        w.se(0);
        w.se(0);
        w.flag(true);
        w.flag(false);
        w.flag(false);
        if self.profile_idc == 100 {
            w.flag(self.transform_8x8);
            w.flag(false);
            w.se(0);
        }
        w.trailing();
        nal(&[0x68], w.bytes())
    }

    fn init_qp(&self) -> u8 {
        self.qp.unwrap_or(26)
    }

    fn sequence(&self, stream: &Stream) -> VAEncSequenceParameterBufferH264 {
        // SAFETY: plain data; zero is valid for every field.
        let mut s: VAEncSequenceParameterBufferH264 = unsafe { std::mem::zeroed() };
        s.level_idc = self.level_idc;
        s.intra_period = 0x7fff;
        s.intra_idr_period = 0x7fff;
        s.ip_period = 1;
        s.bits_per_second = stream.bits_per_second();
        s.max_num_ref_frames = 1;
        s.picture_width_in_mbs = self.width_mbs as u16;
        s.picture_height_in_mbs = self.height_mbs as u16;
        unsafe {
            let f = &mut s.seq_fields.bits;
            f.set_chroma_format_idc(1);
            f.set_frame_mbs_only_flag(1);
            f.set_direct_8x8_inference_flag(1);
            f.set_log2_max_frame_num_minus4(LOG2_MAX_FRAME_NUM - 4);
            f.set_pic_order_cnt_type(2);
        }
        if self.crop_right != 0 || self.crop_bottom != 0 {
            s.frame_cropping_flag = 1;
            s.frame_crop_right_offset = self.crop_right / 2;
            s.frame_crop_bottom_offset = self.crop_bottom / 2;
        }
        s.vui_parameters_present_flag = 1;
        unsafe {
            let v = &mut s.vui_fields.bits;
            v.set_aspect_ratio_info_present_flag(1);
            v.set_bitstream_restriction_flag(1);
            v.set_motion_vectors_over_pic_boundaries_flag(1);
            v.set_log2_max_mv_length_horizontal(15);
            v.set_log2_max_mv_length_vertical(15);
        }
        s.aspect_ratio_idc = 1;
        s
    }
}

fn invalid_pic() -> VAPictureH264 {
    VAPictureH264 {
        picture_id: VA_INVALID_SURFACE,
        flags: VA_PICTURE_H264_INVALID,
        ..Default::default()
    }
}

impl Codec for H264 {
    fn packed_headers(&self) -> u32 {
        if self.packed_sequence {
            VA_ENC_PACKED_HEADER_SEQUENCE
        } else {
            0
        }
    }

    fn picture<'d>(
        &mut self,
        display: &'d Display,
        context: VAContextID,
        stream: &Stream,
        pic: &Pic,
        out: &mut Vec<Buffer<'d>>,
    ) -> Result<()> {
        if pic.idr {
            self.frame_num = 0;
            out.push(display.buffer(
                context,
                VAEncSequenceParameterBufferType,
                &self.sequence(stream),
            )?);
            if self.packed_sequence {
                let mut data = self.sps();
                data.extend_from_slice(&self.pps());
                super::encoder::packed(
                    display,
                    context,
                    VAEncPackedHeaderSequence,
                    &data,
                    data.len() * 8,
                    out,
                )?;
            }
        }
        let max_frame_num = 1u32 << LOG2_MAX_FRAME_NUM;
        let frame_num = self.frame_num % max_frame_num;
        let poc = (pic.since_idr as i32).wrapping_mul(2);
        let current = VAPictureH264 {
            picture_id: pic.recon,
            frame_idx: frame_num,
            flags: 0,
            TopFieldOrderCnt: poc,
            BottomFieldOrderCnt: poc,
            ..Default::default()
        };
        let reference = pic.reference.filter(|_| !pic.idr).map(|r| VAPictureH264 {
            picture_id: r,
            frame_idx: (frame_num + max_frame_num - 1) % max_frame_num,
            flags: VA_PICTURE_H264_SHORT_TERM_REFERENCE,
            TopFieldOrderCnt: poc - 2,
            BottomFieldOrderCnt: poc - 2,
            ..Default::default()
        });

        // SAFETY: plain data; zero is valid for every field.
        let mut p: VAEncPictureParameterBufferH264 = unsafe { std::mem::zeroed() };
        p.CurrPic = current;
        p.ReferenceFrames = [invalid_pic(); 16];
        if let Some(r) = reference {
            p.ReferenceFrames[0] = r;
        }
        p.coded_buf = pic.coded;
        p.frame_num = frame_num as u16;
        p.pic_init_qp = self.init_qp();
        unsafe {
            let f = &mut p.pic_fields.bits;
            f.set_idr_pic_flag(u32::from(pic.idr));
            f.set_reference_pic_flag(1);
            f.set_entropy_coding_mode_flag(u32::from(self.cabac));
            f.set_transform_8x8_mode_flag(u32::from(self.transform_8x8));
            f.set_deblocking_filter_control_present_flag(1);
        }
        out.push(display.buffer(context, VAEncPictureParameterBufferType, &p)?);

        let mut s = VAEncSliceParameterBufferH264 {
            num_macroblocks: self.width_mbs * self.height_mbs,
            macroblock_info: VA_INVALID_ID,
            slice_type: if pic.idr { 2 } else { 0 },
            idr_pic_id: self.idr_id,
            RefPicList0: [invalid_pic(); 32],
            RefPicList1: [invalid_pic(); 32],
            ..Default::default()
        };
        if let Some(r) = reference {
            s.RefPicList0[0] = r;
        }
        out.push(display.buffer(context, VAEncSliceParameterBufferType, &s)?);

        if pic.idr {
            self.idr_id = self.idr_id.wrapping_add(1);
        }
        self.frame_num = self.frame_num.wrapping_add(1);
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn stream(width: u32, height: u32) -> Stream {
        Stream {
            width,
            height,
            fps: 60,
            kbps: 10_000,
            vbv_frames: 1.0,
            qp: None,
            quality_level: None,
        }
    }

    #[test]
    fn sps_for_1080p_high() {
        let h = H264::new(
            VAProfileH264High,
            &stream(1920, 1080),
            VA_ENC_PACKED_HEADER_SEQUENCE,
        );
        assert_eq!((h.width_mbs, h.height_mbs, h.crop_bottom), (120, 68, 8));
        let sps = h.sps();
        assert_eq!(&sps[..8], &[0, 0, 0, 1, 0x67, 100, 0x0c, 52]);
        let pps = h.pps();
        assert_eq!(&pps[..5], &[0, 0, 0, 1, 0x68]);
    }

    #[test]
    fn level_follows_the_frame_size() {
        assert_eq!(
            H264::new(VAProfileH264High, &stream(3840, 2160), 0).level_idc,
            52
        );
        assert_eq!(
            H264::new(VAProfileH264High, &stream(7680, 4320), 0).level_idc,
            62
        );
    }
}
