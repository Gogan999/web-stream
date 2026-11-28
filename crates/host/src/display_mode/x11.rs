//! X11 sessions: RandR 1.3 CRTC and screen size changes.

use x11rb::connection::Connection as _;
use x11rb::protocol::randr::{
    self, ConnectionExt as _, GetCrtcInfoReply, ModeFlag, ModeInfo, Rotation, SetConfig,
};
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _};
use x11rb::rust_connection::RustConnection;

use super::helper::Backend;
use super::plan::{Head, HeadMode, Target};

/// An output with a CRTC, as last listed.
struct Listed {
    crtc: randr::Crtc,
    info: GetCrtcInfoReply,
}

pub struct RandR {
    conn: RustConnection,
    root: u32,
    config_timestamp: u32,
    listed: Vec<Listed>,
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// A mode's refresh rate in millihertz; 0 when unknown.
fn refresh_mhz(m: &ModeInfo) -> u32 {
    let mut clock = u64::from(m.dot_clock) * 1000;
    let mut total = u64::from(m.htotal) * u64::from(m.vtotal);
    if m.mode_flags.contains(ModeFlag::DOUBLE_SCAN) {
        total *= 2;
    }
    if m.mode_flags.contains(ModeFlag::INTERLACE) {
        clock *= 2;
    }
    if total == 0 {
        return 0;
    }
    ((clock + total / 2) / total) as u32
}

/// The screen size that holds every `(x, y, width, height)`, within `min` and `max`.
fn screen_size(
    rects: &[(i32, i32, u32, u32)],
    min: (u16, u16),
    max: (u16, u16),
) -> Result<(u16, u16), String> {
    let w = rects.iter().map(|r| r.0 + r.2 as i32).max().unwrap_or(0);
    let h = rects.iter().map(|r| r.1 + r.3 as i32).max().unwrap_or(0);
    if w > i32::from(max.0) || h > i32::from(max.1) {
        return Err(format!(
            "the desktop would be {w}x{h}, over the X server's {}x{}",
            max.0, max.1
        ));
    }
    Ok(((w.max(0) as u16).max(min.0), (h.max(0) as u16).max(min.1)))
}

/// Millimetres for `px` pixels at the density of `cur_px` pixels over `cur_mm`, else 96 dpi.
fn millimetres(px: u16, cur_px: u16, cur_mm: u16) -> u32 {
    if cur_px > 0 && cur_mm > 0 {
        (u32::from(px) * u32::from(cur_mm) + u32::from(cur_px) / 2) / u32::from(cur_px)
    } else {
        (u32::from(px) * 254 + 480) / 960
    }
}

impl RandR {
    pub fn connect() -> Result<Self, String> {
        let (conn, screen) = x11rb::connect(None).map_err(err)?;
        let root = conn.setup().roots[screen].root;
        let v = conn
            .randr_query_version(1, 3)
            .map_err(err)?
            .reply()
            .map_err(|_| "the X server has no RandR".to_string())?;
        if (v.major_version, v.minor_version) < (1, 3) {
            return Err(format!(
                "the X server has RandR {}.{}, 1.3 is needed",
                v.major_version, v.minor_version
            ));
        }
        Ok(Self {
            conn,
            root,
            config_timestamp: 0,
            listed: Vec::new(),
        })
    }

    fn edid(&self, output: randr::Output, atom: Option<u32>) -> Option<Vec<u8>> {
        let reply = self
            .conn
            .randr_get_output_property(output, atom?, AtomEnum::ANY, 0, 256, false, false)
            .ok()?
            .reply()
            .ok()?;
        (!reply.data.is_empty()).then_some(reply.data)
    }

