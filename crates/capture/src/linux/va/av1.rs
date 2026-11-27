//! AV1 sequence and frame header OBUs and VA-API parameter buffers: profile 0, 8-bit 4:2:0, 64x64
//! superblocks, key frames and inter frames referencing the previous frame, BT.709 limited range.
//! The driver rewrites the quantizer, loop filter and CDEF fields it chooses at the bit offsets
//! given.

use super::bits::{BitWriter, leb128};
use super::encoder::{Codec, Pic, Stream, packed};
use super::sys::*;
use super::{Buffer, Display, VAContextID};
use crate::{CaptureError, Result};

const OBU_SEQUENCE_HEADER: u8 = 1;
const OBU_FRAME_HEADER: u8 = 3;
const PRIMARY_REF_NONE: u8 = 7;
const ORDER_HINT_BITS: u32 = 8;
const TX_MODE_LARGEST: u32 = 1;
const TX_MODE_SELECT: u32 = 2;
const MAX_TILE_WIDTH_SB: u32 = 4096 >> 6;
const MAX_TILE_AREA_SB: u32 = (4096 * 2304) >> 12;
const MAX_TILE_COLS: u32 = 64;
const MAX_TILE_ROWS: u32 = 64;
const LOOP_FILTER: [u8; 4] = [8, 8, 4, 4];

/// Whether the driver takes what this encoder sends: packed frame headers.
pub(crate) fn supported(display: &Display, profile: VAProfile, entrypoint: VAEntrypoint) -> bool {
    display
        .attribute(profile, entrypoint, VAConfigAttribEncPackedHeaders)
        .is_some_and(|p| p & VA_ENC_PACKED_HEADER_PICTURE != 0)
}

/// Uniformly spaced tiles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Tiles {
    pub cols_log2: u32,
    pub rows_log2: u32,
    /// Bounds of `cols_log2` and `rows_log2`, for the increment flags.
    pub max_cols_log2: u32,
    pub max_rows_log2: u32,
    pub min_cols_log2: u32,
    pub min_rows_log2: u32,
    /// Superblocks per tile column and row, the last ones possibly smaller.
    pub widths: Vec<u32>,
    pub heights: Vec<u32>,
}

fn tile_log2(block: u32, target: u32) -> u32 {
    let mut k = 0;
    while (block << k) < target {
        k += 1;
    }
    k
}

impl Tiles {
    /// The fewest uniform tiles the spec allows for a frame of `width` x `height`.
    pub(crate) fn new(width: u32, height: u32) -> Self {
        let mi_cols = 2 * width.div_ceil(8);
        let mi_rows = 2 * height.div_ceil(8);
        let sb_cols = mi_cols.div_ceil(16);
        let sb_rows = mi_rows.div_ceil(16);
        let min_cols_log2 = tile_log2(MAX_TILE_WIDTH_SB, sb_cols);
        let max_cols_log2 = tile_log2(1, sb_cols.min(MAX_TILE_COLS));
        let max_rows_log2 = tile_log2(1, sb_rows.min(MAX_TILE_ROWS));
        let min_tiles_log2 = min_cols_log2.max(tile_log2(MAX_TILE_AREA_SB, sb_rows * sb_cols));
        let cols_log2 = min_cols_log2;
        let min_rows_log2 = min_tiles_log2.saturating_sub(cols_log2);
        let rows_log2 = min_rows_log2;
        let split = |sbs: u32, log2: u32| {
            let size = sbs.div_ceil(1 << log2);
            let mut v = Vec::new();
            let mut start = 0;
            while start < sbs {
                v.push(size.min(sbs - start));
                start += size;
            }
            v
        };
        Self {
            cols_log2,
            rows_log2,
            max_cols_log2,
            max_rows_log2,
            min_cols_log2,
            min_rows_log2,
            widths: split(sb_cols, cols_log2),
            heights: split(sb_rows, rows_log2),
        }
    }

    fn count(&self) -> usize {
        self.widths.len() * self.heights.len()
    }
}

