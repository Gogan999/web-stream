//! NVENC encoder session on a D3D11 device, configured for interactive
//! streaming: ultra-low-latency tuning at the preset [`crate::tuning`] picks
//! for the config's quality, CBR with a VBV of about one frame and a
//! quarter-resolution first pass (so a scene cut stays near its budget), no
//! B-frames, an infinite GOP (keyframes only on demand), and
//! parameter sets repeated on every IDR so a decoder can join at any
//! keyframe.
//!
//! Keyframes get [`VideoConfig::keyframe_scale`] frames' worth of bits, so
//! the picture doesn't turn to mush every time the stream has to restart
//! from one. Where the GPU supports it, a frame that never reached the
//! browser is invalidated instead ([`NvEncoder::invalidate`]): the encoder
//! goes back to an older reference the browser does have, and no keyframe
//! is needed. Frames are tagged with their `frame_index` as NVENC's input
//! timestamp so they can be named.
//!
//! Frames are queued ([`NvEncoder::submit`]) and come out on a thread of
//! the encoder's own (NVENC's async mode, with completion events), so the
//! capture thread takes and converts the next desktop image while one
//! encodes. At most [`SLOTS`] frames are in the encoder: one encoding and
//! one queued, never a backlog.

use std::ffi::{CStr, c_void};
use std::ptr;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bytes::Bytes;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Device,
    ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST, WaitForSingleObject,
};
use windows::core::{Interface, s, w};
use ws_nvenc_sys as nv;
use ws_proto::{VideoCodec, VideoConfig};

use super::Gpu;
use crate::{CaptureError, Result};

/// The NVENC entry points from the driver's DLL, loaded once per process.
struct Api(nv::NV_ENCODE_API_FUNCTION_LIST);

// SAFETY: the function list is plain function pointers filled in once by the
// driver and never mutated afterwards; NVENC entry points are thread safe.
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

fn api() -> Result<&'static nv::NV_ENCODE_API_FUNCTION_LIST> {
    static API: OnceLock<std::result::Result<Api, String>> = OnceLock::new();
    API.get_or_init(load_api)
        .as_ref()
        .map(|a| &a.0)
        .map_err(|e| CaptureError::NoEncoder(e.clone()))
}

