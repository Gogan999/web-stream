//! What the settings window edits, saved as JSON in the data dir. The host
//! is started from the equivalent `ws-host` command line, so the GUI and the
//! console accept and check exactly the same things.

use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use ws_host::options::ServeOptions;

pub const FILE_NAME: &str = "gui-settings.json";

pub const CODECS: &[&str] = &["h264", "h265", "av1"];
pub const QUALITIES: &[&str] = &["speed", "balanced", "clarity"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub port: u16,
    pub ice_tcp: bool,
    /// Public addresses that forward to this PC, separated by spaces or commas.
    pub public_ips: String,
    /// Empty: the web client built into the host.
    pub web_root: String,
    /// `None`: the primary display.
    pub display: Option<u32>,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub kbps: u32,
    pub quality: String,
    pub game_audio: bool,
    /// Game audio Opus bitrate (kb/s) and frame length (10 or 20 ms). A
    /// browser can override both from its own settings.
    pub audio_kbps: u32,
    pub audio_frame_ms: u32,
    pub mic: bool,
    pub mic_device: String,
    pub allow_resolution_change: bool,
    /// Let the browser send files to this PC.
    pub allow_upload: bool,
    /// Where they go; empty: `Downloads\web-stream`.
    pub upload_dir: String,
    /// More `ws-host` options, for what the window has no field for.
    pub extra_args: String,
    /// Start hosting as soon as the app opens.
    pub start_hosting: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            port: 8443,
            ice_tcp: true,
            public_ips: String::new(),
            web_root: String::new(),
            display: None,
            codec: "h264".into(),
            width: 1920,
            height: 1080,
            fps: 60,
            kbps: 20_000,
            quality: "balanced".into(),
            game_audio: true,
            audio_kbps: 128,
            audio_frame_ms: 10,
            mic: true,
            mic_device: "auto".into(),
            allow_resolution_change: false,
            allow_upload: false,
            upload_dir: String::new(),
            extra_args: String::new(),
            start_hosting: true,
        }
    }
}

impl Settings {
    /// The settings in `dir`, or the defaults if there are none yet.
    pub fn load(dir: &Path) -> anyhow::Result<Self> {
        let path = dir.join(FILE_NAME);
        match std::fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str(&s).with_context(|| format!("{}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("{}", path.display())),
        }
    }

    pub fn save(&self, dir: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(FILE_NAME);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, &path).with_context(|| format!("{}", path.display()))
    }

    /// The `ws-host` options these settings stand for.
    pub fn to_args(&self) -> anyhow::Result<Vec<String>> {
        let mut args = vec!["--port".to_string(), self.port.to_string()];
        if !self.ice_tcp {
            args.push("--no-ice-tcp".into());
        }
        for ip in self
            .public_ips
            .split([' ', ',', ';'])
            .filter(|s| !s.is_empty())
        {
            args.extend(["--public-ip".into(), ip.into()]);
        }
        let web_root = self.web_root.trim();
        if !web_root.is_empty() {
            args.extend(["--web-root".into(), web_root.into()]);
        }
        if let Some(d) = self.display {
            args.extend(["--display".into(), d.to_string()]);
        }
        args.extend([
            "--codec".into(),
            self.codec.clone(),
            "--size".into(),
            format!("{}x{}", self.width, self.height),
            "--fps".into(),
            self.fps.to_string(),
            "--kbps".into(),
            self.kbps.to_string(),
            "--quality".into(),
            self.quality.clone(),
        ]);
        if !self.game_audio {
            args.push("--no-audio".into());
        }
        args.extend([
            "--audio-kbps".into(),
            self.audio_kbps.to_string(),
            "--audio-frame-ms".into(),
            self.audio_frame_ms.to_string(),
        ]);
        if self.mic {
            args.extend(["--mic-device".into(), self.mic_device.clone()]);
        } else {
            args.push("--no-mic".into());
        }
        if self.allow_resolution_change {
            args.push("--allow-resolution-change".into());
        }
        if self.allow_upload {
            args.push("--allow-upload".into());
            let dir = self.upload_dir.trim();
            if !dir.is_empty() {
                args.extend(["--upload-dir".into(), dir.into()]);
            }
        }
        args.extend(split_args(&self.extra_args)?);
        Ok(args)
    }

