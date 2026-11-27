//! Media Foundation hardware encoder on the pipeline's D3D11 device: an asynchronous hardware MFT
//! fed NV12 textures in place, in low-latency CBR with a VBV of about one frame, no B-frames, an
//! infinite GOP, keyframes on demand and parameter sets on every keyframe.

use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bytes::Bytes;
use windows::Win32::Foundation::{LUID, S_OK};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
};
use windows::Win32::System::Variant::VARIANT;
use windows::core::{BOOL, GUID, HRESULT, Interface, s, w};
use ws_proto::{VideoCodec, VideoConfig};

use super::Gpu;
use super::encode::{
    EncodeTiming, Encoded, FAILED_SLOT, FRAME_TIMEOUT, Finished, SLOTS, nv12_texture, source_qp,
};
use crate::bitstream;
use crate::caps::CodecSupport;
use crate::{CaptureError, Result};

type CreateObjectFn = unsafe extern "system" fn(*mut *mut c_void) -> HRESULT;
type CreateSizedFn = unsafe extern "system" fn(u32, *mut *mut c_void) -> HRESULT;
type CreateAttributesFn = unsafe extern "system" fn(*mut *mut c_void, u32) -> HRESULT;
type CreateDeviceManagerFn = unsafe extern "system" fn(*mut u32, *mut *mut c_void) -> HRESULT;
type CreateSurfaceBufferFn =
    unsafe extern "system" fn(*const GUID, *mut c_void, u32, BOOL, *mut *mut c_void) -> HRESULT;
type EnumFn = unsafe extern "system" fn(
    GUID,
    u32,
    *const MFT_REGISTER_TYPE_INFO,
    *const MFT_REGISTER_TYPE_INFO,
    *mut c_void,
    *mut *mut *mut c_void,
    *mut u32,
) -> HRESULT;
type StartupFn = unsafe extern "system" fn(u32, u32) -> HRESULT;

/// The mfplat.dll entry points, loaded at run time so a PC without Media Foundation still starts.
pub(super) struct Api {
    create_attributes: CreateAttributesFn,
    create_media_type: CreateObjectFn,
    create_sample: CreateObjectFn,
    create_memory_buffer: CreateSizedFn,
    create_device_manager: CreateDeviceManagerFn,
    create_surface_buffer: CreateSurfaceBufferFn,
    enum2: EnumFn,
}

/// Media Foundation, started once per process and never shut down.
pub(super) fn api() -> Result<&'static Api> {
    static API: OnceLock<std::result::Result<Api, String>> = OnceLock::new();
    API.get_or_init(load_api)
        .as_ref()
        .map_err(|e| CaptureError::NoEncoder(e.clone()))
}

