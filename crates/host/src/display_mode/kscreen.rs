//! KDE Plasma on Wayland: `kscreen-doctor`.

use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

use super::helper::Backend;
use super::plan::{Head, HeadMode, Target};

/// KScreen rotations that turn the output on its side (left, right).
const SIDEWAYS: [u64; 2] = [2, 8];

pub struct KScreen {
    exe: PathBuf,
}

impl KScreen {
    /// `None` when `kscreen-doctor` isn't installed.
    pub fn find() -> Option<Self> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|d| d.join("kscreen-doctor"))
            .find(|p| p.is_file())
            .map(|exe| Self { exe })
    }

    fn run(&self, args: &[String]) -> Result<Vec<u8>, String> {
        let out = Command::new(&self.exe)
            .args(args)
            .output()
            .map_err(|e| format!("kscreen-doctor: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "kscreen-doctor: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(out.stdout)
    }
}

/// The enabled outputs in `kscreen-doctor -j` output.
fn parse(json: &[u8]) -> Result<Vec<Head>, String> {
    let doc: Value = serde_json::from_slice(json).map_err(|e| format!("kscreen-doctor -j: {e}"))?;
    let outputs = doc["outputs"]
        .as_array()
        .ok_or("kscreen-doctor -j: no outputs")?;
    let mut heads = Vec::new();
    for o in outputs {
        if o["enabled"].as_bool() != Some(true) || o["connected"].as_bool() == Some(false) {
            continue;
        }
        let id_of = |v: &Value| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let modes: Vec<HeadMode> = o["modes"]
            .as_array()
            .map(|m| m.as_slice())
            .unwrap_or_default()
            .iter()
            .filter_map(|m| {
                Some(HeadMode {
                    width: u32::try_from(m["size"]["width"].as_u64()?).ok()?,
                    height: u32::try_from(m["size"]["height"].as_u64()?).ok()?,
                    refresh_mhz: (m["refreshRate"].as_f64()? * 1000.0).round() as u32,
                    id: id_of(&m["id"]),
                    scales: Vec::new(),
                })
            })
            .collect();
        let current_id = id_of(&o["currentModeId"]);
        let Some(current) = modes.iter().find(|m| m.id == current_id).cloned() else {
            continue;
        };
        heads.push(Head {
            name: o["name"].as_str().unwrap_or_default().to_string(),
            edid: None,
            x: o["pos"]["x"].as_i64().unwrap_or(0) as i32,
            y: o["pos"]["y"].as_i64().unwrap_or(0) as i32,
            scale: o["scale"].as_f64().unwrap_or(1.0),
            rotated: SIDEWAYS.contains(&o["rotation"].as_u64().unwrap_or(1)),
            logical: true,
            modes,
            current,
            custom: false,
        });
    }
    Ok(heads)
}

/// `kscreen-doctor` arguments that set every head that changes.
fn arguments(heads: &[Head], targets: &[Target]) -> Vec<String> {
    let mut args = Vec::new();
    for (h, t) in heads.iter().zip(targets) {
        if t.mode.id != h.current.id {
            args.push(format!("output.{}.mode.{}", h.name, t.mode.id));
        }
        if (t.x, t.y) != (h.x, h.y) {
            args.push(format!("output.{}.position.{},{}", h.name, t.x, t.y));
        }
        if (t.scale - h.scale).abs() > 1e-6 {
            args.push(format!("output.{}.scale.{}", h.name, t.scale));
        }
    }
    args
}

impl Backend for KScreen {
    fn heads(&mut self) -> Result<Vec<Head>, String> {
        parse(&self.run(&["-j".to_string()])?)
    }

    fn apply(&mut self, heads: &[Head], targets: &[Target]) -> Result<(), String> {
        let args = arguments(heads, targets);
        if args.is_empty() {
            return Ok(());
        }
        self.run(&args).map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::super::plan;
    use super::*;

    const SAMPLE: &str = r#"{
      "outputs": [
        {"id": 1, "name": "eDP-1", "enabled": true, "connected": true,
         "pos": {"x": 0, "y": 0}, "scale": 1.5, "rotation": 1, "currentModeId": "1",
         "modes": [
           {"id": "1", "name": "2880x1800@120", "refreshRate": 120.0, "size": {"width": 2880, "height": 1800}},
           {"id": "2", "name": "1920x1200@60", "refreshRate": 59.95, "size": {"width": 1920, "height": 1200}},
           {"id": "3", "name": "1920x1200@120", "refreshRate": 119.9, "size": {"width": 1920, "height": 1200}}
         ]},
        {"id": 2, "name": "HDMI-A-1", "enabled": true, "connected": true,
         "pos": {"x": 1920, "y": 0}, "scale": 1, "rotation": 8, "currentModeId": "7",
         "modes": [
           {"id": "7", "name": "1920x1080@60", "refreshRate": 60.0, "size": {"width": 1920, "height": 1080}}
         ]},
        {"id": 3, "name": "DP-1", "enabled": false, "connected": true,
         "pos": {"x": 0, "y": 0}, "scale": 1, "rotation": 1, "currentModeId": "",
         "modes": []}
      ]
    }"#;

    #[test]
    fn reads_enabled_outputs() {
        let heads = parse(SAMPLE.as_bytes()).unwrap();
        assert_eq!(heads.len(), 2);
        assert_eq!(heads[0].current.id, "1");
        assert_eq!(heads[0].modes[1].refresh_mhz, 59950);
        assert_eq!(heads[0].scale, 1.5);
        assert!(!heads[0].rotated && heads[1].rotated);
        assert_eq!(heads[1].x, 1920);
    }

    #[test]
    fn sets_mode_and_moves_neighbours() {
        let heads = parse(SAMPLE.as_bytes()).unwrap();
        let mode = plan::choose(&heads[0], 1920, 1200).unwrap();
        assert_eq!(mode.id, "3");
        let targets = plan::plan(&heads, 0, mode, None, None);
        assert_eq!(
            arguments(&heads, &targets),
            vec!["output.eDP-1.mode.3", "output.HDMI-A-1.position.1280,0"]
        );
        let same = plan::plan(&heads, 0, heads[0].current.clone(), None, None);
        assert!(arguments(&heads, &same).is_empty());
    }
}