    fn set_crtc(
        &self,
        crtc: randr::Crtc,
        x: i16,
        y: i16,
        mode: u32,
        rotation: Rotation,
        outputs: &[randr::Output],
    ) -> Result<(), String> {
        let reply = self
            .conn
            .randr_set_crtc_config(
                crtc,
                x11rb::CURRENT_TIME,
                self.config_timestamp,
                x,
                y,
                mode,
                rotation,
                outputs,
            )
            .map_err(err)?
            .reply()
            .map_err(err)?;
        if reply.status != SetConfig::SUCCESS {
            return Err(format!(
                "RandR refused the CRTC setting ({:?})",
                reply.status
            ));
        }
        Ok(())
    }
}

impl Backend for RandR {
    fn heads(&mut self) -> Result<Vec<Head>, String> {
        let mut res = self
            .conn
            .randr_get_screen_resources_current(self.root)
            .map_err(err)?
            .reply()
            .map_err(err)?;
        if res.outputs.is_empty() {
            let probed = self
                .conn
                .randr_get_screen_resources(self.root)
                .map_err(err)?
                .reply()
                .map_err(err)?;
            res.config_timestamp = probed.config_timestamp;
            res.outputs = probed.outputs;
            res.modes = probed.modes;
        }
        self.config_timestamp = res.config_timestamp;
        let edid_atom = self
            .conn
            .intern_atom(true, b"EDID")
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.atom)
            .filter(|&a| a != 0);
        let mode = |id: u32| -> Option<HeadMode> {
            let m = res.modes.iter().find(|m| m.id == id)?;
            if m.mode_flags.contains(ModeFlag::INTERLACE) {
                return None;
            }
            Some(HeadMode {
                width: u32::from(m.width),
                height: u32::from(m.height),
                refresh_mhz: refresh_mhz(m),
                id: id.to_string(),
                scales: Vec::new(),
            })
        };
        self.listed.clear();
        let mut heads = Vec::new();
        for &output in &res.outputs {
            let Ok(info) = self
                .conn
                .randr_get_output_info(output, res.config_timestamp)
                .map_err(err)?
                .reply()
            else {
                continue;
            };
            if info.crtc == 0 {
                continue;
            }
            let crtc = self
                .conn
                .randr_get_crtc_info(info.crtc, res.config_timestamp)
                .map_err(err)?
                .reply()
                .map_err(err)?;
            let Some(current) = mode(crtc.mode) else {
                continue;
            };
            heads.push(Head {
                name: String::from_utf8_lossy(&info.name).into_owned(),
                edid: self.edid(output, edid_atom),
                x: i32::from(crtc.x),
                y: i32::from(crtc.y),
                scale: 1.0,
                rotated: crtc
                    .rotation
                    .intersects(Rotation::ROTATE90 | Rotation::ROTATE270),
                logical: false,
                modes: info.modes.iter().filter_map(|&id| mode(id)).collect(),
                current,
                custom: false,
            });
            self.listed.push(Listed {
                crtc: info.crtc,
                info: crtc,
            });
        }
        Ok(heads)
    }

