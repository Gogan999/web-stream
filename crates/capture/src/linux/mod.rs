//! Linux capture/encode pipeline: KMS scanout capture, VA-API or NVENC encode.
#![allow(dead_code)]

mod display;
mod drm;
mod edid;
mod kms;
#[allow(dead_code)]
mod va;

use ws_proto::{DisplayId, DisplayInfo};

use crate::{CaptureError, ModeChange, Result};

pub(crate) fn list_displays() -> Result<Vec<DisplayInfo>> {
    Ok(display::enumerate()?.into_iter().map(|o| o.info).collect())
}

pub(crate) fn set_display_mode(display: DisplayId, width: u32, height: u32) -> Result<ModeChange> {
    let _ = (display, width, height);
    Err(CaptureError::Unsupported)
}

pub(crate) fn restore_display_mode(display: DisplayId) -> Result<()> {
    let _ = display;
    Err(CaptureError::Unsupported)
}