fn load_api() -> std::result::Result<Api, String> {
    let lib = unsafe { LoadLibraryExW(w!("mfplat.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) }
        .map_err(|e| format!("Media Foundation is not installed (mfplat.dll): {e}"))?;
    // The module stays loaded for the life of the process.
    macro_rules! entry {
        ($name:literal as $ty:ty) => {{
            let f = unsafe { GetProcAddress(lib, s!($name)) }.ok_or(concat!($name, " missing"))?;
            // SAFETY: the signature matches mfapi.h.
            unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, $ty>(f) }
        }};
    }
    let startup = entry!("MFStartup" as StartupFn);
    let api = Api {
        create_attributes: entry!("MFCreateAttributes" as CreateAttributesFn),
        create_media_type: entry!("MFCreateMediaType" as CreateObjectFn),
        create_sample: entry!("MFCreateSample" as CreateObjectFn),
        create_memory_buffer: entry!("MFCreateMemoryBuffer" as CreateSizedFn),
        create_device_manager: entry!("MFCreateDXGIDeviceManager" as CreateDeviceManagerFn),
        create_surface_buffer: entry!("MFCreateDXGISurfaceBuffer" as CreateSurfaceBufferFn),
        enum2: entry!("MFTEnum2" as EnumFn),
    };
    unsafe { startup(MF_VERSION, MFSTARTUP_LITE) }
        .ok()
        .map_err(|e| format!("MFStartup: {e}"))?;
    Ok(api)
}

fn mf_err(what: &'static str) -> impl Fn(windows::core::Error) -> CaptureError {
    move |e| CaptureError::Platform(format!("{what}: {e}"))
}

/// Takes ownership of the interface an `out` parameter returned.
///
/// # Safety
/// `raw` must be null or an owned pointer to a `T`.
unsafe fn owned<T: Interface>(hr: HRESULT, raw: *mut c_void, what: &'static str) -> Result<T> {
    hr.ok().map_err(mf_err(what))?;
    if raw.is_null() {
        return Err(CaptureError::Platform(format!("{what} returned nothing")));
    }
    Ok(unsafe { T::from_raw(raw) })
}

impl Api {
    fn attributes(&self) -> Result<IMFAttributes> {
        let mut raw = ptr::null_mut();
        unsafe {
            owned(
                (self.create_attributes)(&mut raw, 1),
                raw,
                "MFCreateAttributes",
            )
        }
    }

    fn media_type(&self) -> Result<IMFMediaType> {
        let mut raw = ptr::null_mut();
        unsafe { owned((self.create_media_type)(&mut raw), raw, "MFCreateMediaType") }
    }

    pub(super) fn sample(&self) -> Result<IMFSample> {
        let mut raw = ptr::null_mut();
        unsafe { owned((self.create_sample)(&mut raw), raw, "MFCreateSample") }
    }

    pub(super) fn memory_buffer(&self, size: u32) -> Result<IMFMediaBuffer> {
        let mut raw = ptr::null_mut();
        unsafe {
            owned(
                (self.create_memory_buffer)(size, &mut raw),
                raw,
                "MFCreateMemoryBuffer",
            )
        }
    }

    fn device_manager(&self, device: &ID3D11Device) -> Result<IMFDXGIDeviceManager> {
        let mut token = 0;
        let mut raw = ptr::null_mut();
        let manager: IMFDXGIDeviceManager = unsafe {
            owned(
                (self.create_device_manager)(&mut token, &mut raw),
                raw,
                "MFCreateDXGIDeviceManager",
            )
        }?;
        unsafe { manager.ResetDevice(device, token) }.map_err(mf_err("ResetDevice"))?;
        Ok(manager)
    }

    /// A sample whose one buffer is `texture`, read in place by the encoder.
    fn texture_sample(&self, texture: &ID3D11Texture2D) -> Result<IMFSample> {
        let mut raw = ptr::null_mut();
        let buffer: IMFMediaBuffer = unsafe {
            owned(
                (self.create_surface_buffer)(
                    &ID3D11Texture2D::IID,
                    texture.as_raw(),
                    0,
                    false.into(),
                    &mut raw,
                ),
                raw,
                "MFCreateDXGISurfaceBuffer",
            )
        }?;
        unsafe {
            let length = buffer.GetMaxLength().map_err(mf_err("GetMaxLength"))?;
            buffer
                .SetCurrentLength(length)
                .map_err(mf_err("SetCurrentLength"))?;
        }
        let sample = self.sample()?;
        unsafe { sample.AddBuffer(&buffer) }.map_err(mf_err("AddBuffer"))?;
        Ok(sample)
    }

    /// The hardware encoders on the adapter `luid` that take NV12 and produce `codec`, best first.
    fn hardware_encoders(&self, luid: LUID, codec: VideoCodec) -> Result<Vec<IMFActivate>> {
        let attributes = self.attributes()?;
        let mut blob = [0u8; 8];
        blob[..4].copy_from_slice(&luid.LowPart.to_le_bytes());
        blob[4..].copy_from_slice(&luid.HighPart.to_le_bytes());
        unsafe { attributes.SetBlob(&MFT_ENUM_ADAPTER_LUID, &blob) }
            .map_err(mf_err("MFT_ENUM_ADAPTER_LUID"))?;
        let input = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_NV12,
        };
        let output = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: subtype(codec),
        };
        let mut list: *mut *mut c_void = ptr::null_mut();
        let mut count = 0u32;
        unsafe {
            (self.enum2)(
                MFT_CATEGORY_VIDEO_ENCODER,
                (MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0) as u32,
                &input,
                &output,
                attributes.as_raw(),
                &mut list,
                &mut count,
            )
        }
        .ok()
        .map_err(mf_err("MFTEnum2"))?;
        if list.is_null() {
            return Ok(Vec::new());
        }
        // SAFETY: MFTEnum2 returned `count` owned IMFActivate pointers in a CoTaskMemAlloc array.
        let found = (0..count as usize)
            .filter_map(|i| {
                let raw = unsafe { *list.add(i) };
                (!raw.is_null()).then(|| unsafe { IMFActivate::from_raw(raw) })
            })
            .collect();
        unsafe { CoTaskMemFree(Some(list as *const c_void)) };
        Ok(found)
    }
}

/// The largest frame Media Foundation encoders are assumed to take, as they do not report it.
fn assumed_max(codec: VideoCodec) -> (u32, u32) {
    match codec {
        VideoCodec::H264 => (4096, 4096),
        VideoCodec::H265 | VideoCodec::Av1 => (8192, 8192),
    }
}

fn subtype(codec: VideoCodec) -> GUID {
    match codec {
        VideoCodec::H264 => MFVideoFormat_H264,
        VideoCodec::H265 => MFVideoFormat_HEVC,
        VideoCodec::Av1 => MFVideoFormat_AV1,
    }
}

/// Joins the calling thread to the process's multithreaded COM apartment.
pub(super) fn init_com() {
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
}

/// The codecs the Media Foundation hardware encoders on the adapter `luid` produce.
pub(crate) fn probe(luid: LUID) -> Result<Vec<CodecSupport>> {
    let api = api()?;
    let mut out = Vec::new();
    for codec in [VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1] {
        match api.hardware_encoders(luid, codec) {
            Ok(found) if !found.is_empty() => {
                let (max_width, max_height) = assumed_max(codec);
                out.push(CodecSupport {
                    codec,
                    max_width,
                    max_height,
                    repair: false,
                });
            }
            Ok(_) => {}
            Err(e) => tracing::debug!(?codec, "Media Foundation encoder lookup failed: {e}"),
        }
    }
    Ok(out)
}

/// How the encoder is set up.
pub(super) struct Settings {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub kbps: u32,
    pub vbv_frames: f64,
    /// `CODECAPI_AVEncCommonQualityVsSpeed`, 0 (fastest) to 100.
    pub quality_vs_speed: u32,
    /// Constant quantizer instead of a constant bitrate.
    pub fixed_qp: Option<u32>,
}

impl Settings {
    fn from_config(cfg: &VideoConfig) -> Self {
        let effort = crate::tuning::tuning(cfg);
        Self {
            codec: cfg.codec,
            width: cfg.width,
            height: cfg.height,
            fps: cfg.fps.max(1),
            kbps: cfg.bitrate_kbps,
            vbv_frames: effort.vbv_frames,
            quality_vs_speed: u32::from(effort.preset.clamp(1, 7) - 1) * 100 / 6,
            fixed_qp: effort.source.then(|| source_qp(cfg.codec)),
        }
    }

