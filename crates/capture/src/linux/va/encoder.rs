//! VA-API encoder: VPP converts each scanout into a pool surface, the encoder reads it in place,
//! and an output thread copies each bitstream out in order. Low-latency CBR (or a fixed quantizer
//! for the Source quality), no B-frames, an infinite GOP, keyframes on demand, parameter sets on
//! every IDR.

use std::path::Path;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use ws_proto::{VideoCodec, VideoConfig};

use super::av1::Av1;
use super::h264::H264;
use super::hevc::{Hevc, Tools};
use super::sys::*;
use super::vpp::Vpp;
use super::{Buffer, Config, Display, VAContextID, encode_entry};
use crate::encode::{EncodeTiming, Encoded, Finished};
use crate::linux::kms::Frame;
use crate::{CaptureError, Result};

/// Frames in the encoder at once.
const SLOTS: usize = 2;
/// Pool surfaces: one per slot, the newest image and one being converted.
const POOL: usize = SLOTS + 2;
/// A frame not out of the encoder in this long means the GPU hung.
const FRAME_TIMEOUT: Duration = Duration::from_secs(2);

/// The stream's encode settings.
#[derive(Debug, Clone)]
pub(crate) struct Stream {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub kbps: u32,
    pub vbv_frames: f64,
    /// Fixed quantizer instead of CBR.
    pub qp: Option<u8>,
    /// `VAEncMiscParameterBufferQualityLevel`, when the driver has levels.
    pub quality_level: Option<u32>,
}

impl Stream {
    pub(crate) fn bits_per_second(&self) -> u32 {
        self.kbps.saturating_mul(1000)
    }

    /// The HRD buffer: `vbv_frames` frames at the bitrate.
    pub(crate) fn hrd_bits(&self) -> u32 {
        (f64::from(self.bits_per_second()) / f64::from(self.fps.max(1)) * self.vbv_frames) as u32
    }
}

/// One frame to encode.
pub(crate) struct Pic {
    pub idr: bool,
    /// Frames since the last IDR, 0 for the IDR.
    pub since_idr: u64,
    pub recon: VASurfaceID,
    pub reference: Option<VASurfaceID>,
    pub coded: VABufferID,
}

/// The codec-specific parameter buffers and headers for each frame.
pub(crate) trait Codec: Send {
    /// The `VA_ENC_PACKED_HEADER_*` kinds this codec sends.
    fn packed_headers(&self) -> u32;
    /// Appends the buffers for `pic` to `out`.
    fn picture<'d>(
        &mut self,
        display: &'d Display,
        context: VAContextID,
        stream: &Stream,
        pic: &Pic,
        out: &mut Vec<Buffer<'d>>,
    ) -> Result<()>;
}

/// Appends a packed header of `kind`: parameter buffer then data buffer.
pub(crate) fn packed<'d>(
    display: &'d Display,
    context: VAContextID,
    kind: u32,
    data: &[u8],
    bits: usize,
    out: &mut Vec<Buffer<'d>>,
) -> Result<()> {
    let param = VAEncPackedHeaderParameterBuffer {
        type_: kind,
        bit_length: bits as u32,
        has_emulation_bytes: 1,
        ..Default::default()
    };
    out.push(display.buffer(context, VAEncPackedHeaderParameterBufferType, &param)?);
    out.push(display.raw_buffer(
        context,
        VAEncPackedHeaderDataBufferType,
        data.as_ptr().cast_mut().cast(),
        data.len(),
    )?);
    Ok(())
}

/// A frame in the encoder, for the output thread.
struct Pending<M> {
    slot: usize,
    surface_index: usize,
    surface: VASurfaceID,
    coded: VABufferID,
    keyframe: bool,
    before_slot: Instant,
    slot_taken: Instant,
    submitted: Instant,
    meta: M,
}

/// A slot and pool surface the output thread is done with, or `None` once a frame failed.
type Returned = Option<(usize, usize)>;

/// Converter and encoder on one VA display.
pub(crate) struct VaEncoder<M: Send + 'static> {
    display: Arc<Display>,
    vpp: Vpp,
    config: Config,
    context: VAContextID,
    codec: Box<dyn Codec>,
    stream: Stream,
    cqp: bool,
    recon: [VASurfaceID; 2],
    /// The recon surface the next frame writes.
    recon_next: usize,
    /// The other recon surface holds a frame the next one may reference.
    has_reference: bool,
    since_idr: u64,
    rate_changed: bool,
    coded: Vec<VABufferID>,
    free: Vec<usize>,
    returned: std_mpsc::Receiver<Returned>,
    pending: Option<std_mpsc::Sender<Pending<M>>>,
    output: Option<JoinHandle<()>>,
    failed: Arc<Mutex<Option<String>>>,
}

