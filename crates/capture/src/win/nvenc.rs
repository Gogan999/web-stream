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
//! Runs synchronously: one frame in flight, and `encode` returns once the
//! bitstream is ready. That is the lowest-latency mode for a single stream.

use std::ffi::{CStr, c_void};
use std::ptr;
use std::sync::OnceLock;

use bytes::Bytes;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
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

pub(crate) struct EncodedBits {
    pub data: Bytes,
    pub keyframe: bool,
}

pub(crate) struct NvEncoder {
    api: &'static nv::NV_ENCODE_API_FUNCTION_LIST,
    encoder: *mut c_void,
    // Boxed: `init.encodeConfig` points at `config`, and both are handed back
    // to the driver on reconfigure.
    init: Box<nv::NV_ENC_INITIALIZE_PARAMS>,
    config: Box<nv::NV_ENC_CONFIG>,
    fps: u32,
    vbv_frames: f64,
    keyframe_scale: u8,
    /// The GPU can invalidate reference frames, and the config asks for it.
    can_invalidate: bool,
    input: nv::NV_ENC_REGISTERED_PTR,
    bitstream: nv::NV_ENC_OUTPUT_PTR,
}

impl NvEncoder {
    /// Opens an encoder on `gpu` that reads from `input` (NV12, the stream
    /// size). [`Self::resize`] can later change the size in place up to
    /// `max` (width, height); at least the stream size.
    pub(crate) fn new(
        gpu: &Gpu,
        input: &ID3D11Texture2D,
        cfg: &VideoConfig,
        max: (u32, u32),
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
        // From here on Drop cleans up whatever has been created.
        let mut this = Self {
            api,
            encoder,
            init: Box::default(),
            config: Box::default(),
            fps: cfg.fps,
            vbv_frames: 1.0,
            keyframe_scale: 1,
            can_invalidate: false,
            input: ptr::null_mut(),
            bitstream: ptr::null_mut(),
        };
        this.initialize(cfg, max)?;
        this.register_input(input, cfg.width, cfg.height)?;

        let mut bs = nv::NV_ENC_CREATE_BITSTREAM_BUFFER {
            version: nv::NV_ENC_CREATE_BITSTREAM_BUFFER_VER,
            ..Default::default()
        };
        this.check(
            unsafe { call!(api, nvEncCreateBitstreamBuffer(encoder, &mut bs)) },
            "nvEncCreateBitstreamBuffer",
        )?;
        this.bitstream = bs.bitstreamBuffer;
        tracing::info!(codec = ?cfg.codec, cfg.width, cfg.height, cfg.fps, cfg.bitrate_kbps, quality = ?cfg.quality, keyframe_scale = this.keyframe_scale, invalidation = this.can_invalidate, two_pass = this.config.rcParams.multiPass.0 != 0, "NVENC ready");
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
            enableEncodeAsync: 0,
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

    /// Changes the encoded size in place, reading from `input` (NV12, the
    /// new size) from now on; the next frame is an IDR. Cheaper than a new
    /// session: no new device or encoder, so no stall. Fails if the size is
    /// over the maximum the encoder was opened for, or the driver refuses;
    /// the encoder then has no input and must be replaced.
    pub(crate) fn resize(
        &mut self,
        input: &ID3D11Texture2D,
        width: u32,
        height: u32,
    ) -> Result<()> {
        if width > self.init.maxEncodeWidth || height > self.init.maxEncodeHeight {
            return Err(CaptureError::InvalidConfig(format!(
                "{width}x{height} is larger than the encoder was opened for ({}x{})",
                self.init.maxEncodeWidth, self.init.maxEncodeHeight
            )));
        }
        self.unregister_input();
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
        self.register_input(input, width, height)
    }

    fn unregister_input(&mut self) {
        if !self.input.is_null() {
            if let Some(f) = self.api.nvEncUnregisterResource {
                unsafe { f(self.encoder, self.input) };
            }
            self.input = ptr::null_mut();
        }
    }

    fn register_input(&mut self, input: &ID3D11Texture2D, width: u32, height: u32) -> Result<()> {
        let mut reg = nv::NV_ENC_REGISTER_RESOURCE {
            version: nv::NV_ENC_REGISTER_RESOURCE_VER,
            resourceType: nv::NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX,
            width,
            height,
            pitch: 0,
            resourceToRegister: input.as_raw(),
            bufferFormat: nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12,
            bufferUsage: nv::NV_ENC_BUFFER_USAGE::NV_ENC_INPUT_IMAGE,
            ..Default::default()
        };
        let status = unsafe { call!(self.api, nvEncRegisterResource(self.encoder, &mut reg)) };
        self.check(status, "nvEncRegisterResource")?;
        self.input = reg.registeredResource;
        Ok(())
    }

    /// Encodes the current contents of the registered input texture.
    /// `timestamp` names the frame for [`Self::invalidate`]. `intra` says
    /// the frame will be coded from scratch without being an IDR (every
    /// reference was invalidated), so it gets a keyframe's budget too.
    ///
    /// `kbps` is a new bitrate to use from this frame on (after it, for a
    /// keyframe, so the change shares the reconfigure that restores the
    /// ordinary frame budget). [`Self::bitrate_kbps`] says what took.
    pub(crate) fn encode(
        &mut self,
        timestamp: u64,
        force_idr: bool,
        intra: bool,
        kbps: Option<u32>,
    ) -> Result<EncodedBits> {
        // Let the keyframe take a few frames' worth of bits, then go back.
        // A failed change only costs keyframe sharpness.
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
        let bits = self.encode_input(timestamp, force_idr);
        if big_keyframe {
            let kbps = kbps.unwrap_or(self.kbps());
            if let Err(e) = self.reconfigure_rate(kbps, self.vbv_frames, 1) {
                tracing::warn!("restoring the frame budget after a keyframe failed: {e}");
            }
        }
        bits
    }

    fn encode_input(&mut self, timestamp: u64, force_idr: bool) -> Result<EncodedBits> {
        let mut map = nv::NV_ENC_MAP_INPUT_RESOURCE {
            version: nv::NV_ENC_MAP_INPUT_RESOURCE_VER,
            registeredResource: self.input,
            ..Default::default()
        };
        let status = unsafe { call!(self.api, nvEncMapInputResource(self.encoder, &mut map)) };
        self.check(status, "nvEncMapInputResource")?;
        let result = self.encode_mapped(&map, timestamp, force_idr);
        let status = unsafe {
            call!(
                self.api,
                nvEncUnmapInputResource(self.encoder, map.mappedResource)
            )
        };
        let bits = result?;
        self.check(status, "nvEncUnmapInputResource")?;
        Ok(bits)
    }

    fn encode_mapped(
        &self,
        map: &nv::NV_ENC_MAP_INPUT_RESOURCE,
        timestamp: u64,
        force_idr: bool,
    ) -> Result<EncodedBits> {
        let mut flags = 0;
        if force_idr {
            flags |= nv::NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_FORCEIDR.0
                | nv::NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_OUTPUT_SPSPPS.0;
        }
        let mut pic = nv::NV_ENC_PIC_PARAMS {
            version: nv::NV_ENC_PIC_PARAMS_VER,
            inputWidth: self.init.encodeWidth,
            inputHeight: self.init.encodeHeight,
            encodePicFlags: flags,
            inputTimeStamp: timestamp,
            inputBuffer: map.mappedResource,
            outputBitstream: self.bitstream,
            bufferFmt: map.mappedBufferFmt,
            pictureStruct: nv::NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME,
            ..Default::default()
        };
        let status = unsafe { call!(self.api, nvEncEncodePicture(self.encoder, &mut pic)) };
        self.check(status, "nvEncEncodePicture")?;

        let mut lock = nv::NV_ENC_LOCK_BITSTREAM {
            version: nv::NV_ENC_LOCK_BITSTREAM_VER,
            outputBitstream: self.bitstream,
            ..Default::default()
        };
        let status = unsafe { call!(self.api, nvEncLockBitstream(self.encoder, &mut lock)) };
        self.check(status, "nvEncLockBitstream")?;
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
        let bits = EncodedBits {
            data: Bytes::copy_from_slice(data),
            keyframe: lock.pictureType == nv::NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR,
        };
        let status = unsafe { call!(self.api, nvEncUnlockBitstream(self.encoder, self.bitstream)) };
        self.check(status, "nvEncUnlockBitstream")?;
        Ok(bits)
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
        let detail = self
            .api
            .nvEncGetLastErrorString
            .map(|f| unsafe { f(self.encoder) })
            .filter(|p| !p.is_null())
            .map(|p| unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
            .unwrap_or_default();
        format!("NVENCSTATUS {} {detail}", status.0)
    }
}

impl Drop for NvEncoder {
    fn drop(&mut self) {
        let api = self.api;
        unsafe {
            if !self.bitstream.is_null() {
                if let Some(f) = api.nvEncDestroyBitstreamBuffer {
                    f(self.encoder, self.bitstream);
                }
            }
        }
        self.unregister_input();
        unsafe {
            if let Some(f) = api.nvEncDestroyEncoder {
                f(self.encoder);
            }
        }
    }
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