    fn vbv_bits(&self, kbps: u32) -> u32 {
        (f64::from(kbps.saturating_mul(1000)) / f64::from(self.fps) * self.vbv_frames) as u32
    }
}

/// Sets one encoder property, logging a refusal.
pub(super) fn set_value(codec_api: &ICodecAPI, api: &GUID, value: VARIANT, what: &str) -> bool {
    match unsafe { codec_api.SetValue(api, &value) } {
        Ok(()) => true,
        Err(e) => {
            tracing::debug!("Media Foundation encoder refused {what}: {e}");
            false
        }
    }
}

fn set_pair(t: &IMFMediaType, key: &GUID, high: u32, low: u32) -> windows::core::Result<()> {
    unsafe { t.SetUINT64(key, (u64::from(high) << 32) | u64::from(low)) }
}

/// BT.709 limited range, matching what the video processor writes.
fn set_bt709(t: &IMFMediaType) {
    unsafe {
        let _ = t.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32);
        let _ = t.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32);
        let _ = t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32);
        let _ = t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32);
    }
}

fn set_video_format(t: &IMFMediaType, s: &Settings) -> windows::core::Result<()> {
    set_pair(t, &MF_MT_FRAME_SIZE, s.width, s.height)?;
    set_pair(t, &MF_MT_FRAME_RATE, s.fps, 1)?;
    set_pair(t, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
    unsafe { t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32) }?;
    set_bt709(t);
    Ok(())
}

/// Sets the rate control, latency and GOP properties, then the output and input types. Returns
/// whether the encoder runs at a fixed quantizer.
pub(super) fn configure(
    api: &Api,
    transform: &IMFTransform,
    codec_api: &ICodecAPI,
    (input, output): (u32, u32),
    s: &Settings,
) -> Result<bool> {
    let low_latency = set_value(
        codec_api,
        &CODECAPI_AVLowLatencyMode,
        true.into(),
        "low-latency mode",
    ) || set_value(
        codec_api,
        &CODECAPI_AVLowLatencyMode,
        1u32.into(),
        "low-latency mode",
    );
    if !low_latency {
        tracing::warn!("Media Foundation encoder has no low-latency mode");
    }
    let fixed = s.fixed_qp.is_some_and(|qp| {
        set_value(
            codec_api,
            &CODECAPI_AVEncCommonRateControlMode,
            (eAVEncCommonRateControlMode_Quality.0 as u32).into(),
            "quality rate control",
        ) && set_value(
            codec_api,
            &CODECAPI_AVEncVideoEncodeQP,
            u64::from(qp).into(),
            "a fixed quantizer",
        )
    });
    if !fixed {
        set_value(
            codec_api,
            &CODECAPI_AVEncCommonRateControlMode,
            (eAVEncCommonRateControlMode_CBR.0 as u32).into(),
            "CBR",
        );
        set_value(
            codec_api,
            &CODECAPI_AVEncCommonMeanBitRate,
            s.kbps.saturating_mul(1000).into(),
            "the bitrate",
        );
        set_value(
            codec_api,
            &CODECAPI_AVEncCommonBufferSize,
            s.vbv_bits(s.kbps).into(),
            "the VBV size",
        );
    }
    set_value(
        codec_api,
        &CODECAPI_AVEncMPVDefaultBPictureCount,
        0u32.into(),
        "no B-frames",
    );
    set_value(
        codec_api,
        &CODECAPI_AVEncMPVGOPSize,
        u32::MAX.into(),
        "an infinite GOP",
    );
    set_value(
        codec_api,
        &CODECAPI_AVEncCommonQualityVsSpeed,
        s.quality_vs_speed.into(),
        "the speed preset",
    );

    let out = api.media_type()?;
    let negotiate = || -> windows::core::Result<()> {
        unsafe {
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out.SetGUID(&MF_MT_SUBTYPE, &subtype(s.codec))?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, s.kbps.saturating_mul(1000))?;
            match s.codec {
                VideoCodec::H264 => {
                    out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?
                }
                VideoCodec::H265 => {
                    out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH265VProfile_Main_420_8.0 as u32)?
                }
                VideoCodec::Av1 => {}
            }
        }
        set_video_format(&out, s)?;
        unsafe { transform.SetOutputType(output, &out, 0) }
    };
    negotiate()
        .map_err(|e| CaptureError::NoEncoder(format!("{:?} output type refused: {e}", s.codec)))?;

    let offered = (0..64)
        .map_while(|i| unsafe { transform.GetInputAvailableType(input, i) }.ok())
        .find(|t| unsafe { t.GetGUID(&MF_MT_SUBTYPE) }.is_ok_and(|g| g == MFVideoFormat_NV12));
    let input_type = match offered {
        Some(t) => t,
        None => {
            let t = api.media_type()?;
            unsafe {
                t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                    .and_then(|()| t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12))
            }
            .map_err(mf_err("NV12 input type"))?;
            t
        }
    };
    set_video_format(&input_type, s)
        .and_then(|()| unsafe { transform.SetInputType(input, &input_type, 0) })
        .map_err(|e| CaptureError::NoEncoder(format!("NV12 input refused: {e}")))?;
    Ok(fixed)
}

/// The transform's first input and output stream IDs.
pub(super) fn stream_ids(transform: &IMFTransform) -> (u32, u32) {
    let (mut input, mut output) = ([0u32], [0u32]);
    match unsafe { transform.GetStreamIDs(&mut input, &mut output) } {
        Ok(()) => (input[0], output[0]),
        Err(_) => (0, 0),
    }
}