    pub fn serve_options(&self) -> anyhow::Result<ServeOptions> {
        ServeOptions::parse(&self.to_args()?)
    }

    /// The same host as a `ws-host` command line, to copy into a console.
    pub fn command_line(&self) -> anyhow::Result<String> {
        let args = self.to_args()?;
        let quoted: Vec<String> = args
            .iter()
            .map(|a| {
                if a.is_empty() || a.contains([' ', '\t']) {
                    format!("\"{a}\"")
                } else {
                    a.clone()
                }
            })
            .collect();
        Ok(format!("ws-host {}", quoted.join(" ")))
    }
}

/// Splits on whitespace, keeping "double quoted" parts (paths with spaces)
/// together.
fn split_args(s: &str) -> anyhow::Result<Vec<String>> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_arg = false;
    let mut quoted = false;
    for c in s.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                in_arg = true;
            }
            c if c.is_whitespace() && !quoted => {
                if in_arg {
                    args.push(std::mem::take(&mut current));
                    in_arg = false;
                }
            }
            c => {
                current.push(c);
                in_arg = true;
            }
        }
    }
    anyhow::ensure!(!quoted, "unmatched \" in the extra options");
    if in_arg {
        args.push(current);
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_console_defaults() {
        let gui = Settings::default().serve_options().unwrap();
        let cli = ServeOptions::parse(&[]).unwrap();
        assert_eq!(gui, cli);
    }

    #[test]
    fn settings_become_options() {
        let s = Settings {
            port: 9000,
            ice_tcp: false,
            public_ips: "203.0.113.5, 198.51.100.7".into(),
            display: Some(2),
            codec: "av1".into(),
            mic: false,
            game_audio: false,
            allow_resolution_change: true,
            allow_upload: true,
            upload_dir: "D:\\Received files".into(),
            audio_kbps: 256,
            audio_frame_ms: 20,
            extra_args: r#"--hostname home.example.net --data-dir "C:\some dir""#.into(),
            ..Settings::default()
        };
        let o = s.serve_options().unwrap();
        assert_eq!((o.port, o.ice_tcp_port), (9000, None));
        assert_eq!(o.public_ips.len(), 2);
        assert_eq!(o.media.display.map(|d| d.0), Some(2));
        assert_eq!(o.media.mic_device, None);
        assert!(!o.media.game_audio && o.media.allow_resolution_change);
        assert_eq!(
            o.media.upload_dir,
            Some(std::path::PathBuf::from("D:\\Received files"))
        );
        assert_eq!((o.media.audio_kbps, o.media.audio_frame_ms), (256, 20));
        assert_eq!(o.hostnames, vec!["home.example.net".to_string()]);
        assert_eq!(o.data_dir, std::path::PathBuf::from(r"C:\some dir"));
        assert!(s.command_line().unwrap().contains(r#""C:\some dir""#));
    }

    #[test]
    fn bad_settings_are_errors() {
        let bad_size = Settings {
            width: 1921,
            ..Settings::default()
        };
        assert!(bad_size.serve_options().is_err());
        let bad_extra = Settings {
            extra_args: "--bogus".into(),
            ..Settings::default()
        };
        assert!(bad_extra.serve_options().is_err());
        let bad_frame = Settings {
            audio_frame_ms: 15,
            ..Settings::default()
        };
        assert!(bad_frame.serve_options().is_err());
        let unmatched = Settings {
            extra_args: "--hostname \"x".into(),
            ..Settings::default()
        };
        assert!(unmatched.serve_options().is_err());
    }

    #[test]
    fn saves_and_loads() {
        let dir = std::env::temp_dir().join(format!("ws-gui-test-{}", std::process::id()));
        assert_eq!(Settings::load(&dir).unwrap(), Settings::default());
        let s = Settings {
            port: 9100,
            ..Settings::default()
        };
        s.save(&dir).unwrap();
        assert_eq!(Settings::load(&dir).unwrap(), s);
        // Fields added later fall back to their defaults.
        std::fs::write(dir.join(FILE_NAME), r#"{"port": 9200}"#).unwrap();
        let loaded = Settings::load(&dir).unwrap();
        assert_eq!((loaded.port, loaded.fps), (9200, 60));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