/// A packed frame header and where the driver may rewrite it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Header {
    pub data: Vec<u8>,
    pub bits: usize,
    pub qindex: u32,
    pub segmentation: u32,
    pub loop_filter: u32,
    pub cdef: u32,
    pub cdef_bits: u32,
    pub obu_size_byte: u32,
    pub frame_header_bits: u32,
}

pub(crate) struct Av1 {
    width: u32,
    height: u32,
    level_idx: u8,
    tiles: Tiles,
    tx_mode: u32,
    interpolation_filter: u32,
    obu_size_bytes: usize,
    tile_size_bytes_minus1: u32,
    qp: Option<u8>,
}

impl Av1 {
    pub(crate) fn new(
        display: &Display,
        profile: VAProfile,
        entrypoint: VAEntrypoint,
        stream: &Stream,
        packed: u32,
    ) -> Result<Self> {
        if packed & VA_ENC_PACKED_HEADER_PICTURE == 0 {
            return Err(CaptureError::NoEncoder(
                "the VA-API driver takes no packed AV1 frame headers".into(),
            ));
        }
        let attr = |kind| display.attribute(profile, entrypoint, kind);
        let (interpolation_filter, tx_support, obu_size_bytes, tile_size_bytes_minus1) = {
            // SAFETY: both union members are plain bits.
            let ext1 = attr(VAConfigAttribEncAV1Ext1).map(|value| unsafe {
                VAConfigAttribValEncAV1Ext1 { value }
                    .bits
                    .interpolation_filter()
            });
            let ext2 = attr(VAConfigAttribEncAV1Ext2)
                .map(|value| unsafe { VAConfigAttribValEncAV1Ext2 { value }.bits });
            (
                ext1.filter(|&f| f != 0).map_or(0, |f| f.trailing_zeros()),
                ext2.map_or(0b110, |e| e.tx_mode_support()),
                ext2.map_or(4, |e| e.obu_size_bytes_minus1() as usize + 1),
                ext2.map_or(3, |e| e.tile_size_bytes_minus1()),
            )
        };
        Ok(Self::with(
            stream,
            interpolation_filter,
            if tx_support & 0b100 != 0 {
                TX_MODE_SELECT
            } else {
                TX_MODE_LARGEST
            },
            obu_size_bytes.clamp(1, 8),
            tile_size_bytes_minus1,
        ))
    }

    fn with(
        stream: &Stream,
        interpolation_filter: u32,
        tx_mode: u32,
        obu_size_bytes: usize,
        tile_size_bytes_minus1: u32,
    ) -> Self {
        let luma = u64::from(stream.width) * u64::from(stream.height);
        Self {
            width: stream.width,
            height: stream.height,
            level_idx: if luma <= 8_912_896 { 13 } else { 16 },
            tiles: Tiles::new(stream.width, stream.height),
            tx_mode,
            interpolation_filter,
            obu_size_bytes,
            tile_size_bytes_minus1,
            qp: stream.qp,
        }
    }

    fn base_qindex(&self) -> u8 {
        self.qp.unwrap_or(128)
    }