/// What one `ProcessOutput` call gave.
pub(super) enum Pulled {
    Sample(IMFSample),
    NeedInput,
    /// The output type changed and was accepted again; no sample.
    StreamChange,
}

/// How output samples are got.
#[derive(Clone, Copy)]
pub(super) struct OutputStream {
    pub id: u32,
    /// The encoder allocates the samples.
    pub provides: bool,
    /// The buffer size to allocate otherwise.
    pub size: u32,
}

impl OutputStream {
    pub(super) fn new(transform: &IMFTransform, id: u32, s: &Settings) -> Result<Self> {
        let info =
            unsafe { transform.GetOutputStreamInfo(id) }.map_err(mf_err("GetOutputStreamInfo"))?;
        let provides = info.dwFlags
            & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0)
                as u32
            != 0;
        let size = info.cbSize.max(s.width * s.height * 3 / 2);
        Ok(Self { id, provides, size })
    }

    /// Asks the transform for one output sample.
    pub(super) fn pull(&self, api: &Api, transform: &IMFTransform) -> Result<Pulled> {
        let sample = if self.provides {
            None
        } else {
            let sample = api.sample()?;
            let buffer = api.memory_buffer(self.size)?;
            unsafe { sample.AddBuffer(&buffer) }.map_err(mf_err("AddBuffer"))?;
            Some(sample)
        };
        let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: self.id,
            pSample: ManuallyDrop::new(sample),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0;
        let result = unsafe { transform.ProcessOutput(0, &mut buffers, &mut status) };
        let [mut buffer] = buffers;
        // SAFETY: each field is taken once and the buffer is not used afterwards.
        let sample = unsafe { ManuallyDrop::take(&mut buffer.pSample) };
        drop(unsafe { ManuallyDrop::take(&mut buffer.pEvents) });
        match result {
            Ok(()) => sample.map(Pulled::Sample).ok_or(CaptureError::Platform(
                "ProcessOutput gave no sample".into(),
            )),
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(Pulled::NeedInput),
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                let t = unsafe { transform.GetOutputAvailableType(self.id, 0) }
                    .map_err(mf_err("GetOutputAvailableType"))?;
                unsafe { transform.SetOutputType(self.id, &t, 0) }
                    .map_err(mf_err("SetOutputType after a stream change"))?;
                Ok(Pulled::StreamChange)
            }
            Err(e) => Err(mf_err("ProcessOutput")(e)),
        }
    }
}

/// The bitstream of an output sample and whether the encoder marked it a keyframe.
pub(super) fn sample_data(sample: &IMFSample) -> Result<(Vec<u8>, Option<bool>)> {
    let clean = unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }
        .ok()
        .map(|v| v != 0);
    let buffer = unsafe { sample.ConvertToContiguousBuffer() }
        .map_err(mf_err("ConvertToContiguousBuffer"))?;
    let mut data = ptr::null_mut();
    let mut length = 0;
    unsafe { buffer.Lock(&mut data, None, Some(&mut length)) }.map_err(mf_err("Lock"))?;
    // SAFETY: the buffer is locked and `data` holds `length` bytes until Unlock.
    let bytes = if data.is_null() {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(data, length as usize) }.to_vec()
    };
    unsafe { buffer.Unlock() }.map_err(mf_err("Unlock"))?;
    Ok((bytes, clean))
}

/// The stream's parameter sets (sequence header for AV1) as the output type carries them.
pub(super) fn sequence_header(transform: &IMFTransform, output: u32) -> Option<Vec<u8>> {
    let t = unsafe { transform.GetOutputCurrentType(output) }.ok()?;
    let size = unsafe { t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) }.ok()?;
    let mut blob = vec![0u8; size as usize];
    unsafe { t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut blob, None) }.ok()?;
    Some(blob)
}

/// Whether a frame is a keyframe, from its bitstream for H.264 and HEVC, else from the encoder's
/// mark or, without one, from whether a keyframe was asked for.
pub(super) fn is_keyframe(
    codec: VideoCodec,
    data: &[u8],
    marked: Option<bool>,
    asked: bool,
) -> bool {
    match codec {
        VideoCodec::H264 | VideoCodec::H265 => bitstream::is_idr(codec, data),
        VideoCodec::Av1 => marked.unwrap_or(asked),
    }
}

/// The encoder's request count and failure, shared with its output thread.
#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Default)]
struct State {
    /// `METransformNeedInput` events not yet answered with a frame.
    wanted: u32,
    failed: Option<String>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn want(&self, n: u32) {
        self.lock().wanted += n;
        self.changed.notify_all();
    }

    fn fail(&self, error: String) {
        self.lock().failed.get_or_insert(error);
        self.changed.notify_all();
    }

    fn failure(&self) -> Option<String> {
        self.lock().failed.clone()
    }