fn load_api() -> std::result::Result<Api, String> {
    // Only ever load the driver's copy from System32, never one planted next
    // to the executable or on PATH.
    let lib =
        unsafe { LoadLibraryExW(w!("nvEncodeAPI64.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) }
            .map_err(|e| format!("NVIDIA driver not found (nvEncodeAPI64.dll): {e}"))?;
    // The module stays loaded for the life of the process.
    let max_version = unsafe { GetProcAddress(lib, s!("NvEncodeAPIGetMaxSupportedVersion")) }
        .ok_or("NvEncodeAPIGetMaxSupportedVersion missing")?;
    let create = unsafe { GetProcAddress(lib, s!("NvEncodeAPICreateInstance")) }
        .ok_or("NvEncodeAPICreateInstance missing")?;
    // SAFETY: signatures match nvEncodeAPI.h.
    let max_version: nv::NvEncodeAPIGetMaxSupportedVersionFn =
        unsafe { std::mem::transmute(max_version) };
    let create: nv::NvEncodeAPICreateInstanceFn = unsafe { std::mem::transmute(create) };

    let mut driver_version = 0u32;
    check(
        unsafe { max_version(&mut driver_version) },
        "NvEncodeAPIGetMaxSupportedVersion",
    )
    .map_err(|e| e.to_string())?;
    if driver_version < nv::REQUIRED_DRIVER_API_VERSION {
        return Err(format!(
            "NVIDIA driver supports NVENC API {}.{}, need {}.{} (update the driver)",
            driver_version >> 4,
            driver_version & 0xf,
            nv::NVENCAPI_MAJOR_VERSION,
            nv::NVENCAPI_MINOR_VERSION
        ));
    }

    let mut list = nv::NV_ENCODE_API_FUNCTION_LIST {
        version: nv::NV_ENCODE_API_FUNCTION_LIST_VER,
        ..Default::default()
    };
    check(unsafe { create(&mut list) }, "NvEncodeAPICreateInstance").map_err(|e| e.to_string())?;
    Ok(Api(list))
}

fn check(status: nv::NVENCSTATUS, what: &str) -> Result<()> {
    if status == nv::NVENCSTATUS::NV_ENC_SUCCESS {
        Ok(())
    } else {
        Err(CaptureError::Platform(format!(
            "{what} failed: NVENCSTATUS {}",
            status.0
        )))
    }
}

macro_rules! call {
    ($api:expr, $f:ident ( $($arg:expr),* $(,)? )) => {{
        let f = $api.$f.ok_or(CaptureError::Platform(concat!(stringify!($f), " missing").into()))?;
        f($($arg),*)
    }};
}

/// Frames in the encoder at once: one being encoded while the next is
/// copied in and queued behind it. More would only queue frames, which is
/// latency.
pub(crate) const SLOTS: usize = 2;
/// A frame not out of the encoder in this long means the GPU hung.
const FRAME_TIMEOUT: Duration = Duration::from_secs(2);
/// Sent in place of a slot once a frame has failed: the encoder is done for.
const FAILED_SLOT: usize = usize::MAX;

/// One frame out of the encoder, with what the capture thread tagged it
/// with when it went in.
pub(crate) struct Encoded<M> {
    pub data: Bytes,
    pub keyframe: bool,
    pub meta: M,
    /// From going in to the bitstream being ready.
    pub encode_time: Duration,
}

/// What the output thread hands on for each frame.
pub(crate) type Finished<M> = std::result::Result<Encoded<M>, (M, CaptureError)>;

/// An input texture registered with the encoder, and the bitstream buffer
/// (and in async mode the event) its frame comes out through.
struct Slot {
    /// NV12 at the stream size: a copy of the converter's output, so the
    /// converter can take the next desktop image while this one encodes.
    texture: ID3D11Texture2D,
    registered: nv::NV_ENC_REGISTERED_PTR,
    bitstream: nv::NV_ENC_OUTPUT_PTR,
    /// Signalled when the frame is encoded; null when synchronous (the
    /// lock then waits instead).
    event: *mut c_void,
    /// Mapped for a frame in the encoder; unmapped once the slot is back.
    mapped: nv::NV_ENC_INPUT_PTR,
}

/// A frame in the encoder, for the output thread.
struct Pending<M> {
    slot: usize,
    bitstream: nv::NV_ENC_OUTPUT_PTR,
    event: *mut c_void,
    submitted: Instant,
    meta: M,
}

// SAFETY: the pointers are NVENC and event handles. The output thread only
// waits on the event and locks and unlocks the bitstream, which NVENC
// allows from a thread of its own; the capture thread keeps the slot alive
// until the slot comes back.
unsafe impl<M: Send> Send for Pending<M> {}

/// The encoder session, for the output thread.
#[derive(Clone, Copy)]
struct Session {
    api: &'static nv::NV_ENCODE_API_FUNCTION_LIST,
    encoder: *mut c_void,
}

// SAFETY: as for Pending; the session outlives the output thread (Drop
// joins it before destroying the encoder).
unsafe impl Send for Session {}

/// An NVENC session that encodes in the background: [`Self::submit`]
/// returns once the frame is queued, and a thread of its own hands each
/// finished frame to the callback given to [`Self::new`], in order. The
/// capture thread meanwhile takes and converts the next desktop image, so
/// capture, conversion and encoding overlap instead of running in turn.
pub(crate) struct NvEncoder<M: Send + 'static> {
    api: &'static nv::NV_ENCODE_API_FUNCTION_LIST,
    encoder: *mut c_void,
    device: ID3D11Device,
    // Boxed: `init.encodeConfig` points at `config`, and both are handed back
    // to the driver on reconfigure.
    init: Box<nv::NV_ENC_INITIALIZE_PARAMS>,
    config: Box<nv::NV_ENC_CONFIG>,
    fps: u32,
    vbv_frames: f64,
    keyframe_scale: u8,
    /// The GPU can invalidate reference frames, and the config asks for it.
    can_invalidate: bool,
    /// Completion events (NVENC's async mode); otherwise the output thread
    /// waits in the bitstream lock.
    async_mode: bool,
    slots: Vec<Slot>,
    /// Slots ready for a frame.
    free: Vec<usize>,
    /// Slots the output thread is done with.
    returned: std_mpsc::Receiver<usize>,
    returned_tx: std_mpsc::Sender<usize>,
    pending: Option<std_mpsc::Sender<Pending<M>>>,
    output: Option<JoinHandle<()>>,
    /// What went wrong on the output thread, for the next submit to report
    /// (the pipeline then rebuilds the encoder).
    failed: Arc<Mutex<Option<String>>>,
}

impl<M: Send + 'static> NvEncoder<M> {
    /// Opens an encoder on `gpu` for `cfg`'s stream size. [`Self::resize`]
    /// can later change the size in place up to `max` (width, height); at
    /// least the stream size. `on_output` gets every frame, on the
    /// encoder's output thread.
    pub(crate) fn new(
        gpu: &Gpu,
        cfg: &VideoConfig,
        max: (u32, u32),
        on_output: impl FnMut(Finished<M>) + Send + 'static,
    ) -> Result<Self> {
        let api = api()?;
        let mut session = nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
            version: nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER,
            deviceType: nv::NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_DIRECTX,
            device: gpu.device.as_raw(),
            apiVersion: nv::NVENCAPI_VERSION,
            ..Default::default()
        };
        let mut encoder = ptr::null_mut();
        let status = unsafe { call!(api, nvEncOpenEncodeSessionEx(&mut session, &mut encoder)) };
        if status != nv::NVENCSTATUS::NV_ENC_SUCCESS {
            // Common causes: not an NVIDIA adapter, or the consumer-GPU
            // concurrent session limit.
            return Err(CaptureError::NoEncoder(format!(
                "NVENC session failed to open on this adapter (NVENCSTATUS {})",
                status.0
            )));
        }
        let (returned_tx, returned) = std_mpsc::channel();
        // From here on Drop cleans up whatever has been created.
        let mut this = Self {
            api,
            encoder,
            device: gpu.device.clone(),
            init: Box::default(),
            config: Box::default(),
            fps: cfg.fps,
            vbv_frames: 1.0,
            keyframe_scale: 1,
            can_invalidate: false,
            async_mode: false,
            slots: Vec::new(),
            free: Vec::new(),
            returned,
            returned_tx,
            pending: None,
            output: None,
            failed: Arc::default(),
        };
        this.initialize(cfg, max)?;
        for _ in 0..SLOTS {
            this.add_slot(cfg.width, cfg.height)?;
        }
        this.free = (0..SLOTS).rev().collect();

        let (pending_tx, pending) = std_mpsc::channel();
        let session = Session { api, encoder };
        let (returned_tx, failed) = (this.returned_tx.clone(), this.failed.clone());
        this.output = Some(
            std::thread::Builder::new()
                .name("ws-encode-out".into())
                .spawn(move || {
                    let _ =
                        unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) };
                    output_loop(session, pending, returned_tx, failed, on_output)
                })
                .map_err(|e| CaptureError::Platform(format!("spawn encoder output thread: {e}")))?,
        );
        this.pending = Some(pending_tx);
        tracing::info!(codec = ?cfg.codec, cfg.width, cfg.height, cfg.fps, cfg.bitrate_kbps, quality = ?cfg.quality, keyframe_scale = this.keyframe_scale, invalidation = this.can_invalidate, two_pass = this.config.rcParams.multiPass.0 != 0, async_mode = this.async_mode, "NVENC ready");
        Ok(this)
    }

    fn initialize(&mut self, cfg: &VideoConfig, max: (u32, u32)) -> Result<()> {
        let codec_guid = match cfg.codec {
            VideoCodec::H264 => nv::NV_ENC_CODEC_H264_GUID,
            VideoCodec::H265 => nv::NV_ENC_CODEC_HEVC_GUID,
            VideoCodec::Av1 => nv::NV_ENC_CODEC_AV1_GUID,
        };
        let effort = crate::tuning::tuning(cfg);
        self.vbv_frames = effort.vbv_frames;
        self.keyframe_scale = effort.keyframe_scale;
        let preset_guid = match effort.preset {
            1 => nv::NV_ENC_PRESET_P1_GUID,
            2 => nv::NV_ENC_PRESET_P2_GUID,
            3 => nv::NV_ENC_PRESET_P3_GUID,
            4 => nv::NV_ENC_PRESET_P4_GUID,
            5 => nv::NV_ENC_PRESET_P5_GUID,
            6 => nv::NV_ENC_PRESET_P6_GUID,
            _ => nv::NV_ENC_PRESET_P7_GUID,
        };
        let tuning = nv::NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;

        let mut preset = nv::NV_ENC_PRESET_CONFIG {
            version: nv::NV_ENC_PRESET_CONFIG_VER,
            presetCfg: nv::NV_ENC_CONFIG {
                version: nv::NV_ENC_CONFIG_VER,
                ..Default::default()
            },
            ..Default::default()
        };
        let status = unsafe {
            call!(
                self.api,
                nvEncGetEncodePresetConfigEx(
                    self.encoder,
                    codec_guid,
                    preset_guid,
                    tuning,
                    &mut preset
                )
            )
        };
        if status != nv::NVENCSTATUS::NV_ENC_SUCCESS {
            return Err(CaptureError::NoEncoder(format!(
                "{:?} is not supported by this GPU's NVENC ({})",
                cfg.codec,
                self.last_error(status)
            )));
        }

        self.can_invalidate = cfg.recover_drops
            && self
                .cap(
                    codec_guid,
                    nv::NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_REF_PIC_INVALIDATION,
                )
                .is_ok_and(|v| v > 0);
        // With invalidation the encoder needs older pictures to fall back
        // on; one reference (the ultra-low-latency default) leaves it none.
        let refs = if self.can_invalidate { DPB_FRAMES } else { 0 };
        self.async_mode = self
            .cap(
                codec_guid,
                nv::NV_ENC_CAPS::NV_ENC_CAPS_ASYNC_ENCODE_SUPPORT,
            )
            .is_ok_and(|v| v > 0);

        let mut config = preset.presetCfg;
        config.gopLength = nv::NVENC_INFINITE_GOPLENGTH;
        config.frameIntervalP = 1;
        config.profileGUID = match cfg.codec {
            VideoCodec::H264 => nv::NV_ENC_H264_PROFILE_HIGH_GUID,
            VideoCodec::H265 => nv::NV_ENC_HEVC_PROFILE_MAIN_GUID,
            VideoCodec::Av1 => nv::NV_ENC_AV1_PROFILE_MAIN_GUID,
        };
        apply_rate(&mut config, cfg.bitrate_kbps, cfg.fps, self.vbv_frames);
        // The preset already implies these for ultra-low latency; set them
        // so a driver default change can't add frames of delay.
        config.rcParams.set_enableLookahead(0);
        config.rcParams.lookaheadDepth = 0;
        // A quarter-resolution first pass over each frame, so rate control
        // knows what the frame costs before it picks a quantizer. Without
        // it a scene cut came out at up to 20 times the frame's budget (a
        // 449 KB frame against about 25 KB in a real run), which backed the
        // send queue up for hundreds of milliseconds. NVIDIA recommends it
        // for low-latency CBR with a one-frame VBV; it adds no frames of
        // delay, only a little encode time.
        config.rcParams.multiPass = nv::NV_ENC_MULTI_PASS::NV_ENC_TWO_PASS_QUARTER_RESOLUTION;
        // SAFETY: the union member matches the codec GUID chosen above.
        unsafe {
            let codec = &mut config.encodeCodecConfig;
            match cfg.codec {
                VideoCodec::H264 => {
                    let h = &mut codec.h264Config;
                    h.idrPeriod = nv::NVENC_INFINITE_GOPLENGTH;
                    if refs > 0 {
                        h.maxNumRefFrames = refs;
                    }
                    h.level = nv::NV_ENC_LEVEL::NV_ENC_LEVEL_H264_52.0;
                    h.set_repeatSPSPPS(1);
                    set_vui_bt709(&mut h.h264VUIParameters);
                }
                VideoCodec::H265 => {
                    let h = &mut codec.hevcConfig;
                    h.idrPeriod = nv::NVENC_INFINITE_GOPLENGTH;
                    if refs > 0 {
                        h.maxNumRefFramesInDPB = refs;
                    }
                    h.level = nv::NV_ENC_LEVEL::NV_ENC_LEVEL_HEVC_51.0;
                    h.tier = nv::NV_ENC_LEVEL::NV_ENC_TIER_HEVC_MAIN.0;
                    h.set_repeatSPSPPS(1);
                    set_vui_bt709(&mut h.hevcVUIParameters);
                }
                VideoCodec::Av1 => {
                    let a = &mut codec.av1Config;
                    a.idrPeriod = nv::NVENC_INFINITE_GOPLENGTH;
                    if refs > 0 {
                        a.maxNumRefFramesInDPB = refs;
                    }
                    a.level = nv::NV_ENC_LEVEL::NV_ENC_LEVEL_AV1_51.0;
                    a.tier = 0;
                    a.set_repeatSeqHdr(1);
                    // Low-overhead OBU stream, as the WebRTC AV1 payload expects.
                    a.set_outputAnnexBFormat(0);
                    a.colorPrimaries =
                        nv::NV_ENC_VUI_COLOR_PRIMARIES::NV_ENC_VUI_COLOR_PRIMARIES_BT709;
                    a.transferCharacteristics =
                        nv::NV_ENC_VUI_TRANSFER_CHARACTERISTIC::NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
                    a.matrixCoefficients =
                        nv::NV_ENC_VUI_MATRIX_COEFFS::NV_ENC_VUI_MATRIX_COEFFS_BT709;
                    a.colorRange = 0;
                }
            }
        }
        *self.config = config;

        *self.init = nv::NV_ENC_INITIALIZE_PARAMS {
            version: nv::NV_ENC_INITIALIZE_PARAMS_VER,
            encodeGUID: codec_guid,
            presetGUID: preset_guid,
            encodeWidth: cfg.width,
            encodeHeight: cfg.height,
            darWidth: cfg.width,
            darHeight: cfg.height,
            frameRateNum: cfg.fps,
            frameRateDen: 1,
            enableEncodeAsync: u32::from(self.async_mode),
            enablePTD: 1,
            encodeConfig: &mut *self.config,
            maxEncodeWidth: max.0.max(cfg.width),
            maxEncodeHeight: max.1.max(cfg.height),
            tuningInfo: tuning,
            ..Default::default()
        };
        let mut status = unsafe {
            call!(
                self.api,
                nvEncInitializeEncoder(self.encoder, &mut *self.init)
            )
        };
        if status != nv::NVENCSTATUS::NV_ENC_SUCCESS {
            // An older GPU or driver may refuse the first pass: do without.
            tracing::warn!(
                "encoder refused two-pass rate control ({}); using one pass",
                self.last_error(status)
            );
            self.config.rcParams.multiPass = nv::NV_ENC_MULTI_PASS::NV_ENC_MULTI_PASS_DISABLED;
            status = unsafe {
                call!(
                    self.api,
                    nvEncInitializeEncoder(self.encoder, &mut *self.init)
                )
            };
        }
        if status != nv::NVENCSTATUS::NV_ENC_SUCCESS {
            return Err(CaptureError::NoEncoder(format!(
                "nvEncInitializeEncoder: {}",
                self.last_error(status)
            )));
        }
        Ok(())
    }

    /// Adds an input texture of `width` x `height`, with its bitstream
    /// buffer and, in async mode, its completion event.
    fn add_slot(&mut self, width: u32, height: u32) -> Result<()> {
        let texture = nv12_texture(&self.device, width, height)?;
        let mut bs = nv::NV_ENC_CREATE_BITSTREAM_BUFFER {
            version: nv::NV_ENC_CREATE_BITSTREAM_BUFFER_VER,
            ..Default::default()
        };
        self.check(
            unsafe { call!(self.api, nvEncCreateBitstreamBuffer(self.encoder, &mut bs)) },
            "nvEncCreateBitstreamBuffer",
        )?;
        // Pushed before anything else can fail, so Drop frees what was made.
        self.slots.push(Slot {
            texture,
            registered: ptr::null_mut(),
            bitstream: bs.bitstreamBuffer,
            event: ptr::null_mut(),
            mapped: ptr::null_mut(),
        });
        let index = self.slots.len() - 1;
        if self.async_mode {
            let event = unsafe { CreateEventW(None, false, false, None) }
                .map_err(|e| CaptureError::Platform(format!("CreateEvent: {e}")))?;
            self.slots[index].event = event.0;
            // SAFETY: plain data; zero is every reserved field's value.
            let mut params: nv::NV_ENC_EVENT_PARAMS = unsafe { std::mem::zeroed() };
            params.version = nv::NV_ENC_EVENT_PARAMS_VER;
            params.completionEvent = event.0;
            let status =
                unsafe { call!(self.api, nvEncRegisterAsyncEvent(self.encoder, &mut params)) };
            if let Err(e) = self.check(status, "nvEncRegisterAsyncEvent") {
                unsafe {
                    let _ = CloseHandle(event);
                }
                self.slots[index].event = ptr::null_mut();
                return Err(e);
            }
        }
        self.register(index, width, height)
    }

    fn register(&mut self, index: usize, width: u32, height: u32) -> Result<()> {
        let mut reg = nv::NV_ENC_REGISTER_RESOURCE {
            version: nv::NV_ENC_REGISTER_RESOURCE_VER,
            resourceType: nv::NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX,
            width,
            height,
            pitch: 0,
            resourceToRegister: self.slots[index].texture.as_raw(),
            bufferFormat: nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12,
            bufferUsage: nv::NV_ENC_BUFFER_USAGE::NV_ENC_INPUT_IMAGE,
            ..Default::default()
        };
        let status = unsafe { call!(self.api, nvEncRegisterResource(self.encoder, &mut reg)) };
        self.check(status, "nvEncRegisterResource")?;
        self.slots[index].registered = reg.registeredResource;
        Ok(())
    }

    fn unregister(&mut self, index: usize) {
        self.unmap(index);
        let slot = &mut self.slots[index];
        if !slot.registered.is_null() {
            if let Some(f) = self.api.nvEncUnregisterResource {
                unsafe { f(self.encoder, slot.registered) };
            }
            slot.registered = ptr::null_mut();
        }
    }

    fn unmap(&mut self, index: usize) {
        let slot = &mut self.slots[index];
        if !slot.mapped.is_null() {
            if let Some(f) = self.api.nvEncUnmapInputResource {
                unsafe { f(self.encoder, slot.mapped) };
            }
            slot.mapped = ptr::null_mut();
        }
    }

    /// Changes the encoded size in place; the next frame is an IDR. Cheaper
    /// than a new session: no new device or encoder, so no stall. Waits for
    /// the frames in the encoder first. Fails if the size is over the
    /// maximum the encoder was opened for, or the driver refuses; the
    /// encoder must then be replaced.
    pub(crate) fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        if width > self.init.maxEncodeWidth || height > self.init.maxEncodeHeight {
            return Err(CaptureError::InvalidConfig(format!(
                "{width}x{height} is larger than the encoder was opened for ({}x{})",
                self.init.maxEncodeWidth, self.init.maxEncodeHeight
            )));
        }
        self.drain()?;
        for index in 0..self.slots.len() {
            self.unregister(index);
        }
        let previous = (
            self.init.encodeWidth,
            self.init.encodeHeight,
            self.init.darWidth,
            self.init.darHeight,
        );
        self.init.encodeWidth = width;
        self.init.encodeHeight = height;
        self.init.darWidth = width;
        self.init.darHeight = height;
        let mut params = nv::NV_ENC_RECONFIGURE_PARAMS {
            version: nv::NV_ENC_RECONFIGURE_PARAMS_VER,
            reInitEncodeParams: *self.init,
            ..Default::default()
        };
        params.set_resetEncoder(1);
        params.set_forceIDR(1);
        let status = unsafe { call!(self.api, nvEncReconfigureEncoder(self.encoder, &mut params)) };
        if let Err(e) = self.check(status, "nvEncReconfigureEncoder (resize)") {
            // Keep our copy in sync with what the encoder is still running.
            (
                self.init.encodeWidth,
                self.init.encodeHeight,
                self.init.darWidth,
                self.init.darHeight,
            ) = previous;
            return Err(e);
        }
        for index in 0..self.slots.len() {
            self.slots[index].texture = nv12_texture(&self.device, width, height)?;
            self.register(index, width, height)?;
        }
        Ok(())
    }

    /// Waits until every frame in the encoder has come out (and been handed
    /// to the callback).
    pub(crate) fn drain(&mut self) -> Result<()> {
        while self.free.len() < self.slots.len() {
            let slot = self.returned_slot()?;
            self.free.push(slot);
        }
        for index in 0..self.slots.len() {
            self.unmap(index);
        }
        Ok(())
    }

    /// A slot for the next frame, waiting for one to come back if the
    /// encoder holds them all.
    fn take_slot(&mut self) -> Result<usize> {
        let slot = match self.free.pop() {
            Some(slot) => slot,
            None => self.returned_slot()?,
        };
        self.unmap(slot);
        Ok(slot)
    }

    /// The next slot the output thread gives back.
    fn returned_slot(&mut self) -> Result<usize> {
        match self.returned.recv_timeout(FRAME_TIMEOUT * 2) {
            Ok(FAILED_SLOT) => {
                let e = self.failed.lock().unwrap_or_else(|e| e.into_inner()).take();
                Err(CaptureError::Platform(
                    e.unwrap_or_else(|| "a frame failed in the encoder".into()),
                ))
            }
            Ok(slot) => Ok(slot),
            Err(_) => Err(CaptureError::Platform(
                "the encoder stopped returning frames".into(),
            )),
        }
    }

    /// Queues `src` (NV12, the stream size) for encoding and returns; the
    /// frame comes out on the output thread with `meta`. `timestamp` names
    /// the frame for [`Self::invalidate`]. `intra` says the frame will be
    /// coded from scratch without being an IDR (every reference was
    /// invalidated), so it gets a keyframe's budget too.
    ///
    /// `kbps` is a new bitrate to use from this frame on (after it, for a
    /// keyframe, so the change shares the reconfigure that restores the
    /// ordinary frame budget). [`Self::bitrate_kbps`] says what took.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn submit(
        &mut self,
        context: &ID3D11DeviceContext,
        src: &ID3D11Texture2D,
        timestamp: u64,
        force_idr: bool,
        intra: bool,
        kbps: Option<u32>,
        meta: M,
    ) -> Result<()> {
        if let Some(e) = self.failed.lock().unwrap_or_else(|e| e.into_inner()).take() {
            return Err(CaptureError::Platform(e));
        }
        let index = self.take_slot()?;
        let result = self.submit_to(index, context, src, timestamp, force_idr, intra, kbps);
        let Err(e) = result else {
            let slot = &self.slots[index];
            let pending = Pending {
                slot: index,
                bitstream: slot.bitstream,
                event: slot.event,
                submitted: Instant::now(),
                meta,
            };
            return match self.pending.as_ref().map(|p| p.send(pending)) {
                Some(Ok(())) => Ok(()),
                _ => Err(CaptureError::Platform(
                    "the encoder output thread stopped".into(),
                )),
            };
        };
        self.unmap(index);
        self.free.push(index);
        Err(e)
    }

    #[allow(clippy::too_many_arguments)]
    fn submit_to(
        &mut self,
        index: usize,
        context: &ID3D11DeviceContext,
        src: &ID3D11Texture2D,
        timestamp: u64,
        force_idr: bool,
        intra: bool,
        kbps: Option<u32>,
    ) -> Result<()> {
        unsafe { context.CopyResource(&self.slots[index].texture, src) };
        let mut map = nv::NV_ENC_MAP_INPUT_RESOURCE {
            version: nv::NV_ENC_MAP_INPUT_RESOURCE_VER,
            registeredResource: self.slots[index].registered,
            ..Default::default()
        };
        let status = unsafe { call!(self.api, nvEncMapInputResource(self.encoder, &mut map)) };
        self.check(status, "nvEncMapInputResource")?;
        self.slots[index].mapped = map.mappedResource;

        // Let the keyframe take a few frames' worth of bits, then go back.
        // Reconfiguring applies to the frames queued after it, so the
        // budget goes back as soon as this one is queued. A failed change
        // only costs keyframe sharpness.
        let big_keyframe = (force_idr || intra) && self.keyframe_scale > 1;
        if big_keyframe {
            let vbv = self.vbv_frames * f64::from(self.keyframe_scale);
            if let Err(e) = self.reconfigure_rate(self.kbps(), vbv, self.keyframe_scale) {
                tracing::debug!("larger keyframe budget refused: {e}");
            }
        } else if let Some(kbps) = kbps {
            if let Err(e) = self.reconfigure_rate(kbps, self.vbv_frames, 1) {
                tracing::debug!("bitrate change to {kbps} kbps refused: {e}");
            }
        }
        let mut flags = 0;
        if force_idr {
            flags |= nv::NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_FORCEIDR.0
                | nv::NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_OUTPUT_SPSPPS.0;
        }
        let slot = &self.slots[index];
        let mut pic = nv::NV_ENC_PIC_PARAMS {
            version: nv::NV_ENC_PIC_PARAMS_VER,
            inputWidth: self.init.encodeWidth,
            inputHeight: self.init.encodeHeight,
            encodePicFlags: flags,
            inputTimeStamp: timestamp,
            inputBuffer: map.mappedResource,
            outputBitstream: slot.bitstream,
            completionEvent: slot.event,
            bufferFmt: map.mappedBufferFmt,
            pictureStruct: nv::NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME,
            ..Default::default()
        };
        let status = unsafe { call!(self.api, nvEncEncodePicture(self.encoder, &mut pic)) };
        if big_keyframe {
            let kbps = kbps.unwrap_or(self.kbps());
            if let Err(e) = self.reconfigure_rate(kbps, self.vbv_frames, 1) {
                tracing::warn!("restoring the frame budget after a keyframe failed: {e}");
            }
        }
        // No B-frames, so never NV_ENC_ERR_NEED_MORE_INPUT: every frame
        // comes out in order.
        self.check(status, "nvEncEncodePicture")
    }

    /// Stops the encoder referencing the frame encoded with `timestamp`.
    /// Only that frame: to drop a run of frames, invalidate each one. The
    /// next frame uses an older reference, or is coded intra (but is not an
    /// IDR) if none is left.
    pub(crate) fn invalidate(&mut self, timestamp: u64) -> Result<()> {
        if !self.can_invalidate {
            return Err(CaptureError::Platform(
                "reference invalidation not available".into(),
            ));
        }
        let status = unsafe { call!(self.api, nvEncInvalidateRefFrames(self.encoder, timestamp)) };
        self.check(status, "nvEncInvalidateRefFrames")
    }

    pub(crate) fn can_invalidate(&self) -> bool {
        self.can_invalidate
    }

    /// The bitrate the encoder is running at.
    pub(crate) fn bitrate_kbps(&self) -> u32 {
        self.kbps()
    }

    fn kbps(&self) -> u32 {
        self.config.rcParams.averageBitRate / 1000
    }

    fn reconfigure_rate(&mut self, kbps: u32, vbv_frames: f64, keyframe_scale: u8) -> Result<()> {
        let previous = self.config.rcParams;
        apply_rate(&mut self.config, kbps, self.fps, vbv_frames);
        // The ratio of keyframe to ordinary frame bits rate control aims for
        // (ultra-low latency defaults to 1).
        self.config.rcParams.lowDelayKeyFrameScale = keyframe_scale;
        let mut params = nv::NV_ENC_RECONFIGURE_PARAMS {
            version: nv::NV_ENC_RECONFIGURE_PARAMS_VER,
            reInitEncodeParams: *self.init,
            ..Default::default()
        };
        let status = unsafe { call!(self.api, nvEncReconfigureEncoder(self.encoder, &mut params)) };
        let result = self.check(status, "nvEncReconfigureEncoder");
        if result.is_err() {
            // Keep our copy in sync with what the encoder is still running.
            self.config.rcParams = previous;
        }
        result
    }

    fn check(&self, status: nv::NVENCSTATUS, what: &str) -> Result<()> {
        if status == nv::NVENCSTATUS::NV_ENC_SUCCESS {
            Ok(())
        } else {
            Err(CaptureError::Platform(format!(
                "{what}: {}",
                self.last_error(status)
            )))
        }
    }

    /// One capability of this GPU's encoder for `codec`.
    fn cap(&self, codec: nv::GUID, cap: nv::NV_ENC_CAPS) -> Result<i32> {
        let mut param = nv::NV_ENC_CAPS_PARAM {
            version: nv::NV_ENC_CAPS_PARAM_VER,
            capsToQuery: cap,
            ..Default::default()
        };
        let mut value = 0;
        let status = unsafe {
            call!(
                self.api,
                nvEncGetEncodeCaps(self.encoder, codec, &mut param, &mut value)
            )
        };
        self.check(status, "nvEncGetEncodeCaps")?;
        Ok(value)
    }

    fn last_error(&self, status: nv::NVENCSTATUS) -> String {
        last_error(self.api, self.encoder, status)
    }
}

