//! Changes a display's desktop resolution, temporarily (`CDS_FULLSCREEN`):
//! the change is never written to the registry, so restoring or a reboot
//! puts the user's own setting back (and Windows is expected to when the
//! host process ends, though that is not documented).

use windows::Win32::Graphics::Gdi::{
    CDS_FULLSCREEN, CDS_TYPE, ChangeDisplaySettingsExW, DEVMODEW, DISP_CHANGE,
    DISP_CHANGE_SUCCESSFUL, DM_BITSPERPEL, DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH,
    ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_MODE, EnumDisplaySettingsW,
};
use windows::core::HSTRING;
use ws_proto::DisplayId;

use super::{display, init_process};
use crate::modes::{Mode, pick_mode};
use crate::{CaptureError, ModeChange, Result};

pub(crate) fn set_display_mode(id: DisplayId, width: u32, height: u32) -> Result<ModeChange> {
    init_process();
    let name = HSTRING::from(display::find(id)?.gdi_name);
    let current = enum_mode(&name, ENUM_CURRENT_SETTINGS)
        .ok_or_else(|| CaptureError::Platform("can't read the display's current mode".into()))?;
    let modes: Vec<Mode> = (0..)
        .map_while(|i| enum_mode(&name, ENUM_DISPLAY_SETTINGS_MODE(i)))
        .map(mode)
        .collect();
    let want = pick_mode(&modes, width, height, current.dmDisplayFrequency).ok_or_else(|| {
        CaptureError::Platform(format!(
            "display {} has no resolution that fits in {width}x{height}",
            id.0
        ))
    })?;
    let change = ModeChange {
        width: want.width,
        height: want.height,
        changed: mode(current) != want,
    };
    if !change.changed {
        return Ok(change);
    }
    let dm = DEVMODEW {
        dmSize: size_of::<DEVMODEW>() as u16,
        dmFields: DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY | DM_BITSPERPEL,
        dmPelsWidth: want.width,
        dmPelsHeight: want.height,
        dmDisplayFrequency: want.refresh_hz,
        dmBitsPerPel: want.bits_per_pixel,
        ..Default::default()
    };
    let rc = unsafe { ChangeDisplaySettingsExW(&name, Some(&dm), None, CDS_FULLSCREEN, None) };
    check(rc, "change resolution")?;
    tracing::info!(
        display = id.0,
        width = want.width,
        height = want.height,
        hz = want.refresh_hz,
        "display resolution changed"
    );
    Ok(change)
}

pub(crate) fn restore_display_mode(id: DisplayId) -> Result<()> {
    init_process();
    let name = HSTRING::from(display::find(id)?.gdi_name);
    // No mode: go back to what the registry holds, the user's own setting.
    let rc = unsafe { ChangeDisplaySettingsExW(&name, None, None, CDS_TYPE(0), None) };
    check(rc, "restore resolution")?;
    tracing::info!(display = id.0, "display resolution restored");
    Ok(())
}

fn enum_mode(name: &HSTRING, which: ENUM_DISPLAY_SETTINGS_MODE) -> Option<DEVMODEW> {
    let mut dm = DEVMODEW {
        dmSize: size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    unsafe { EnumDisplaySettingsW(name, which, &mut dm) }
        .as_bool()
        .then_some(dm)
}

fn mode(dm: DEVMODEW) -> Mode {
    Mode {
        width: dm.dmPelsWidth,
        height: dm.dmPelsHeight,
        refresh_hz: dm.dmDisplayFrequency,
        bits_per_pixel: dm.dmBitsPerPel,
    }
}

fn check(rc: DISP_CHANGE, what: &str) -> Result<()> {
    if rc == DISP_CHANGE_SUCCESSFUL {
        Ok(())
    } else {
        Err(CaptureError::Platform(format!(
            "{what}: DISP_CHANGE {}",
            rc.0
        )))
    }
}