impl<M: Send + 'static> VaEncoder<M> {
    /// Opens a converter and encoder on the GPU at `node` for `cfg`.
    pub(crate) fn new(
        node: &Path,
        cfg: &VideoConfig,
        on_output: impl FnMut(Finished<M>) + Send + 'static,
    ) -> Result<Self> {
        let display = Display::open(node)?;
        let (profile, entrypoint) = encode_entry(&display, cfg.codec).ok_or_else(|| {
            CaptureError::NoEncoder(format!(
                "{:?} encoding is not supported by the VA-API driver ({})",
                cfg.codec, display.vendor
            ))
        })?;
        let tuning = crate::tuning::tuning(cfg);
        let attr = |kind| display.attribute(profile, entrypoint, kind);
        let rc_modes = attr(VAConfigAttribRateControl).unwrap_or(0);
        let cqp = tuning.source && rc_modes & VA_RC_CQP != 0;
        let packed_supported = attr(VAConfigAttribEncPackedHeaders).unwrap_or(0);
        let quality_level = attr(VAConfigAttribEncQualityRange)
            .filter(|&range| range > 1)
            .map(|range| quality_level(tuning.preset, range));
        let stream = Stream {
            width: cfg.width,
            height: cfg.height,
            fps: cfg.fps,
            kbps: cfg.bitrate_kbps,
            vbv_frames: tuning.vbv_frames,
            qp: cqp.then(|| crate::nvenc::source_qp(cfg.codec) as u8),
            quality_level,
        };
        let (codec, align): (Box<dyn Codec>, u32) = match cfg.codec {
            VideoCodec::H264 => (Box::new(H264::new(profile, &stream, packed_supported)), 16),
            VideoCodec::H265 => {
                let tools = Tools::new(
                    attr(VAConfigAttribEncHEVCFeatures),
                    attr(VAConfigAttribEncHEVCBlockSizes),
                    attr(VAConfigAttribPredictionDirection),
                    cqp,
                );
                let align = (1u32 << tools.log2_min_cb).max(16);
                (Box::new(Hevc::new(&stream, tools, packed_supported)), align)
            }
            VideoCodec::Av1 => (
                Box::new(Av1::new(
                    &display,
                    profile,
                    entrypoint,
                    &stream,
                    packed_supported,
                )?),
                64,
            ),
        };
        let wanted = codec.packed_headers();
        let mut attribs = vec![
            VAConfigAttrib {
                type_: VAConfigAttribRTFormat,
                value: VA_RT_FORMAT_YUV420,
            },
            VAConfigAttrib {
                type_: VAConfigAttribRateControl,
                value: if cqp { VA_RC_CQP } else { VA_RC_CBR },
            },
        ];
        if wanted != 0 {
            attribs.push(VAConfigAttrib {
                type_: VAConfigAttribEncPackedHeaders,
                value: wanted,
            });
        }
        let surface_w = cfg.width.next_multiple_of(align);
        let surface_h = cfg.height.next_multiple_of(align);
        let vpp = Vpp::new(
            display.clone(),
            cfg.width,
            cfg.height,
            surface_w,
            surface_h,
            POOL,
        )?;
        let config = display.create_config(profile, entrypoint, &mut attribs)?;
        let mut recon = match display.create_surfaces(
            VA_RT_FORMAT_YUV420,
            VA_FOURCC_NV12,
            surface_w,
            surface_h,
            2,
        ) {
            Ok(r) => r,
            Err(e) => {
                display.destroy_config(&config);
                return Err(e);
            }
        };
        let context = match display.create_context(&config, surface_w, surface_h, &mut recon) {
            Ok(c) => c,
            Err(e) => {
                display.destroy_surfaces(&mut recon);
                display.destroy_config(&config);
                return Err(e);
            }
        };
        let (returned_tx, returned) = std_mpsc::channel();
        let mut this = Self {
            display: display.clone(),
            vpp,
            config,
            context,
            codec,
            stream,
            cqp,
            recon: [recon[0], recon[1]],
            recon_next: 0,
            has_reference: false,
            since_idr: 0,
            rate_changed: false,
            coded: Vec::new(),
            free: Vec::new(),
            returned,
            pending: None,
            output: None,
            failed: Arc::default(),
        };
        let coded_size = (surface_w as usize * surface_h as usize * 3 / 2).max(1 << 20);
        for _ in 0..SLOTS {
            let buffer = display.raw_buffer(
                context,
                VAEncCodedBufferType,
                std::ptr::null_mut(),
                coded_size,
            )?;
            let id = buffer.id;
            std::mem::forget(buffer);
            this.coded.push(id);
        }
        this.free = (0..SLOTS).rev().collect();
        let (pending_tx, pending) = std_mpsc::channel();
        let failed = this.failed.clone();
        this.output = Some(
            std::thread::Builder::new()
                .name("ws-encode-out".into())
                .spawn(move || output_loop(display, pending, returned_tx, failed, on_output))
                .map_err(|e| CaptureError::Platform(format!("spawn encoder output thread: {e}")))?,
        );
        this.pending = Some(pending_tx);
        tracing::info!(
            codec = ?cfg.codec,
            cfg.width,
            cfg.height,
            cfg.fps,
            cfg.bitrate_kbps,
            quality = ?cfg.quality,
            vendor = %this.display.vendor,
            low_power = entrypoint == VAEntrypointEncSliceLP,
            packed_headers = wanted & packed_supported,
            cqp,
            "VA-API encoder ready"
        );
        Ok(this)
    }

    /// Converts a scanout into the next image to encode.
    pub(crate) fn convert(&mut self, frame: &Frame) -> Result<()> {
        self.collect()?;
        let target = match self.vpp.free_surface() {
            Some(t) => t,
            None => {
                self.wait_returned()?;
                self.vpp
                    .free_surface()
                    .ok_or_else(|| CaptureError::Platform("no free surface".into()))?
            }
        };
        self.vpp.convert(frame, target)
    }

    /// Whether there is an image to encode.
    pub(crate) fn has_image(&self) -> bool {
        self.vpp.current.is_some()
    }

    /// Queues the newest image for encoding; it comes out on the output thread with `meta`.
    pub(crate) fn submit(&mut self, force_idr: bool, kbps: Option<u32>, meta: M) -> Result<()> {
        if let Some(e) = self.failed.lock().unwrap_or_else(|e| e.into_inner()).take() {
            return Err(CaptureError::Platform(e));
        }
        let surface_index = self
            .vpp
            .current
            .ok_or_else(|| CaptureError::Platform("no image to encode".into()))?;
        let before_slot = Instant::now();
        self.collect()?;
        let slot = match self.free.pop() {
            Some(s) => s,
            None => {
                while self.free.is_empty() {
                    self.wait_returned()?;
                }
                self.free.pop().unwrap_or_default()
            }
        };
        let slot_taken = Instant::now();
        if let Some(kbps) = kbps.filter(|&k| k != self.stream.kbps) {
            self.stream.kbps = kbps;
            self.rate_changed = true;
        }
        let idr = force_idr || !self.has_reference;
        if idr {
            self.since_idr = 0;
        }
        let pic = Pic {
            idr,
            since_idr: self.since_idr,
            recon: self.recon[self.recon_next],
            reference: self.has_reference.then(|| self.recon[1 - self.recon_next]),
            coded: self.coded[slot],
        };
        let surface = self.vpp.pool[surface_index].surface;
        if let Err(e) = self.render(&pic, surface) {
            self.free.push(slot);
            self.has_reference = false;
            return Err(e);
        }
        self.vpp.pool[surface_index].users += 1;
        self.recon_next = 1 - self.recon_next;
        self.has_reference = true;
        self.since_idr += 1;
        let pending = Pending {
            slot,
            surface_index,
            surface,
            coded: self.coded[slot],
            keyframe: idr,
            before_slot,
            slot_taken,
            submitted: Instant::now(),
            meta,
        };
        match self.pending.as_ref().map(|p| p.send(pending)) {
            Some(Ok(())) => Ok(()),
            _ => Err(CaptureError::Platform(
                "the encoder output thread stopped".into(),
            )),
        }
    }

    fn render(&mut self, pic: &Pic, surface: VASurfaceID) -> Result<()> {
        let d = &*self.display;
        let mut buffers: Vec<Buffer<'_>> = Vec::with_capacity(12);
        if pic.idr || self.rate_changed {
            self.rate_buffers(d, &mut buffers)?;
            self.rate_changed = false;
        }
        if pic.idr {
            if let Some(level) = self.stream.quality_level {
                let q = VAEncMiscParameterBufferQualityLevel {
                    quality_level: level,
                    ..Default::default()
                };
                buffers.push(d.misc_buffer(
                    self.context,
                    VAEncMiscParameterTypeQualityLevel,
                    &q,
                )?);
            }
        }
        self.codec
            .picture(d, self.context, &self.stream, pic, &mut buffers)?;
        d.render(self.context, surface, &buffers)
    }

    /// Rate control, HRD and frame rate parameters.
    fn rate_buffers<'d>(&self, d: &'d Display, out: &mut Vec<Buffer<'d>>) -> Result<()> {
        let s = &self.stream;
        let fr = VAEncMiscParameterFrameRate {
            framerate: (1 << 16) | s.fps,
            // SAFETY: plain bits.
            ..unsafe { std::mem::zeroed() }
        };
        out.push(d.misc_buffer(self.context, VAEncMiscParameterTypeFrameRate, &fr)?);
        if self.cqp {
            return Ok(());
        }
        let hrd_bits = s.hrd_bits();
        // SAFETY: plain data; zero is valid for every field.
        let mut rc: VAEncMiscParameterRateControl = unsafe { std::mem::zeroed() };
        rc.bits_per_second = s.bits_per_second();
        rc.target_percentage = 100;
        rc.window_size =
            ((u64::from(hrd_bits) * 1000) / u64::from(rc.bits_per_second.max(1))).max(1) as u32;
        unsafe {
            let f = &mut rc.rc_flags.bits;
            f.set_disable_frame_skip(1);
            f.set_disable_bit_stuffing(1);
        }
        out.push(d.misc_buffer(self.context, VAEncMiscParameterTypeRateControl, &rc)?);
        let hrd = VAEncMiscParameterHRD {
            initial_buffer_fullness: hrd_bits,
            buffer_size: hrd_bits,
            ..Default::default()
        };
        out.push(d.misc_buffer(self.context, VAEncMiscParameterTypeHRD, &hrd)?);
        Ok(())
    }

    /// Takes back whatever the output thread has finished with, without waiting.
    fn collect(&mut self) -> Result<()> {
        loop {
            match self.returned.try_recv() {
                Ok(r) => self.give_back(r)?,
                Err(_) => return Ok(()),
            }
        }
    }

    fn wait_returned(&mut self) -> Result<()> {
        match self.returned.recv_timeout(FRAME_TIMEOUT * 2) {
            Ok(r) => self.give_back(r),
            Err(_) => Err(CaptureError::Platform(
                "the encoder stopped returning frames".into(),
            )),
        }
    }

    fn give_back(&mut self, r: Returned) -> Result<()> {
        let Some((slot, surface)) = r else {
            let e = self.failed.lock().unwrap_or_else(|e| e.into_inner()).take();
            return Err(CaptureError::Platform(
                e.unwrap_or_else(|| "a frame failed in the encoder".into()),
            ));
        };
        self.free.push(slot);
        let users = &mut self.vpp.pool[surface].users;
        *users = users.saturating_sub(1);
        Ok(())
    }

    /// Waits until every frame in the encoder has come out.
    pub(crate) fn drain(&mut self) -> Result<()> {
        while self.free.len() < SLOTS {
            self.wait_returned()?;
        }
        Ok(())
    }

    /// The bitrate the encoder is running at.
    pub(crate) fn bitrate_kbps(&self) -> u32 {
        self.stream.kbps
    }
}