impl<M: Send + 'static> Drop for NvEncoder<M> {
    fn drop(&mut self) {
        // The output thread finishes the frames in the encoder and exits.
        self.pending.take();
        if let Some(t) = self.output.take() {
            let _ = t.join();
        }
        let api = self.api;
        for index in 0..self.slots.len() {
            self.unregister(index);
            let slot = &self.slots[index];
            unsafe {
                if !slot.bitstream.is_null() {
                    if let Some(f) = api.nvEncDestroyBitstreamBuffer {
                        f(self.encoder, slot.bitstream);
                    }
                }
                if !slot.event.is_null() {
                    // SAFETY: plain data; zero is every reserved field's value.
                    let mut params: nv::NV_ENC_EVENT_PARAMS = std::mem::zeroed();
                    params.version = nv::NV_ENC_EVENT_PARAMS_VER;
                    params.completionEvent = slot.event;
                    if let Some(f) = api.nvEncUnregisterAsyncEvent {
                        f(self.encoder, &mut params);
                    }
                    let _ = CloseHandle(HANDLE(slot.event));
                }
            }
        }
        unsafe {
            if let Some(f) = api.nvEncDestroyEncoder {
                f(self.encoder);
            }
        }
    }
}

/// The output thread: takes each frame out of the encoder, in order, gives
/// its slot back, and hands it on.
fn output_loop<M: Send>(
    session: Session,
    pending: std_mpsc::Receiver<Pending<M>>,
    returned: std_mpsc::Sender<usize>,
    failed: Arc<Mutex<Option<String>>>,
    mut on_output: impl FnMut(Finished<M>),
) {
    let mut broken = false;
    while let Ok(p) = pending.recv() {
        let result = if broken {
            Err(CaptureError::Platform("an earlier frame failed".into()))
        } else {
            finish(session, &p)
        };
        let encode_time = p.submitted.elapsed();
        match result {
            Ok((data, keyframe)) => {
                // Back first: the capture thread may be waiting for it.
                let _ = returned.send(p.slot);
                on_output(Ok(Encoded {
                    data,
                    keyframe,
                    meta: p.meta,
                    encode_time,
                }));
            }
            Err(e) => {
                // The slot never goes back: after a timeout the GPU may still
                // own its texture, bitstream and event, and a late finish
                // would signal whatever frame used the event next. Failed is
                // set first, so the next submit stops, and a capture thread
                // already waiting for a slot hears it at once.
                *failed.lock().unwrap_or_else(|e| e.into_inner()) = Some(e.to_string());
                if !broken {
                    let _ = returned.send(FAILED_SLOT);
                }
                broken = true;
                on_output(Err((p.meta, e)));
            }
        }
    }
}

