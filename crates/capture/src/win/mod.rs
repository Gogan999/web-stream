//! Windows capture/encode pipeline.

mod convert;
mod display;
mod duplication;
mod mode;
mod nvenc;
mod pipeline;

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
};
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter1, IDXGIDevice};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::core::Interface;
use ws_proto::DisplayInfo;

pub(crate) use mode::{restore_display_mode, set_display_mode};
pub(crate) use pipeline::start;

use crate::{CaptureError, Result};

pub(crate) fn list_displays() -> Result<Vec<DisplayInfo>> {
    init_process();
    Ok(display::enumerate()?.into_iter().map(|o| o.info).collect())
}

/// The hardware encoders on each adapter.
pub(crate) fn probe_encoders() -> Vec<crate::caps::EncoderInfo> {
    Vec::new()
}

/// Opts the process into per-monitor DPI awareness, once, before DXGI is used.
fn init_process() {
    use windows::Win32::System::Threading::{
        GetCurrentProcess, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
        PROCESS_POWER_THROTTLING_STATE, ProcessPowerThrottling, SetProcessInformation,
    };
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        // Opt out of EcoQoS and timer-resolution throttling.
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED
                | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
            StateMask: 0,
        };
        let _ = unsafe {
            SetProcessInformation(
                GetCurrentProcess(),
                ProcessPowerThrottling,
                std::ptr::from_ref(&state).cast(),
                std::mem::size_of_val(&state) as u32,
            )
        };
    });
}

/// Puts the calling thread in the MMCSS "Games" class for the life of the thread.
pub(crate) fn enter_mmcss() {
    use windows::Win32::System::Threading::AvSetMmThreadCharacteristicsW;
    let mut index = 0;
    if let Err(e) = unsafe { AvSetMmThreadCharacteristicsW(windows::core::w!("Games"), &mut index) }
    {
        tracing::debug!("MMCSS Games class refused: {e}");
    }
}

fn win_err(what: &'static str) -> impl Fn(windows::core::Error) -> CaptureError {
    move |e| CaptureError::Platform(format!("{what}: {e}"))
}

/// A D3D11 device on the adapter that owns the captured output; capture, conversion and NVENC all
/// share it.
pub(crate) struct Gpu {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
}

impl Gpu {
    pub(crate) fn new(adapter: &IDXGIAdapter1) -> Result<Self> {
        let mut device = None;
        let mut context = None;
        unsafe {
            D3D11CreateDevice(
                adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        }
        .map_err(win_err("D3D11CreateDevice"))?;
        let device: ID3D11Device =
            device.ok_or(CaptureError::Platform("no D3D11 device".into()))?;
        let context = context.ok_or(CaptureError::Platform("no D3D11 context".into()))?;
        if let Ok(mt) = context.cast::<ID3D11Multithread>() {
            let _ = unsafe { mt.SetMultithreadProtected(true) };
        }
        if let Ok(dxgi) = device.cast::<IDXGIDevice>() {
            let _ = unsafe { dxgi.SetGPUThreadPriority(7) };
        }
        Ok(Self { device, context })
    }
}
