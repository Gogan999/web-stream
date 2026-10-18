//! DXGI Desktop Duplication for one output.

use windows::Win32::Foundation::E_ACCESSDENIED;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_NOT_CURRENTLY_AVAILABLE, DXGI_ERROR_UNSUPPORTED,
    DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, IDXGIOutput1, IDXGIOutputDuplication,
    IDXGIResource,
};
use windows::core::Interface;

use super::{Gpu, win_err};
use crate::{CaptureError, Result};

pub(crate) enum Acquired {
    /// The desktop image changed. Valid until the next `acquire` call.
    Frame(ID3D11Texture2D),
    /// Timed out, or only the pointer moved.
    Nothing,
    /// The duplication is gone (mode change, fullscreen transition, secure
    /// desktop); create a new one.
    Lost,
}

pub(crate) struct Duplicator {
    dup: IDXGIOutputDuplication,
    holding: bool,
}

impl Duplicator {
    pub(crate) fn new(output: &IDXGIOutput1, gpu: &Gpu) -> Result<Self> {
        let dup = unsafe { output.DuplicateOutput(&gpu.device) }.map_err(|e| {
            let hint = match e.code() {
                E_ACCESSDENIED => " (secure desktop such as UAC or the lock screen is showing)",
                DXGI_ERROR_NOT_CURRENTLY_AVAILABLE => {
                    " (too many apps are duplicating this output)"
                }
                DXGI_ERROR_UNSUPPORTED => " (driver or adapter does not support duplication)",
                _ => "",
            };
            CaptureError::Platform(format!("DuplicateOutput: {e}{hint}"))
        })?;
        Ok(Self {
            dup,
            holding: false,
        })
    }

    /// Waits up to `timeout_ms` for a new desktop image. Releases the frame
    /// returned by the previous call first, so copy it out before calling
    /// again.
    pub(crate) fn acquire(&mut self, timeout_ms: u32) -> Result<Acquired> {
        self.release();
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        match unsafe {
            self.dup
                .AcquireNextFrame(timeout_ms, &mut info, &mut resource)
        } {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(Acquired::Nothing),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => return Ok(Acquired::Lost),
            Err(e) => return Err(win_err("AcquireNextFrame")(e)),
        }
        self.holding = true;
        // LastPresentTime == 0 means only the pointer changed.
        if info.LastPresentTime == 0 {
            return Ok(Acquired::Nothing);
        }
        let texture = resource
            .ok_or(CaptureError::Platform(
                "AcquireNextFrame returned no resource".into(),
            ))?
            .cast::<ID3D11Texture2D>()
            .map_err(win_err("desktop resource as texture"))?;
        Ok(Acquired::Frame(texture))
    }

    /// Hands the frame back to DWM. Doing this right after copying keeps
    /// DWM from stalling on us.
    pub(crate) fn release(&mut self) {
        if std::mem::take(&mut self.holding) {
            let _ = unsafe { self.dup.ReleaseFrame() };
        }
    }
}

impl Drop for Duplicator {
    fn drop(&mut self) {
        self.release();
    }
}