    fn apply(&mut self, heads: &[Head], targets: &[Target]) -> Result<(), String> {
        struct Change<'a> {
            listed: &'a Listed,
            x: i16,
            y: i16,
            mode: u32,
            width: u32,
            height: u32,
        }
        let mut crtcs: Vec<(Change, &Target)> = Vec::new();
        for (i, l) in self.listed.iter().enumerate() {
            let t = &targets[i];
            if let Some((_, other)) = crtcs.iter().find(|(c, _)| c.listed.crtc == l.crtc) {
                if *other != t {
                    return Err(format!("{} is mirrored on another output", heads[i].name));
                }
                continue;
            }
            let (width, height) = if heads[i].rotated {
                (t.mode.height, t.mode.width)
            } else {
                (t.mode.width, t.mode.height)
            };
            crtcs.push((
                Change {
                    listed: l,
                    x: i16::try_from(t.x).map_err(err)?,
                    y: i16::try_from(t.y).map_err(err)?,
                    mode: t
                        .mode
                        .id
                        .parse()
                        .map_err(|_| "unknown RandR mode".to_string())?,
                    width,
                    height,
                },
                t,
            ));
        }
        let rects: Vec<_> = crtcs
            .iter()
            .map(|(c, _)| (i32::from(c.x), i32::from(c.y), c.width, c.height))
            .collect();
        let range = self
            .conn
            .randr_get_screen_size_range(self.root)
            .map_err(err)?
            .reply()
            .map_err(err)?;
        let (sw, sh) = screen_size(
            &rects,
            (range.min_width, range.min_height),
            (range.max_width, range.max_height),
        )?;
        let geometry = self
            .conn
            .get_geometry(self.root)
            .map_err(err)?
            .reply()
            .map_err(err)?;
        let (cur_w, cur_h) = (geometry.width, geometry.height);
        let setup = self
            .conn
            .setup()
            .roots
            .iter()
            .find(|s| s.root == self.root)
            .ok_or("no X screen")?;
        let density = (
            (setup.width_in_pixels, setup.width_in_millimeters),
            (setup.height_in_pixels, setup.height_in_millimeters),
        );
        let fits = |x: i16, y: i16, w: u32, h: u32, sw: u16, sh: u16| {
            i32::from(x) + w as i32 <= i32::from(sw) && i32::from(y) + h as i32 <= i32::from(sh)
        };
        let changes: Vec<&Change> = crtcs
            .iter()
            .map(|(c, _)| c)
            .filter(|c| {
                c.mode != c.listed.info.mode || (c.x, c.y) != (c.listed.info.x, c.listed.info.y)
            })
            .collect();
        self.conn.grab_server().map_err(err)?;
        let result = (|| -> Result<(), String> {
            let (now, later): (Vec<&&Change>, Vec<&&Change>) = changes
                .iter()
                .partition(|c| fits(c.x, c.y, c.width, c.height, cur_w, cur_h));
            for c in &now {
                let i = &c.listed.info;
                self.set_crtc(c.listed.crtc, c.x, c.y, c.mode, i.rotation, &i.outputs)?;
            }
            for c in &later {
                let i = &c.listed.info;
                if !fits(i.x, i.y, u32::from(i.width), u32::from(i.height), sw, sh) {
                    self.set_crtc(c.listed.crtc, 0, 0, 0, Rotation::ROTATE0, &[])?;
                }
            }
            if (sw, sh) != (cur_w, cur_h) {
                self.conn
                    .randr_set_screen_size(
                        self.root,
                        sw,
                        sh,
                        millimetres(sw, density.0.0, density.0.1),
                        millimetres(sh, density.1.0, density.1.1),
                    )
                    .map_err(err)?
                    .check()
                    .map_err(err)?;
            }
            for c in &later {
                let i = &c.listed.info;
                self.set_crtc(c.listed.crtc, c.x, c.y, c.mode, i.rotation, &i.outputs)?;
            }
            Ok(())
        })();
        let _ = self.conn.ungrab_server();
        let _ = self.conn.flush();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(dot_clock: u32, htotal: u16, vtotal: u16, mode_flags: ModeFlag) -> ModeInfo {
        ModeInfo {
            id: 1,
            width: 1920,
            height: 1080,
            dot_clock,
            hsync_start: 0,
            hsync_end: 0,
            htotal,
            hskew: 0,
            vsync_start: 0,
            vsync_end: 0,
            vtotal,
            name_len: 0,
            mode_flags,
        }
    }

    #[test]
    fn refresh_from_timings() {
        assert_eq!(
            refresh_mhz(&mode(148_500_000, 2200, 1125, ModeFlag::from(0u32))),
            60000
        );
        assert_eq!(
            refresh_mhz(&mode(148_351_648, 2200, 1125, ModeFlag::from(0u32))),
            59940
        );
        assert_eq!(
            refresh_mhz(&mode(74_250_000, 2200, 1125, ModeFlag::INTERLACE)),
            60000
        );
        assert_eq!(refresh_mhz(&mode(1, 0, 0, ModeFlag::from(0u32))), 0);
    }

    #[test]
    fn screen_holds_every_crtc() {
        let rects = [(0, 0, 1920, 1080), (1920, 0, 1280, 1024)];
        assert_eq!(
            screen_size(&rects, (8, 8), (16384, 16384)),
            Ok((3200, 1080))
        );
        assert_eq!(
            screen_size(&[(0, 0, 640, 480)], (800, 600), (8192, 8192)),
            Ok((800, 600))
        );
        assert!(screen_size(&rects, (8, 8), (2048, 2048)).is_err());
    }

    #[test]
    fn millimetres_keep_the_density() {
        assert_eq!(millimetres(1920, 3840, 600), 300);
        assert_eq!(millimetres(960, 0, 0), 254);
    }
}