/// Waits for one frame and copies its bitstream out.
fn finish<M>(session: Session, p: &Pending<M>) -> Result<(Bytes, bool)> {
    let Session { api, encoder } = session;
    if !p.event.is_null() {
        let waited =
            unsafe { WaitForSingleObject(HANDLE(p.event), FRAME_TIMEOUT.as_millis() as u32) };
        if waited != WAIT_OBJECT_0 {
            return Err(CaptureError::Platform(format!(
                "the encoder did not finish a frame in {} s",
                FRAME_TIMEOUT.as_secs()
            )));
        }
    }
    let mut lock = nv::NV_ENC_LOCK_BITSTREAM {
        version: nv::NV_ENC_LOCK_BITSTREAM_VER,
        outputBitstream: p.bitstream,
        ..Default::default()
    };
    let status = unsafe { call!(api, nvEncLockBitstream(encoder, &mut lock)) };
    if status != nv::NVENCSTATUS::NV_ENC_SUCCESS {
        return Err(CaptureError::Platform(format!(
            "nvEncLockBitstream: {}",
            last_error(api, encoder, status)
        )));
    }
    // SAFETY: NVENC guarantees the pointer/size describe the locked
    // bitstream until nvEncUnlockBitstream.
    let data: &[u8] = if lock.bitstreamBufferPtr.is_null() || lock.bitstreamSizeInBytes == 0 {
        &[]
    } else {
        unsafe {
            std::slice::from_raw_parts(
                lock.bitstreamBufferPtr as *const u8,
                lock.bitstreamSizeInBytes as usize,
            )
        }
    };
    let data = Bytes::copy_from_slice(data);
    let keyframe = lock.pictureType == nv::NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR;
    let status = unsafe { call!(api, nvEncUnlockBitstream(encoder, p.bitstream)) };
    if status != nv::NVENCSTATUS::NV_ENC_SUCCESS {
        return Err(CaptureError::Platform(format!(
            "nvEncUnlockBitstream: {}",
            last_error(api, encoder, status)
        )));
    }
    Ok((data, keyframe))
}

