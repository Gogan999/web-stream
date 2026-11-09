//! Windows capture/encode pipeline.

mod convert;
mod display;
mod duplication;
mod mode;
mod nvenc;
mod pipeline;

use windows::Win32::Foundation::{HANDLE, HMODULE};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
};
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter1, IDXGIDevice};
use windows::Win32::System::Threading::GetCurrentProcess;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::core::Interface;
use ws_proto::DisplayInfo;

pub(crate) use mode::{restore_display_mode, set_display_mode};
pub(crate) use pipeline::{Control, PipelineHandle};

use crate::{CaptureError, Result};

pub(crate) fn list_displays() -> Result<Vec<DisplayInfo>> {
    init_process();
    Ok(display::enumerate()?.into_iter().map(|o| o.info).collect())
}

/// Desktop Duplication reports scaled coordinates and can refuse to start
/// for DPI-unaware processes, so opt in once before touching DXGI.
fn init_process() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // Fails harmlessly if the manifest or an earlier call already set it.
        let _ =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        set_gpu_scheduling_class();
    });
}

// The GPU scheduler's process classes (D3DKMT_SCHEDULINGPRIORITYCLASS).
const KMT_ABOVE_NORMAL: i32 = 3;
const KMT_HIGH: i32 = 4;
const KMT_REALTIME: i32 = 5;

#[link(name = "gdi32")]
unsafe extern "system" {
    /// Returns an NTSTATUS: 0 or positive is success. Raising a process above
    /// "normal" takes the privilege to increase base priority (administrators,
    /// SYSTEM).
    fn D3DKMTSetProcessSchedulingPriorityClass(process: HANDLE, class: i32) -> i32;
}

/// Asks the GPU scheduler to run this process's work ahead of other
/// processes' (a game's) when the GPU is saturated: capture's copy and
/// conversion and the encoder's input queue behind a game's long draw calls
/// otherwise, and frames take tens of milliseconds instead of a few.
///
/// `WS_GPU_PRIORITY` picks it: `off` (the default for now: the scheduler's
/// own choice), `above`, `high`, `realtime`, or `auto` (the highest the
/// process is allowed). The outcome is logged either way.
fn set_gpu_scheduling_class() {
    let wanted: &[(&str, i32)] = match std::env::var("WS_GPU_PRIORITY")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "above" => &[("above-normal", KMT_ABOVE_NORMAL)],
        "high" => &[("high", KMT_HIGH)],
        "realtime" => &[("realtime", KMT_REALTIME)],
        "auto" => &[
            ("realtime", KMT_REALTIME),
            ("high", KMT_HIGH),
            ("above-normal", KMT_ABOVE_NORMAL),
        ],
        _ => {
            tracing::info!(
                "GPU scheduling class left at the system's choice (WS_GPU_PRIORITY=off)"
            );
            return;
        }
    };
    for &(name, class) in wanted {
        let status = unsafe { D3DKMTSetProcessSchedulingPriorityClass(GetCurrentProcess(), class) };
        if status >= 0 {
            tracing::info!("GPU scheduling class set to {name}");
            ws_diag::diag!("gpu_sched", "class={name} status=ok");
            return;
        }
        tracing::info!("GPU scheduling class {name} refused (NTSTATUS {status:#010x})");
        ws_diag::diag!("gpu_sched", "class={name} status={status:#010x}");
    }
}

fn win_err(what: &'static str) -> impl Fn(windows::core::Error) -> CaptureError {
    move |e| CaptureError::Platform(format!("{what}: {e}"))
}

/// A D3D11 device on the adapter that owns the captured output; capture,
/// conversion and NVENC all share it so frames never leave the GPU.
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
        // The encoder's output thread works alongside the capture thread.
        if let Ok(mt) = context.cast::<ID3D11Multithread>() {
            let _ = unsafe { mt.SetMultithreadProtected(true) };
        }
        // Keep our GPU work ahead of a game's when the GPU is saturated.
        // Best effort: higher priorities may need privileges we don't have.
        if let Ok(dxgi) = device.cast::<IDXGIDevice>() {
            // Above 0 this needs the privilege to raise base priority, so it
            // is refused for an ordinary user's tray app: say so once.
            static LOGGED: std::sync::Once = std::sync::Once::new();
            let result = unsafe { dxgi.SetGPUThreadPriority(7) };
            LOGGED.call_once(|| match &result {
                Ok(()) => tracing::info!("GPU thread priority 7 granted"),
                Err(e) => tracing::info!("GPU thread priority 7 refused: {e}"),
            });
        }
        Ok(Self { device, context })
    }
}