    /// Waits until the encoder asks for a frame, and takes that request.
    fn take_request(&self) -> Result<()> {
        let deadline = Instant::now() + FRAME_TIMEOUT * 2;
        let mut state = self.lock();
        loop {
            if let Some(e) = &state.failed {
                return Err(CaptureError::Platform(e.clone()));
            }
            if state.wanted > 0 {
                state.wanted -= 1;
                return Ok(());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(CaptureError::Platform(
                    "the encoder stopped asking for frames".into(),
                ));
            }
            state = self
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

/// A frame in the encoder, for the output thread.
struct Pending<M> {
    slot: usize,
    keyframe_asked: bool,
    before_slot: Instant,
    slot_taken: Instant,
    submitted: Instant,
    meta: M,
}

/// An input texture and the sample that wraps it.
struct Slot {
    /// NV12 at the stream size: a copy of the converter's output.
    texture: ID3D11Texture2D,
    sample: IMFSample,
}

/// COM objects handed to the output thread.
struct ForOutput {
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
}

// SAFETY: asynchronous MFTs are free-threaded.
unsafe impl Send for ForOutput {}

/// A Media Foundation hardware encoder that encodes in the background: [`Self::submit`] returns
/// once the frame is queued, and a thread of its own hands each finished frame to the callback
/// given to [`Self::new`], in order.
pub(crate) struct MfEncoder<M: Send + 'static> {
    activate: IMFActivate,
    transform: IMFTransform,
    codec_api: ICodecAPI,
    events: IMFMediaEventGenerator,
    _manager: IMFDXGIDeviceManager,
    input: u32,
    settings: Settings,
    fixed_qp: bool,
    kbps: u32,
    slots: Vec<Slot>,
    /// Slots ready for a frame.
    free: Vec<usize>,
    /// Slots the output thread is done with.
    returned: std_mpsc::Receiver<usize>,
    pending: Option<std_mpsc::Sender<Pending<M>>>,
    shared: Arc<Shared>,
    stopping: Arc<AtomicBool>,
    output: Option<JoinHandle<()>>,
}

impl<M: Send + 'static> MfEncoder<M> {
    /// Opens a hardware encoder for `cfg` on `gpu`'s adapter.
    pub(crate) fn new(
        gpu: &Gpu,
        cfg: &VideoConfig,
        on_output: impl FnMut(Finished<M>) + Send + 'static,
    ) -> Result<Self> {
        let api = api()?;
        init_com();
        let luid = adapter_luid(&gpu.device)?;
        let candidates = api.hardware_encoders(luid, cfg.codec)?;
        if candidates.is_empty() {
            return Err(CaptureError::NoEncoder(format!(
                "no Media Foundation hardware encoder for {:?} on this adapter",
                cfg.codec
            )));
        }
        let manager = api.device_manager(&gpu.device)?;
        let settings = Settings::from_config(cfg);
        let mut errors = Vec::new();
        let mut opened = None;
        for activate in candidates {
            let name = friendly_name(&activate);
            match open_transform(api, &activate, &manager, &settings) {
                Ok(t) => {
                    opened = Some((activate, t, name));
                    break;
                }
                Err(e) => {
                    tracing::debug!(encoder = %name, "Media Foundation encoder did not open: {e}");
                    let _ = unsafe { activate.ShutdownObject() };
                    errors.push(format!("{name}: {e}"));
                }
            }
        }
        let Some((activate, opened, name)) = opened else {
            return Err(CaptureError::NoEncoder(errors.join("; ")));
        };
        let Opened {
            transform,
            codec_api,
            events,
            ids: (input, output),
            fixed_qp,
        } = opened;
        let (returned_tx, returned) = std_mpsc::channel();
        let mut this = Self {
            activate,
            transform: transform.clone(),
            codec_api,
            events: events.clone(),
            _manager: manager,
            input,
            kbps: settings.kbps,
            fixed_qp,
            slots: Vec::new(),
            free: (0..SLOTS).rev().collect(),
            returned,
            pending: None,
            shared: Arc::default(),
            stopping: Arc::default(),
            output: None,
            settings,
        };
        for _ in 0..SLOTS {
            let texture = nv12_texture(&gpu.device, cfg.width, cfg.height)?;
            let sample = api.texture_sample(&texture)?;
            this.slots.push(Slot { texture, sample });
        }
        let stream = OutputStream::new(&transform, output, &this.settings)?;
        unsafe {
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .and_then(|()| transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0))
        }
        .map_err(mf_err("start streaming"))?;

        let (pending_tx, pending) = std_mpsc::channel();
        let thread = OutputThread {
            api,
            com: ForOutput { transform, events },
            stream,
            codec: cfg.codec,
            header: None,
            shared: this.shared.clone(),
            stopping: this.stopping.clone(),
        };
        this.output = Some(
            std::thread::Builder::new()
                .name("ws-encode-out".into())
                .spawn(move || {
                    let _ =
                        unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) };
                    super::enter_mmcss();
                    init_com();
                    thread.run(pending, returned_tx, on_output)
                })
                .map_err(|e| CaptureError::Platform(format!("spawn encoder output thread: {e}")))?,
        );
        this.pending = Some(pending_tx);
        tracing::info!(encoder = %name, codec = ?cfg.codec, cfg.width, cfg.height, cfg.fps, cfg.bitrate_kbps, quality = ?cfg.quality, fixed_qp, "Media Foundation encoder ready");
        Ok(this)
    }

    /// Changes the encoded size; not done in place, so the pipeline opens a new encoder.
    pub(crate) fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        Err(CaptureError::InvalidConfig(format!(
            "the Media Foundation encoder cannot change to {width}x{height} in place"
        )))
    }

    /// Waits until every frame in the encoder has come out.
    pub(crate) fn drain(&mut self) -> Result<()> {
        while self.free.len() < self.slots.len() {
            let slot = self.returned_slot()?;
            self.free.push(slot);
        }
        Ok(())
    }

    fn take_slot(&mut self) -> Result<usize> {
        match self.free.pop() {
            Some(slot) => Ok(slot),
            None => self.returned_slot(),
        }
    }

    fn returned_slot(&mut self) -> Result<usize> {
        match self.returned.recv_timeout(FRAME_TIMEOUT * 2) {
            Ok(FAILED_SLOT) => Err(CaptureError::Platform(
                self.shared
                    .failure()
                    .unwrap_or_else(|| "a frame failed in the encoder".into()),
            )),
            Ok(slot) => Ok(slot),
            Err(_) => Err(CaptureError::Platform(
                "the encoder stopped returning frames".into(),
            )),
        }
    }

    /// Queues `src` for encoding and returns; the frame comes out on the output thread with `meta`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn submit(
        &mut self,
        context: &ID3D11DeviceContext,
        src: &ID3D11Texture2D,
        timestamp: u64,
        force_idr: bool,
        _intra: bool,
        kbps: Option<u32>,
        meta: M,
    ) -> Result<()> {
        if let Some(e) = self.shared.failure() {
            return Err(CaptureError::Platform(e));
        }
        let before_slot = Instant::now();
        let index = self.take_slot()?;
        let slot_taken = Instant::now();
        match self.submit_to(index, context, src, timestamp, force_idr, kbps) {
            Ok(()) => {
                let pending = Pending {
                    slot: index,
                    keyframe_asked: force_idr,
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
            Err(e) => {
                self.free.push(index);
                Err(e)
            }
        }
    }

    fn submit_to(
        &mut self,
        index: usize,
        context: &ID3D11DeviceContext,
        src: &ID3D11Texture2D,
        timestamp: u64,
        force_idr: bool,
        kbps: Option<u32>,
    ) -> Result<()> {
        let slot = &self.slots[index];
        unsafe {
            context.CopyResource(&slot.texture, src);
            context.Flush();
        }
        if let Some(kbps) = kbps.filter(|k| *k != self.kbps) {
            if self.set_rate(kbps) {
                self.kbps = kbps;
            }
        }
        if force_idr {
            set_value(
                &self.codec_api,
                &CODECAPI_AVEncVideoForceKeyFrame,
                1u32.into(),
                "a keyframe",
            );
        }
        let duration = 10_000_000 / i64::from(self.settings.fps);
        let sample = &self.slots[index].sample;
        unsafe {
            sample
                .SetSampleTime(timestamp as i64 * duration)
                .and_then(|()| sample.SetSampleDuration(duration))
        }
        .map_err(mf_err("sample time"))?;
        self.shared.take_request()?;
        if let Err(e) = unsafe { self.transform.ProcessInput(self.input, sample, 0) } {
            self.shared.want(1);
            return Err(mf_err("ProcessInput")(e));
        }
        Ok(())
    }

    /// Changes the bitrate of a running encoder.
    fn set_rate(&self, kbps: u32) -> bool {
        if self.fixed_qp {
            return true;
        }
        let ok = set_value(
            &self.codec_api,
            &CODECAPI_AVEncCommonMeanBitRate,
            kbps.saturating_mul(1000).into(),
            "a bitrate change",
        );
        if ok {
            set_value(
                &self.codec_api,
                &CODECAPI_AVEncCommonBufferSize,
                self.settings.vbv_bits(kbps).into(),
                "a VBV size change",
            );
        }
        ok
    }

    /// Reference invalidation is not used with Media Foundation: a lost frame needs a keyframe.
    pub(crate) fn invalidate(&mut self, _timestamp: u64) -> Result<()> {
        Err(CaptureError::Platform(
            "reference invalidation not available".into(),
        ))
    }

    pub(crate) fn can_invalidate(&self) -> bool {
        false
    }

    /// The bitrate the encoder is running at.
    pub(crate) fn bitrate_kbps(&self) -> u32 {
        self.kbps
    }
}

impl<M: Send + 'static> Drop for MfEncoder<M> {
    fn drop(&mut self) {
        self.pending.take();
        self.stopping.store(true, Ordering::Release);
        if let Some(thread) = self.output.take() {
            let woke = unsafe {
                self.events
                    .QueueEvent(MEUnknown.0 as u32, &GUID::zeroed(), S_OK, ptr::null())
            }
            .is_ok();
            let mut stopped = woke && wait_finished(&thread, Duration::from_secs(1));
            if !stopped {
                if let Ok(shutdown) = self.transform.cast::<IMFShutdown>() {
                    let _ = unsafe { shutdown.Shutdown() };
                }
                stopped = wait_finished(&thread, Duration::from_secs(1));
            }
            if stopped {
                let _ = thread.join();
            } else {
                tracing::warn!("Media Foundation encoder output thread did not stop");
            }
        }
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, 0);
            if let Ok(shutdown) = self.transform.cast::<IMFShutdown>() {
                let _ = shutdown.Shutdown();
            }
            let _ = self.activate.ShutdownObject();
        }
    }
}