    /// The sequence header OBU.
    pub(crate) fn sequence_header(&self) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.u(3, 0);
        w.flag(false);
        w.flag(false);
        w.flag(false);
        w.flag(false);
        w.u(5, 0);
        w.u(12, 0);
        w.u(5, u64::from(self.level_idx));
        if self.level_idx > 7 {
            w.flag(false);
        }
        let wbits = bits_for(self.width - 1);
        let hbits = bits_for(self.height - 1);
        w.u(4, u64::from(wbits - 1));
        w.u(4, u64::from(hbits - 1));
        w.u(wbits, u64::from(self.width - 1));
        w.u(hbits, u64::from(self.height - 1));
        w.flag(false);
        w.flag(false);
        w.flag(false);
        w.flag(false);
        w.flag(false);
        w.flag(false);
        w.flag(false);
        w.flag(false);
        w.flag(true);
        w.flag(false);
        w.flag(false);
        w.flag(false);
        w.flag(false);
        w.u(3, u64::from(ORDER_HINT_BITS - 1));
        w.flag(false);
        w.flag(true);
        w.flag(false);
        // color_config: 8-bit, BT.709 limited range, 4:2:0.
        w.flag(false);
        w.flag(false);
        w.flag(true);
        w.u(8, 1);
        w.u(8, 1);
        w.u(8, 1);
        w.flag(false);
        w.u(2, 0);
        w.flag(false);
        w.flag(false);
        w.trailing();
        let payload = w.into_bytes();
        let mut obu = vec![obu_header(OBU_SEQUENCE_HEADER)];
        leb128(payload.len() as u64, &mut obu);
        obu.extend_from_slice(&payload);
        obu
    }

    /// The packed picture header: the sequence header on key frames, then the frame header OBU.
    pub(crate) fn frame_header(&self, key: bool, order_hint: u8) -> Header {
        let prefix = if key {
            self.sequence_header()
        } else {
            Vec::new()
        };
        let base = (prefix.len() + 1 + self.obu_size_bytes) * 8;
        let mut w = BitWriter::new();
        let mut h = Header::default();
        w.flag(false);
        w.u(2, if key { 0 } else { 1 });
        w.flag(true);
        if !key {
            w.flag(false);
        }
        w.flag(false);
        w.flag(false);
        w.u(ORDER_HINT_BITS, u64::from(order_hint));
        if !key {
            w.u(3, u64::from(PRIMARY_REF_NONE));
            w.u(8, 0x01);
            w.flag(false);
            for _ in 0..7 {
                w.u(3, 0);
            }
            w.flag(false);
            w.flag(false);
            let switchable = self.interpolation_filter == 4;
            w.flag(switchable);
            if !switchable {
                w.u(2, u64::from(self.interpolation_filter));
            }
            w.flag(false);
        } else {
            w.flag(false);
        }
        w.flag(true);
        self.tile_info(&mut w);
        h.qindex = (base + w.len()) as u32;
        w.u(8, u64::from(self.base_qindex()));
        w.flag(false);
        w.flag(false);
        w.flag(false);
        w.flag(false);
        h.segmentation = (base + w.len()) as u32;
        w.flag(false);
        w.flag(false);
        h.loop_filter = (base + w.len()) as u32;
        w.u(6, u64::from(LOOP_FILTER[0]));
        w.u(6, u64::from(LOOP_FILTER[1]));
        w.u(6, u64::from(LOOP_FILTER[2]));
        w.u(6, u64::from(LOOP_FILTER[3]));
        w.u(3, 0);
        w.flag(false);
        h.cdef = (base + w.len()) as u32;
        let cdef_start = w.len();
        w.u(2, 0);
        w.u(2, 0);
        w.u(4, 0);
        w.u(2, 0);
        w.u(4, 0);
        w.u(2, 0);
        h.cdef_bits = (w.len() - cdef_start) as u32;
        w.flag(self.tx_mode == TX_MODE_SELECT);
        if !key {
            w.flag(false);
        }
        w.flag(false);
        if !key {
            for _ in 0..7 {
                w.flag(false);
            }
        }
        let header_bits = w.len() + 1;
        w.trailing();
        let payload = w.into_bytes();
        let mut data = prefix;
        data.push(obu_header(OBU_FRAME_HEADER));
        h.obu_size_byte = data.len() as u32;
        padded_leb128(payload.len() as u64, self.obu_size_bytes, &mut data);
        data.extend_from_slice(&payload);
        h.bits = data.len() * 8;
        h.frame_header_bits = ((1 + self.obu_size_bytes) * 8 + header_bits) as u32;
        h.data = data;
        h
    }

    fn tile_info(&self, w: &mut BitWriter) {
        let t = &self.tiles;
        w.flag(true);
        if t.cols_log2 < t.max_cols_log2 {
            w.flag(false);
        }
        if t.rows_log2 < t.max_rows_log2 {
            w.flag(false);
        }
        if t.cols_log2 > 0 || t.rows_log2 > 0 {
            w.u(t.cols_log2 + t.rows_log2, 0);
            w.u(2, u64::from(self.tile_size_bytes_minus1));
        }
    }

    fn sequence(&self, stream: &Stream) -> VAEncSequenceParameterBufferAV1 {
        // SAFETY: plain data; zero is valid for every field.
        let mut s: VAEncSequenceParameterBufferAV1 = unsafe { std::mem::zeroed() };
        s.seq_level_idx = self.level_idx;
        s.intra_period = 0x7fff;
        s.ip_period = 1;
        s.bits_per_second = stream.bits_per_second();
        unsafe {
            let f = &mut s.seq_fields.bits;
            f.set_enable_order_hint(1);
            f.set_enable_cdef(1);
            f.set_subsampling_x(1);
            f.set_subsampling_y(1);
        }
        s.order_hint_bits_minus_1 = (ORDER_HINT_BITS - 1) as u8;
        s
    }
}

