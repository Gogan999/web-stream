//! NVENC on a D3D11 device.

use std::ffi::c_void;
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D};
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST, WaitForSingleObject,
};
use windows::core::Interface;
use ws_nvenc_sys as nv;

use super::Gpu;
use super::encode::nv12_texture;
use crate::{CaptureError, Result};

pub(crate) use crate::nvenc::DPB_FRAMES;

/// The encoder for the capture thread's D3D11 device.
pub(crate) type NvEncoder<M> = crate::nvenc::NvEncoder<D3d, M>;

/// The D3D11 device the converter renders on.
pub(crate) struct D3d {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
}

impl D3d {
    pub(crate) fn new(gpu: &Gpu) -> Self {
        Self {
            device: gpu.device.clone(),
            context: gpu.context.clone(),
        }
    }
}

impl crate::nvenc::Device for D3d {
    type Source = ID3D11Texture2D;
    type Buffer = ID3D11Texture2D;
    const DEVICE_TYPE: nv::NV_ENC_DEVICE_TYPE = nv::NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_DIRECTX;
    const RESOURCE_TYPE: nv::NV_ENC_INPUT_RESOURCE_TYPE =
        nv::NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
    const ASYNC: bool = true;

    fn handle(&self) -> *mut c_void {
        self.device.as_raw()
    }

    fn buffer(&self, width: u32, height: u32) -> Result<ID3D11Texture2D> {
        nv12_texture(&self.device, width, height)
    }

    fn resource(buffer: &ID3D11Texture2D) -> (*mut c_void, u32) {
        (buffer.as_raw(), 0)
    }

    fn copy(&self, buffer: &ID3D11Texture2D, src: &ID3D11Texture2D) -> Result<()> {
        unsafe {
            self.context.CopyResource(buffer, src);
            self.context.Flush();
        }
        Ok(())
    }

    fn output_thread_init(&self) -> Box<dyn FnOnce() + Send> {
        Box::new(|| {
            let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) };
            super::enter_mmcss();
        })
    }

    fn create_event() -> Result<*mut c_void> {
        let event = unsafe { CreateEventW(None, false, false, None) }
            .map_err(|e| CaptureError::Platform(format!("CreateEvent: {e}")))?;
        Ok(event.0)
    }

    fn close_event(event: *mut c_void) {
        unsafe {
            let _ = CloseHandle(HANDLE(event));
        }
    }

    fn wait_event(event: *mut c_void, timeout: Duration) -> bool {
        unsafe { WaitForSingleObject(HANDLE(event), timeout.as_millis() as u32) == WAIT_OBJECT_0 }
    }
}
