//! Raw bindings to `nvEncodeAPI.h` from the NVIDIA Video Codec SDK 12.1.

#[cfg(all(windows, not(target_pointer_width = "64")))]
compile_error!("ws-nvenc-sys supports 64-bit Windows only");

mod bindings;
mod consts;

pub use bindings::*;
pub use consts::*;

/// `NVENCSTATUS NvEncodeAPICreateInstance(NV_ENCODE_API_FUNCTION_LIST*)`
pub type NvEncodeAPICreateInstanceFn =
    unsafe extern "C" fn(function_list: *mut NV_ENCODE_API_FUNCTION_LIST) -> NVENCSTATUS;

/// `NVENCSTATUS NvEncodeAPIGetMaxSupportedVersion(uint32_t*)`.
pub type NvEncodeAPIGetMaxSupportedVersionFn =
    unsafe extern "C" fn(version: *mut u32) -> NVENCSTATUS;

/// The version [`NvEncodeAPIGetMaxSupportedVersionFn`] must report at least.
pub const REQUIRED_DRIVER_API_VERSION: u32 = (NVENCAPI_MAJOR_VERSION << 4) | NVENCAPI_MINOR_VERSION;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struct_versions_match_header_macros() {
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
