//! Lit KMS connectors as capturable displays.

use std::path::PathBuf;

use ws_proto::{DisplayId, DisplayInfo};

use super::drm::{self, Card};
use super::edid;
use crate::{CaptureError, Result};

/// A display and where its picture comes from.
#[derive(Debug, Clone)]
pub(crate) struct Output {
    pub info: DisplayInfo,
    pub card: PathBuf,
    /// The render node of the same GPU, for the encoder.
    pub render: Option<PathBuf>,
    /// The kernel driver, e.g. `i915`.
    pub driver: String,
    /// PCI vendor id, 0 if unknown.
    pub vendor: u32,
    pub crtc_id: u32,
    /// The CRTC's index on its card.
    pub pipe: u32,
    pub connector_id: u32,
    pub internal: bool,
}

pub(crate) fn enumerate() -> Result<Vec<Output>> {
    let mut outputs = Vec::new();
    let mut errors = Vec::new();
    for path in drm::cards() {
        match card_outputs(&path) {
            Ok(mut o) => outputs.append(&mut o),
            Err(e) => errors.push(format!("{}: {e}", path.display())),
        }
    }
    if outputs.is_empty() && !errors.is_empty() {
        return Err(CaptureError::Platform(format!(
            "no DRM device could be read ({})",
            errors.join("; ")
        )));
    }
    outputs.sort_by_key(|o| !o.internal);
    let mut rects: Vec<Rect> = outputs
        .iter()
        .map(|o| Rect {
            x: o.info.x,
            y: o.info.y,
            width: o.info.width,
            height: o.info.height,
        })
        .collect();
    arrange(&mut rects);
    let mut seen = Vec::new();
    for (o, r) in outputs.iter_mut().zip(&rects) {
        o.info.x = r.x;
        o.info.y = r.y;
        while seen.contains(&o.info.id) {
            o.info.id = DisplayId(o.info.id.0.wrapping_add(1).max(1));
        }
        seen.push(o.info.id);
    }
    if let Some(first) = outputs.iter().position(|o| o.info.x == 0 && o.info.y == 0) {
        outputs[first].info.primary = true;
    } else if let Some(o) = outputs.first_mut() {
        o.info.primary = true;
    }
    Ok(outputs)
}

pub(crate) fn find(id: DisplayId) -> Result<Output> {
    enumerate()?
        .into_iter()
        .find(|o| o.info.id == id)
        .ok_or(CaptureError::DisplayNotFound(id))
}

fn card_outputs(path: &std::path::Path) -> std::io::Result<Vec<Output>> {
    let card = Card::open(path)?;
    let driver = card.driver().unwrap_or_default();
    let res = card.resources()?;
    let key = drm::device_key(path);
    let render = drm::render_node(path);
    let vendor = drm::vendor(path);
    let mut out = Vec::new();
    for &connector_id in &res.connectors {
        let Ok(c) = card.connector(connector_id) else {
            continue;
        };
        if !c.connected || c.kind == drm::CONNECTOR_WRITEBACK || c.encoder_id == 0 {
            continue;
        }
        let Ok(crtc_id) = card.encoder_crtc(c.encoder_id) else {
            continue;
        };
        let Some(pipe) = res.crtcs.iter().position(|&id| id == crtc_id) else {
            continue;
        };
        let Ok(crtc) = card.crtc(crtc_id) else {
            continue;
        };
        let Some(mode) = crtc.mode else {
            continue;
        };
        let connector = c.name();
        let monitor = card
            .find_property(&c.props, "EDID")
            .filter(|&blob| blob != 0)
            .and_then(|blob| card.blob(blob as u32).ok())
            .and_then(|e| edid::monitor_name(&e));
        let name = match monitor {
            Some(m) => format!("{m} ({connector})"),
            None => connector.clone(),
        };
        out.push(Output {
            info: DisplayInfo {
                id: DisplayId(stable_id(&key, &connector)),
                name,
                x: i32::try_from(crtc.x).unwrap_or(0),
                y: i32::try_from(crtc.y).unwrap_or(0),
                width: u32::from(mode.hdisplay),
                height: u32::from(mode.vdisplay),
                refresh_mhz: mode.refresh_mhz(),
                primary: false,
            },
            card: path.to_owned(),
            render: render.clone(),
            driver: driver.clone(),
            vendor,
            crtc_id,
            pipe: pipe as u32,
            connector_id,
            internal: matches!(
                c.kind,
                drm::CONNECTOR_EDP | drm::CONNECTOR_LVDS | drm::CONNECTOR_DSI
            ),
        });
    }
    Ok(out)
}

/// An id from the GPU's device path and the connector's name, the same across boots.
fn stable_id(device: &str, connector: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in device.bytes().chain([b'/']).chain(connector.bytes()) {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    (h & 0x7fff_ffff).max(1)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rect {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

impl Rect {
    fn overlaps(&self, o: &Rect) -> bool {
        let (r1, b1) = (self.x + self.width as i32, self.y + self.height as i32);
        let (r2, b2) = (o.x + o.width as i32, o.y + o.height as i32);
        self.x < r2 && o.x < r1 && self.y < b2 && o.y < b1
    }
}

/// Keeps the CRTCs' own positions when they tile a shared desktop, else places the displays left
/// to right in order.
fn arrange(rects: &mut [Rect]) {
    let overlapping = rects
        .iter()
        .enumerate()
        .any(|(i, a)| rects[i + 1..].iter().any(|b| a.overlaps(b)));
    if !overlapping {
        return;
    }
    let mut x = 0i32;
    for r in rects {
        r.x = x;
        r.y = 0;
        x = x.saturating_add(r.width as i32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: i32, y: i32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn separate_framebuffers_are_laid_out_side_by_side() {
        let mut v = [r(0, 0, 2560, 1440), r(0, 0, 1920, 1080), r(0, 0, 1280, 720)];
        arrange(&mut v);
        assert_eq!(
            v,
            [
                r(0, 0, 2560, 1440),
                r(2560, 0, 1920, 1080),
                r(4480, 0, 1280, 720)
            ]
        );
    }

    #[test]
    fn a_shared_desktop_keeps_its_layout() {
        let mut v = [r(1920, 0, 1920, 1080), r(0, 120, 1920, 1080)];
        arrange(&mut v);
        assert_eq!(v, [r(1920, 0, 1920, 1080), r(0, 120, 1920, 1080)]);
    }

    #[test]
    fn ids_are_stable_and_distinct() {
        let a = stable_id("/sys/devices/pci0000:00/0000:00:02.0", "HDMI-A-1");
        assert_eq!(
            a,
            stable_id("/sys/devices/pci0000:00/0000:00:02.0", "HDMI-A-1")
        );
        assert_ne!(
            a,
            stable_id("/sys/devices/pci0000:00/0000:00:02.0", "HDMI-A-2")
        );
        assert_ne!(
            a,
            stable_id("/sys/devices/pci0000:00/0000:01:00.0", "HDMI-A-1")
        );
        assert!(a > 0 && a <= 0x7fff_ffff);
    }
}