impl Codec for Av1 {
    fn packed_headers(&self) -> u32 {
        VA_ENC_PACKED_HEADER_PICTURE
    }

    fn picture<'d>(
        &mut self,
        display: &'d Display,
        context: VAContextID,
        stream: &Stream,
        pic: &Pic,
        out: &mut Vec<Buffer<'d>>,
    ) -> Result<()> {
        let key = pic.idr;
        let order_hint = (pic.since_idr & 0xff) as u8;
        if key {
            out.push(display.buffer(
                context,
                VAEncSequenceParameterBufferType,
                &self.sequence(stream),
            )?);
        }
        let header = self.frame_header(key, order_hint);
        let cqp = self.qp.is_some();

        // SAFETY: plain data; zero is valid for every field.
        let mut p: VAEncPictureParameterBufferAV1 = unsafe { std::mem::zeroed() };
        p.frame_width_minus_1 = (self.width - 1) as u16;
        p.frame_height_minus_1 = (self.height - 1) as u16;
        p.reconstructed_frame = pic.recon;
        p.coded_buf = pic.coded;
        p.reference_frames = [VA_INVALID_SURFACE; 8];
        if let Some(r) = pic.reference.filter(|_| !key) {
            p.reference_frames[0] = r;
            unsafe { p.ref_frame_ctrl_l0.fields.set_search_idx0(1) };
        }
        p.primary_ref_frame = PRIMARY_REF_NONE;
        p.order_hint = order_hint;
        p.refresh_frame_flags = if key { 0xff } else { 0x01 };
        unsafe {
            let f = &mut p.picture_flags.bits;
            f.set_frame_type(if key { 0 } else { 1 });
            f.set_error_resilient_mode(u32::from(key));
            f.set_disable_frame_end_update_cdf(1);
        }
        p.filter_level = [LOOP_FILTER[0], LOOP_FILTER[1]];
        p.filter_level_u = LOOP_FILTER[2];
        p.filter_level_v = LOOP_FILTER[3];
        p.interpolation_filter = self.interpolation_filter as u8;
        p.base_qindex = self.base_qindex();
        p.min_base_qindex = 1;
        p.max_base_qindex = 255;
        unsafe { p.mode_control_flags.bits.set_tx_mode(self.tx_mode) };
        p.tile_cols = self.tiles.widths.len() as u8;
        p.tile_rows = self.tiles.heights.len() as u8;
        for (i, &w) in self.tiles.widths.iter().enumerate().take(63) {
            p.width_in_sbs_minus_1[i] = (w - 1) as u16;
        }
        for (i, &h) in self.tiles.heights.iter().enumerate().take(63) {
            p.height_in_sbs_minus_1[i] = (h - 1) as u16;
        }
        if !cqp {
            p.bit_offset_qindex = header.qindex;
            p.bit_offset_loopfilter_params = header.loop_filter;
            p.bit_offset_cdef_params = header.cdef;
            p.size_in_bits_cdef_params = header.cdef_bits;
            p.size_in_bits_frame_hdr_obu = header.frame_header_bits;
        }
        p.byte_offset_frame_hdr_obu_size = header.obu_size_byte;
        unsafe { p.tile_group_obu_hdr_info.bits.set_obu_has_size_field(1) };
        out.push(display.buffer(context, VAEncPictureParameterBufferType, &p)?);

        packed(
            display,
            context,
            VAEncPackedHeaderPicture,
            &header.data,
            header.bits,
            out,
        )?;
        let tg = VAEncTileGroupBufferAV1 {
            tg_start: 0,
            tg_end: (self.tiles.count() - 1) as u8,
            ..Default::default()
        };
        out.push(display.buffer(context, VAEncSliceParameterBufferType, &tg)?);
        Ok(())
    }
}