impl<M: Send + 'static> Drop for VaEncoder<M> {
    fn drop(&mut self) {
        self.pending.take();
        if let Some(t) = self.output.take() {
            let _ = t.join();
        }
        let d = &self.display;
        d.destroy_context(self.context);
        for &id in &self.coded {
            unsafe { (d.lib().vaDestroyBuffer)(d.raw(), id) };
        }
        d.destroy_surfaces(&mut self.recon);
        d.destroy_config(&self.config);
    }
}

/// Maps NVENC's preset (1 fastest ..= 7 slowest) onto the driver's quality levels (1 best ..=
/// `range`).
pub(crate) fn quality_level(preset: u8, range: u32) -> u32 {
    let preset = u32::from(preset.clamp(1, 7));
    let level = range - ((preset - 1) * (range - 1) + 3) / 6;
    level.clamp(1, range)
}

/// The output thread: waits for each frame, in order, copies its bitstream out, gives its slot back
/// and hands it on.
fn output_loop<M: Send>(
    display: Arc<Display>,
    pending: std_mpsc::Receiver<Pending<M>>,
    returned: std_mpsc::Sender<Returned>,
    failed: Arc<Mutex<Option<String>>>,
    mut on_output: impl FnMut(Finished<M>),
) {
    let mut broken = false;
    while let Ok(p) = pending.recv() {
        let wait_at = Instant::now();
        let result = if broken {
            Err(CaptureError::Platform("an earlier frame failed".into()))
        } else {
            finish(&display, &p)
        };
        let done = Instant::now();
        match result {
            Ok((data, completed)) => {
                let _ = returned.send(Some((p.slot, p.surface_index)));
                on_output(Ok(Encoded {
                    data,
                    keyframe: p.keyframe,
                    meta: p.meta,
                    encode_time: done.saturating_duration_since(p.submitted),
                    timing: EncodeTiming {
                        before_slot: p.before_slot,
                        slot_taken: p.slot_taken,
                        submitted: p.submitted,
                        wait_at,
                        completed,
                        done,
                        lock_retries: 0,
                    },
                }));
            }
            Err(e) => {
                *failed.lock().unwrap_or_else(|e| e.into_inner()) = Some(e.to_string());
                if !broken {
                    let _ = returned.send(None);
                }
                broken = true;
                on_output(Err((p.meta, e)));
            }
        }
    }
}