fn wait_finished(thread: &JoinHandle<()>, limit: Duration) -> bool {
    let started = Instant::now();
    while !thread.is_finished() {
        if started.elapsed() >= limit {
            return false;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    true
}

/// An activated, configured hardware transform.
struct Opened {
    transform: IMFTransform,
    codec_api: ICodecAPI,
    events: IMFMediaEventGenerator,
    ids: (u32, u32),
    fixed_qp: bool,
}

fn open_transform(
    api: &Api,
    activate: &IMFActivate,
    manager: &IMFDXGIDeviceManager,
    settings: &Settings,
) -> Result<Opened> {
    let transform: IMFTransform =
        unsafe { activate.ActivateObject() }.map_err(mf_err("ActivateObject"))?;
    let attributes = unsafe { transform.GetAttributes() }.map_err(mf_err("GetAttributes"))?;
    let flag = |key: &GUID| unsafe { attributes.GetUINT32(key) }.is_ok_and(|v| v != 0);
    if !flag(&MF_TRANSFORM_ASYNC) {
        return Err(CaptureError::NoEncoder("not an asynchronous MFT".into()));
    }
    if !flag(&MF_SA_D3D11_AWARE) {
        return Err(CaptureError::NoEncoder(
            "does not take D3D11 textures".into(),
        ));
    }
    unsafe {
        attributes
            .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
            .map_err(mf_err("MF_TRANSFORM_ASYNC_UNLOCK"))?;
        let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);
        transform
            .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
            .map_err(mf_err("MFT_MESSAGE_SET_D3D_MANAGER"))?;
    }
    let codec_api: ICodecAPI = transform.cast().map_err(mf_err("ICodecAPI"))?;
    let events: IMFMediaEventGenerator = transform.cast().map_err(mf_err("event generator"))?;
    let ids = stream_ids(&transform);
    let fixed_qp = configure(api, &transform, &codec_api, ids, settings)?;
    Ok(Opened {
        transform,
        codec_api,
        events,
        ids,
        fixed_qp,
    })
}

fn friendly_name(activate: &IMFActivate) -> String {
    let mut name = windows::core::PWSTR::null();
    let mut len = 0;
    if unsafe { activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut name, &mut len) }
        .is_err()
        || name.is_null()
    {
        return "Media Foundation encoder".into();
    }
    let text = unsafe { name.to_string() }.unwrap_or_default();
    unsafe { CoTaskMemFree(Some(name.0 as *const c_void)) };
    text
}

