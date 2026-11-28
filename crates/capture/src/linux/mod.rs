//! Linux capture/encode pipeline: KMS scanout capture, VA-API or NVENC encode.

mod cuda;
pub(crate) mod display;
mod drm;
mod edid;
mod egl;
mod kms;
mod nvenc;
mod pipeline;
mod va;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use ws_proto::{DisplayId, DisplayInfo};

pub(crate) use pipeline::start;

use crate::caps::EncoderInfo;
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

/// The hardware encoders on every GPU.
pub(crate) fn probe_encoders() -> Vec<EncoderInfo> {
    drm::cards()
        .iter()
        .flat_map(|card| card_encoders(card))
        .collect()
}

/// The hardware encoders on the GPU that drives `output`.
pub(crate) fn device_encoders(output: &display::Output) -> Vec<EncoderInfo> {
    card_encoders(&output.card)
}

/// The encoders on the GPU behind `card`, probed once per process.
fn card_encoders(card: &Path) -> Vec<EncoderInfo> {
    static PROBED: OnceLock<Mutex<HashMap<PathBuf, Vec<EncoderInfo>>>> = OnceLock::new();
    let cache = PROBED.get_or_init(Mutex::default);
    if let Some(found) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(card) {
        return found.clone();
    }
    let found = probe_card(card);
    cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(card.to_owned(), found.clone());
    found
}

fn probe_card(card: &Path) -> Vec<EncoderInfo> {
    let driver = drm::Card::open(card)
        .and_then(|c| c.driver())
        .unwrap_or_default();
    let name = card
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let adapter = format!("{driver} ({name})");
    let mut out = Vec::new();
    if drm::vendor(card) == 0x10de || driver.starts_with("nvidia") {
        out.extend(nvenc::probe(card));
    }
    let node = drm::render_node(card).unwrap_or_else(|| card.to_owned());
    out.extend(va::probe(&node, &adapter));
    if out.is_empty() {
        tracing::info!(card = %card.display(), %driver, "no hardware encoder");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probing_without_gpu_libraries_is_clean() {
        let encoders = probe_encoders();
        for e in &encoders {
            assert!(!e.codecs.is_empty());
        }
        if drm::cards().is_empty() {
            assert!(encoders.is_empty());
        }
    }
}