/// Waits for one frame and copies its bitstream out.
fn finish<M>(display: &Display, p: &Pending<M>) -> Result<(Bytes, Instant)> {
    let lib = display.lib();
    let status = match lib.vaSyncBuffer {
        Some(sync) => {
            let s = unsafe { sync(display.raw(), p.coded, FRAME_TIMEOUT.as_nanos() as u64) };
            if s == VA_STATUS_ERROR_UNIMPLEMENTED as VAStatus {
                unsafe { (lib.vaSyncSurface)(display.raw(), p.surface) }
            } else {
                s
            }
        }
        None => unsafe { (lib.vaSyncSurface)(display.raw(), p.surface) },
    };
    if status == VA_STATUS_ERROR_TIMEDOUT as VAStatus {
        return Err(CaptureError::Platform(format!(
            "the encoder did not finish a frame in {} s",
            FRAME_TIMEOUT.as_secs()
        )));
    }
    display.check(status, "vaSyncBuffer")?;
    let completed = Instant::now();
    let mut mapped: *mut std::ffi::c_void = std::ptr::null_mut();
    display.check(
        unsafe { (lib.vaMapBuffer)(display.raw(), p.coded, &mut mapped) },
        "vaMapBuffer (coded)",
    )?;
    let mut out = BytesMut::new();
    let mut overflow = false;
    let mut segment = mapped.cast::<VACodedBufferSegment>();
    // SAFETY: the driver hands back a list of segments valid until vaUnmapBuffer.
    unsafe {
        while !segment.is_null() {
            let s = &*segment;
            if !s.buf.is_null() && s.size > 0 {
                out.extend_from_slice(std::slice::from_raw_parts(
                    s.buf.cast::<u8>(),
                    s.size as usize,
                ));
            }
            overflow |= s.status & VA_CODED_BUF_STATUS_SLICE_OVERFLOW_MASK != 0;
            segment = s.next.cast();
        }
    }
    display.check(
        unsafe { (lib.vaUnmapBuffer)(display.raw(), p.coded) },
        "vaUnmapBuffer (coded)",
    )?;
    if overflow {
        return Err(CaptureError::Platform(
            "the coded frame did not fit its buffer".into(),
        ));
    }
    Ok((out.freeze(), completed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_map_onto_quality_levels() {
        assert_eq!(quality_level(1, 7), 7);
        assert_eq!(quality_level(4, 7), 4);
        assert_eq!(quality_level(7, 7), 1);
        assert_eq!(quality_level(1, 2), 2);
        assert_eq!(quality_level(7, 2), 1);
        for p in 1..=7 {
            let l = quality_level(p, 3);
            assert!((1..=3).contains(&l));
        }
    }

    #[test]
    fn hrd_is_vbv_frames_at_the_bitrate() {
        let s = Stream {
            width: 1920,
            height: 1080,
            fps: 60,
            kbps: 6000,
            vbv_frames: 1.5,
            qp: None,
            quality_level: None,
        };
        assert_eq!(s.hrd_bits(), 150_000);
    }
}