fn adapter_luid(device: &ID3D11Device) -> Result<LUID> {
    let dxgi: IDXGIDevice = device.cast().map_err(mf_err("IDXGIDevice"))?;
    let adapter = unsafe { dxgi.GetAdapter() }.map_err(mf_err("GetAdapter"))?;
    let desc = unsafe { adapter.GetDesc() }.map_err(mf_err("GetDesc"))?;
    Ok(desc.AdapterLuid)
}

/// The output thread's view of the encoder.
struct OutputThread {
    api: &'static Api,
    com: ForOutput,
    stream: OutputStream,
    codec: VideoCodec,
    /// The parameter sets last read from the output type.
    header: Option<Vec<u8>>,
    shared: Arc<Shared>,
    stopping: Arc<AtomicBool>,
}

impl OutputThread {
    /// Answers the encoder's events until it is dropped: counts its requests for frames, and takes
    /// each frame out, in order, gives its slot back and hands it on.
    fn run<M: Send>(
        mut self,
        pending: std_mpsc::Receiver<Pending<M>>,
        returned: std_mpsc::Sender<usize>,
        mut on_output: impl FnMut(Finished<M>),
    ) {
        let error = loop {
            let event = unsafe { self.com.events.GetEvent(MF_EVENT_FLAG_NONE) };
            if self.stopping.load(Ordering::Acquire) {
                return;
            }
            let event = match event {
                Ok(event) => event,
                Err(e) => break format!("encoder events: {e}"),
            };
            let status = unsafe { event.GetStatus() }.unwrap_or(S_OK);
            if status.is_err() {
                break format!(
                    "encoder error event: {}",
                    windows::core::Error::from(status)
                );
            }
            let kind = MF_EVENT_TYPE(unsafe { event.GetType() }.unwrap_or(0) as i32);
            if kind == METransformNeedInput {
                self.shared.want(1);
            } else if kind == METransformHaveOutput {
                let completed = Instant::now();
                let sample = match self.stream.pull(self.api, &self.com.transform) {
                    Ok(Pulled::Sample(sample)) => sample,
                    Ok(Pulled::NeedInput | Pulled::StreamChange) => continue,
                    Err(e) => break e.to_string(),
                };
                let Ok(p) = pending.recv_timeout(FRAME_TIMEOUT) else {
                    break "the encoder gave a frame that was not asked for".into();
                };
                match self.finish(&sample, p.keyframe_asked) {
                    Ok((data, keyframe)) => {
                        let done = Instant::now();
                        let _ = returned.send(p.slot);
                        on_output(Ok(Encoded {
                            data,
                            keyframe,
                            meta: p.meta,
                            encode_time: done.saturating_duration_since(p.submitted),
                            timing: EncodeTiming {
                                before_slot: p.before_slot,
                                slot_taken: p.slot_taken,
                                submitted: p.submitted,
                                wait_at: completed,
                                completed,
                                done,
                                lock_retries: 0,
                            },
                        }));
                    }
                    Err(e) => {
                        let message = e.to_string();
                        self.shared.fail(message.clone());
                        on_output(Err((p.meta, e)));
                        break message;
                    }
                }
            } else if kind == MEError {
                break "the encoder reported an error".into();
            }
        };
        self.shared.fail(error);
        let _ = returned.send(FAILED_SLOT);
        for p in pending.iter() {
            on_output(Err((
                p.meta,
                CaptureError::Platform("an earlier frame failed".into()),
            )));
        }
    }

    /// The frame's bitstream, with the parameter sets put in front of a keyframe that lacks them.
    fn finish(&mut self, sample: &IMFSample, asked: bool) -> Result<(Bytes, bool)> {
        let (data, marked) = sample_data(sample)?;
        let keyframe = is_keyframe(self.codec, &data, marked, asked);
        if !keyframe {
            return Ok((Bytes::from(data), false));
        }
        let fixed = self
            .header
            .as_deref()
            .and_then(|h| bitstream::with_parameter_sets(self.codec, &data, h));
        let fixed = fixed.or_else(|| {
            self.header = sequence_header(&self.com.transform, self.stream.id);
            self.header
                .as_deref()
                .and_then(|h| bitstream::with_parameter_sets(self.codec, &data, h))
        });
        Ok((Bytes::from(fixed.unwrap_or(data)), true))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};