fn obu_header(kind: u8) -> u8 {
    (kind << 3) | 0x02
}

/// Bits needed to write `v`, at least 1.
fn bits_for(v: u32) -> u32 {
    (32 - v.leading_zeros()).max(1)
}

/// `leb128()` padded to exactly `bytes` bytes, so the driver can rewrite it in place.
fn padded_leb128(v: u64, bytes: usize, out: &mut Vec<u8>) {
    for i in 0..bytes {
        let byte = ((v >> (7 * i)) & 0x7f) as u8;
        out.push(if i + 1 < bytes { byte | 0x80 } else { byte });
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::h264::tests::stream;
    use super::*;

    pub(crate) fn av1(width: u32, height: u32) -> Av1 {
        Av1::with(&stream(width, height), 0, TX_MODE_SELECT, 4, 3)
    }

    #[test]
    fn tiles_follow_the_spec_limits() {
        let t = Tiles::new(1920, 1080);
        assert_eq!(
            (
                t.cols_log2,
                t.rows_log2,
                t.widths.clone(),
                t.heights.clone()
            ),
            (0, 0, vec![30], vec![17])
        );
        assert_eq!((t.max_cols_log2, t.max_rows_log2), (5, 5));
        let t = Tiles::new(3840, 2160);
        assert_eq!(t.count(), 1);
        let t = Tiles::new(7680, 4320);
        assert_eq!((t.cols_log2, t.rows_log2), (1, 1));
        assert_eq!(
            (t.widths.clone(), t.heights.clone()),
            (vec![60, 60], vec![34, 34])
        );
    }

    #[test]
    fn padded_size_field() {
        let mut v = Vec::new();
        padded_leb128(300, 4, &mut v);
        assert_eq!(v, [0xac, 0x82, 0x80, 0x00]);
    }

    #[test]
    fn header_offsets_point_into_the_data() {
        let a = av1(1920, 1080);
        let seq = a.sequence_header();
        assert_eq!(seq[0], 0x0a);
        let key = a.frame_header(true, 0);
        assert_eq!(&key.data[..seq.len()], &seq[..]);
        assert_eq!(key.data[seq.len()], 0x1a);
        assert_eq!(key.obu_size_byte as usize, seq.len() + 1);
        let size = key.data[key.obu_size_byte as usize..key.obu_size_byte as usize + 4]
            .iter()
            .enumerate()
            .fold(0usize, |acc, (i, b)| {
                acc | (usize::from(b & 0x7f) << (7 * i))
            });
        assert_eq!(size, key.data.len() - seq.len() - 5);
        let bit = |h: &Header, at: u32, n: u32| {
            (0..n).fold(0u32, |acc, i| {
                let b = at + i;
                acc << 1 | u32::from(h.data[(b / 8) as usize] >> (7 - b % 8) & 1)
            })
        };
        assert_eq!(bit(&key, key.qindex, 8), 128);
        assert_eq!(bit(&key, key.loop_filter, 6), 8);
        assert_eq!(key.cdef_bits, 16);
        let inter = a.frame_header(false, 5);
        assert_eq!(inter.obu_size_byte, 1);
        assert_eq!(bit(&inter, inter.qindex, 8), 128);
        assert_eq!(bit(&inter, 8 * 5 + 7, 8), 5);
        assert_eq!(bit(&key, (seq.len() as u32 + 5) * 8 + 6, 8), 0);
    }
}
