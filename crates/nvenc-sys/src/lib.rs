//! Raw bindings to `nvEncodeAPI.h` from the NVIDIA Video Codec SDK 12.1.
//!
//! Nothing here links against NVIDIA libraries: callers load
//! `nvEncodeAPI64.dll` (shipped with the driver) at runtime and resolve
//! [`NvEncodeAPICreateInstanceFn`] / [`NvEncodeAPIGetMaxSupportedVersionFn`]
//! from it. SDK 12.1 needs driver 531 or newer.
//!
//! Regenerate with `./generate.sh` after updating `include/nvEncodeAPI.h`.

// Header calls use `__stdcall`, which only matches `extern "C"` on 64-bit.
#[cfg(all(windows, not(target_pointer_width = "64")))]
compile_error!("ws-nvenc-sys supports 64-bit Windows only");

mod bindings;
mod consts;

pub use bindings::*;
pub use consts::*;

/// `NVENCSTATUS NvEncodeAPICreateInstance(NV_ENCODE_API_FUNCTION_LIST*)`
pub type NvEncodeAPICreateInstanceFn =
    unsafe extern "C" fn(function_list: *mut NV_ENCODE_API_FUNCTION_LIST) -> NVENCSTATUS;

/// `NVENCSTATUS NvEncodeAPIGetMaxSupportedVersion(uint32_t*)`, which reports
/// `(major << 4) | minor`.
pub type NvEncodeAPIGetMaxSupportedVersionFn =
    unsafe extern "C" fn(version: *mut u32) -> NVENCSTATUS;

/// The version [`NvEncodeAPIGetMaxSupportedVersionFn`] must report at least.
pub const REQUIRED_DRIVER_API_VERSION: u32 = (NVENCAPI_MAJOR_VERSION << 4) | NVENCAPI_MINOR_VERSION;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struct_versions_match_header_macros() {
        // NVENCAPI_STRUCT_VERSION(ver) = NVENCAPI_VERSION | ver << 16 | 7 << 28
        assert_eq!(NVENCAPI_VERSION, 12 | (1 << 24));
        assert_eq!(
            NV_ENC_PIC_PARAMS_VER,
            NVENCAPI_VERSION | (6 << 16) | (7 << 28) | (1 << 31)
        );
        assert_eq!(
            NV_ENCODE_API_FUNCTION_LIST_VER,
            NVENCAPI_VERSION | (2 << 16) | (7 << 28)
        );
        assert_eq!(NV_ENC_CODEC_H264_GUID.Data1, 0x6bc82762);
    }
}