fn last_error(
    api: &nv::NV_ENCODE_API_FUNCTION_LIST,
    encoder: *mut c_void,
    status: nv::NVENCSTATUS,
) -> String {
    let detail = api
        .nvEncGetLastErrorString
        .map(|f| unsafe { f(encoder) })
        .filter(|p| !p.is_null())
        .map(|p| unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!("NVENCSTATUS {} {detail}", status.0)
}

/// An NV12 texture the encoder can read.
fn nv12_texture(device: &ID3D11Device, width: u32, height: u32) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .map_err(|e| CaptureError::Platform(format!("CreateTexture2D NV12: {e}")))?;
    texture.ok_or(CaptureError::Platform("no NV12 texture".into()))
}

/// Reference pictures kept when invalidation is on: how far back the encoder
/// can reach for a picture the browser has.
pub(crate) const DPB_FRAMES: u32 = 4;

/// CBR at `kbps` with a VBV of `vbv_frames` frames: every frame fits in
/// about one frame time on the wire, so the network never has to buffer a
/// long burst.
fn apply_rate(config: &mut nv::NV_ENC_CONFIG, kbps: u32, fps: u32, vbv_frames: f64) {
    let bps = kbps.saturating_mul(1000);
    let rc = &mut config.rcParams;
    rc.rateControlMode = nv::NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR;
    rc.averageBitRate = bps;
    rc.maxBitRate = bps;
    rc.vbvBufferSize = (f64::from(bps) / f64::from(fps.max(1)) * vbv_frames) as u32;
    rc.vbvInitialDelay = rc.vbvBufferSize;
}

/// BT.709 limited range, matching what the video processor writes.
fn set_vui_bt709(vui: &mut nv::NV_ENC_CONFIG_H264_VUI_PARAMETERS) {
    vui.videoSignalTypePresentFlag = 1;
    vui.videoFormat = nv::NV_ENC_VUI_VIDEO_FORMAT::NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
    vui.videoFullRangeFlag = 0;
    vui.colourDescriptionPresentFlag = 1;
    vui.colourPrimaries = nv::NV_ENC_VUI_COLOR_PRIMARIES::NV_ENC_VUI_COLOR_PRIMARIES_BT709;
    vui.transferCharacteristics =
        nv::NV_ENC_VUI_TRANSFER_CHARACTERISTIC::NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
    vui.colourMatrix = nv::NV_ENC_VUI_MATRIX_COEFFS::NV_ENC_VUI_MATRIX_COEFFS_BT709;
}