    const W: u32 = 320;
    const H: u32 = 240;
    const FRAMES: u32 = 12;
    const FORCED: u32 = 6;

    /// Microsoft's software H.264 encoder, when this Windows has it.
    fn software_h264() -> Option<(&'static Api, IMFTransform)> {
        let api = match api() {
            Ok(api) => api,
            Err(e) => {
                eprintln!("skipped: {e}");
                return None;
            }
        };
        init_com();
        match unsafe { CoCreateInstance(&CLSID_MSH264EncoderMFT, None, CLSCTX_INPROC_SERVER) } {
            Ok(t) => Some((api, t)),
            Err(e) => {
                eprintln!("skipped: no software H.264 encoder ({e})");
                None
            }
        }
    }

    fn frame(api: &Api, n: u32) -> IMFSample {
        let size = W * H * 3 / 2;
        let buffer = api.memory_buffer(size).unwrap();
        unsafe {
            let mut data = ptr::null_mut();
            buffer.Lock(&mut data, None, None).unwrap();
            let pixels = std::slice::from_raw_parts_mut(data, size as usize);
            for (i, p) in pixels.iter_mut().enumerate() {
                *p = ((i as u32 + n * 7) % 251) as u8;
            }
            buffer.Unlock().unwrap();
            buffer.SetCurrentLength(size).unwrap();
        }
        let sample = api.sample().unwrap();
        unsafe {
            sample.AddBuffer(&buffer).unwrap();
            sample.SetSampleTime(i64::from(n) * 333_333).unwrap();
            sample.SetSampleDuration(333_333).unwrap();
        }
        sample
    }

    #[test]
    fn software_h264_negotiates_forces_keyframes_and_writes_annex_b() {
        let Some((api, transform)) = software_h264() else {
            return;
        };
        let settings = Settings {
            codec: VideoCodec::H264,
            width: W,
            height: H,
            fps: 30,
            kbps: 1000,
            vbv_frames: 1.0,
            quality_vs_speed: 0,
            fixed_qp: None,
        };
        let codec_api: ICodecAPI = transform.cast().unwrap();
        let ids = stream_ids(&transform);
        let fixed = configure(api, &transform, &codec_api, ids, &settings).unwrap();
        assert!(!fixed);
        let current = unsafe { transform.GetOutputCurrentType(ids.1) }.unwrap();
        assert_eq!(
            unsafe { current.GetGUID(&MF_MT_SUBTYPE) }.unwrap(),
            MFVideoFormat_H264
        );
        let input = unsafe { transform.GetInputCurrentType(ids.0) }.unwrap();
        assert_eq!(
            unsafe { input.GetGUID(&MF_MT_SUBTYPE) }.unwrap(),
            MFVideoFormat_NV12
        );
        let stream = OutputStream::new(&transform, ids.1, &settings).unwrap();
        unsafe {
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .unwrap();
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .unwrap();
        }

        let mut out: Vec<(i64, Vec<u8>, bool)> = Vec::new();
        let collect = |out: &mut Vec<(i64, Vec<u8>, bool)>| {
            loop {
                match stream.pull(api, &transform).unwrap() {
                    Pulled::Sample(sample) => {
                        let time = unsafe { sample.GetSampleTime() }.unwrap_or(-1);
                        let (mut data, marked) = sample_data(&sample).unwrap();
                        let keyframe = is_keyframe(VideoCodec::H264, &data, marked, false);
                        if keyframe {
                            if let Some(fixed) = sequence_header(&transform, ids.1).and_then(|h| {
                                bitstream::with_parameter_sets(VideoCodec::H264, &data, &h)
                            }) {
                                data = fixed;
                            }
                        }
                        out.push((time, data, keyframe));
                    }
                    Pulled::StreamChange => {}
                    Pulled::NeedInput => break,
                }
            }
        };
        for n in 0..FRAMES {
            if n == FORCED {
                assert!(set_value(
                    &codec_api,
                    &CODECAPI_AVEncVideoForceKeyFrame,
                    1u32.into(),
                    "a keyframe"
                ));
            }
            unsafe { transform.ProcessInput(ids.0, &frame(api, n), 0) }.unwrap();
            collect(&mut out);
        }
        unsafe { transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0) }.unwrap();
        collect(&mut out);

        assert_eq!(out.len(), FRAMES as usize, "one output per frame");
        for (_, data, _) in &out {
            assert!(bitstream::is_annex_b(data), "Annex-B output");
        }
        let keyframes: Vec<i64> = out
            .iter()
            .filter(|(_, _, k)| *k)
            .map(|(t, _, _)| *t)
            .collect();
        assert_eq!(keyframes.first(), Some(&0), "the first frame is an IDR");
        assert!(
            keyframes.contains(&(i64::from(FORCED) * 333_333)),
            "the forced frame is an IDR: {keyframes:?}"
        );
        for (_, data, keyframe) in &out {
            if *keyframe {
                let types: Vec<u8> = bitstream::nal_units(data).map(|u| u[0] & 0x1f).collect();
                assert!(
                    types.contains(&7) && types.contains(&8),
                    "SPS and PPS: {types:?}"
                );
            }
        }
    }
}
