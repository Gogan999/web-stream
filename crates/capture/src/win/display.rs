//! Enumerates capturable displays: DXGI (adapter, output) pairs attached to
//! the desktop, named with the monitor's friendly name from the display
//! configuration API.

use std::collections::HashMap;

use windows::Win32::Devices::Display::{
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
    DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_TARGET_DEVICE_NAME, DisplayConfigGetDeviceInfo,
    GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig,
};
use windows::Win32::Foundation::{ERROR_SUCCESS, LUID};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_UNSPECIFIED,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_ERROR_NOT_FOUND, IDXGIAdapter1,
    IDXGIFactory1, IDXGIOutput1,
};
use windows::core::Interface;
use ws_proto::{DisplayId, DisplayInfo};

use super::win_err;
use crate::Result;

/// A capturable display and the DXGI objects needed to duplicate it.
pub(crate) struct Output {
    pub info: DisplayInfo,
    pub adapter: IDXGIAdapter1,
    pub adapter_luid: LUID,
    pub adapter_vendor: u32,
    pub output: IDXGIOutput1,
}

pub(crate) fn enumerate() -> Result<Vec<Output>> {
    let factory: IDXGIFactory1 =
        unsafe { CreateDXGIFactory1() }.map_err(win_err("CreateDXGIFactory1"))?;
    let config = display_config();
    let mut outputs = Vec::new();
    let mut next_fallback_id = 1000;

    for ai in 0.. {
        let adapter = match unsafe { factory.EnumAdapters1(ai) } {
            Ok(a) => a,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => return Err(win_err("EnumAdapters1")(e)),
        };
        let adesc = unsafe { adapter.GetDesc1() }.map_err(win_err("GetDesc1"))?;
        if adesc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
            continue;
        }
        let adapter_name = wide(&adesc.Description);

        for oi in 0.. {
            let output = match unsafe { adapter.EnumOutputs(oi) } {
                Ok(o) => o,
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(e) => return Err(win_err("EnumOutputs")(e)),
            };
            let odesc = unsafe { output.GetDesc() }.map_err(win_err("IDXGIOutput::GetDesc"))?;
            if !odesc.AttachedToDesktop.as_bool() {
                continue;
            }
            let Ok(output) = output.cast::<IDXGIOutput1>() else {
                continue;
            };
            let gdi_name = wide(&odesc.DeviceName);
            let r = odesc.DesktopCoordinates;
            let id = parse_display_number(&gdi_name).unwrap_or_else(|| {
                next_fallback_id += 1;
                next_fallback_id
            });
            let (monitor, refresh_mhz) = config
                .get(&gdi_name)
                .map(|c| (c.monitor_name.clone(), c.refresh_mhz))
                .unwrap_or_default();
            let monitor = if monitor.is_empty() {
                gdi_name.clone()
            } else {
                monitor
            };
            if odesc.Rotation != DXGI_MODE_ROTATION_IDENTITY
                && odesc.Rotation != DXGI_MODE_ROTATION_UNSPECIFIED
            {
                tracing::warn!(display = %gdi_name, "rotated displays are captured unrotated for now");
            }

            outputs.push(Output {
                info: DisplayInfo {
                    id: DisplayId(id),
                    name: format!("{monitor} ({adapter_name})"),
                    x: r.left,
                    y: r.top,
                    width: (r.right - r.left).unsigned_abs(),
                    height: (r.bottom - r.top).unsigned_abs(),
                    refresh_mhz,
                    primary: r.left == 0 && r.top == 0,
                },
                adapter: adapter.clone(),
                adapter_luid: adesc.AdapterLuid,
                adapter_vendor: adesc.VendorId,
                output,
            });
        }
    }
    outputs.sort_by_key(|o| o.info.id);
    Ok(outputs)
}

pub(crate) fn find(id: DisplayId) -> Result<Output> {
    enumerate()?
        .into_iter()
        .find(|o| o.info.id == id)
        .ok_or(crate::CaptureError::DisplayNotFound(id))
}

/// `\\.\DISPLAY3` -> 3. Stable for the session, unlike enumeration order.
fn parse_display_number(gdi_name: &str) -> Option<u32> {
    gdi_name.strip_prefix(r"\\.\DISPLAY")?.parse().ok()
}

fn wide(s: &[u16]) -> String {
    let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..end])
}

#[derive(Default)]
struct PathConfig {
    monitor_name: String,
    refresh_mhz: u32,
}

/// GDI device name (`\\.\DISPLAY1`) -> monitor friendly name and exact
/// refresh rate. Best effort: an empty map just means generic names.
fn display_config() -> HashMap<String, PathConfig> {
    let mut map = HashMap::new();
    let (mut np, mut nm) = (0u32, 0u32);
    if unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm) }
        != ERROR_SUCCESS
    {
        return map;
    }
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
    let rc = unsafe {
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut np,
            paths.as_mut_ptr(),
            &mut nm,
            modes.as_mut_ptr(),
            None,
        )
    };
    if rc != ERROR_SUCCESS {
        return map;
    }
    paths.truncate(np as usize);

    for path in &paths {
        let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                size: size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                adapterId: path.sourceInfo.adapterId,
                id: path.sourceInfo.id,
            },
            ..Default::default()
        };
        if unsafe { DisplayConfigGetDeviceInfo(&mut source.header) } != 0 {
            continue;
        }
        let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
                size: size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
                adapterId: path.targetInfo.adapterId,
                id: path.targetInfo.id,
            },
            ..Default::default()
        };
        let monitor_name = if unsafe { DisplayConfigGetDeviceInfo(&mut target.header) } == 0 {
            wide(&target.monitorFriendlyDeviceName)
        } else {
            String::new()
        };
        let rate = path.targetInfo.refreshRate;
        let refresh_mhz = if rate.Denominator != 0 {
            (u64::from(rate.Numerator) * 1000 / u64::from(rate.Denominator)) as u32
        } else {
            0
        };
        map.insert(
            wide(&source.viewGdiDeviceName),
            PathConfig {
                monitor_name,
                refresh_mhz,
            },
        );
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_numbers() {
        assert_eq!(parse_display_number(r"\\.\DISPLAY12"), Some(12));
        assert_eq!(parse_display_number("other"), None);
    }
}
