//! What the settings window edits, saved as JSON in the data dir. The host
//! is started from the equivalent host options (`ws_host::options`), so the
//! window, its "More options" field and the service check exactly the same
//! things.

use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use ws_host::options::ServeOptions;

pub const FILE_NAME: &str = "gui-settings.json";

pub const CODECS: &[&str] = &["h264", "h265", "av1"];
pub const QUALITIES: &[&str] = &["speed", "balanced", "clarity"];
pub const GAMEPADS: &[(&str, &str)] = &[("xbox360", "Xbox 360"), ("ds4", "DualShock 4")];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub port: u16,
    pub ice_tcp: bool,
    /// `None`: the port after `port`.
    pub ice_tcp_port: Option<u16>,
    /// Public addresses that forward to this PC, separated by spaces or commas.
    pub public_ips: String,
    /// Names or IPs browsers reach this PC by (a DDNS name), for the
    /// self-signed certificate; separated like `public_ips`.
    pub hostnames: String,
    /// A DuckDNS subdomain: the host gets a Let's Encrypt certificate for
    /// it and keeps it pointing here (`--duckdns`). Empty: none.
    pub duckdns: String,
    /// A newly typed DuckDNS token, until saved to its own file in the data
    /// folder ([`ws_host::options::DUCKDNS_TOKEN_FILE`]); never in the JSON.
    #[serde(skip)]
    pub duckdns_token: String,
    /// Where Let's Encrypt sends expiry warnings (optional).
    pub acme_email: String,
    /// A paired browser unused for this many days is unpaired.
    pub forget_after_days: u32,
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
    /// Output device game audio is captured from; empty: the Windows
    /// default output.
    pub audio_device: String,
    /// Game audio Opus bitrate (kb/s) and frame length (10 or 20 ms). A
    /// browser can override both from its own settings.
    pub audio_kbps: u32,
    pub audio_frame_ms: u32,
    pub mic: bool,
    pub mic_device: String,
    /// One of [`GAMEPADS`].
    pub gamepad: String,
    pub allow_resolution_change: bool,
    /// Let the browser send files to this PC.
    pub allow_upload: bool,
    /// Where they go; empty: `Downloads\web-stream`.
    pub upload_dir: String,
    /// Let a paired browser update the host to the latest `main` and
    /// restart it (`scripts\update.ps1`).
    pub allow_remote_update: bool,
    /// More host options, for what the window has no field for.
    pub extra_args: String,
    /// Start hosting as soon as the app opens.
    pub start_hosting: bool,
    /// Ask the router to forward the ports (UPnP). Off unless the user
    /// turns it on.
    pub upnp: bool,
    /// The first-run setup was finished or dismissed.
    pub setup_done: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            port: 8443,
            ice_tcp: true,
            ice_tcp_port: None,
            public_ips: String::new(),
            hostnames: String::new(),
            duckdns: String::new(),
            duckdns_token: String::new(),
            acme_email: String::new(),
            forget_after_days: 30,
            web_root: String::new(),
            display: None,
            codec: "h264".into(),
            width: 1920,
            height: 1080,
            fps: 60,
            kbps: 20_000,
            quality: "balanced".into(),
            game_audio: true,
            audio_device: String::new(),
            audio_kbps: 128,
            audio_frame_ms: 10,
            mic: true,
            mic_device: "auto".into(),
            gamepad: "xbox360".into(),
            allow_resolution_change: false,
            allow_upload: false,
            upload_dir: String::new(),
            allow_remote_update: false,
            extra_args: String::new(),
            start_hosting: true,
            upnp: false,
            setup_done: false,
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
        let token = self.duckdns_token.trim();
        if !token.is_empty() {
            ws_host::options::save_duckdns_token(dir, token)?;
        }
        let path = dir.join(FILE_NAME);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, &path).with_context(|| format!("{}", path.display()))
    }

    /// The host options these settings stand for.
    pub fn to_args(&self) -> anyhow::Result<Vec<String>> {
        let mut args = vec!["--port".to_string(), self.port.to_string()];
        if !self.ice_tcp {
            args.push("--no-ice-tcp".into());
        } else if let Some(p) = self.ice_tcp_port {
            args.extend(["--ice-tcp-port".into(), p.to_string()]);
        }
        for ip in list(&self.public_ips) {
            args.extend(["--public-ip".into(), ip.into()]);
        }
        for name in list(&self.hostnames) {
            args.extend(["--hostname".into(), name.into()]);
        }
        let duckdns = self.duckdns.trim();
        if !duckdns.is_empty() {
            args.extend(["--duckdns".into(), duckdns.into()]);
            let email = self.acme_email.trim();
            if !email.is_empty() {
                args.extend(["--acme-email".into(), email.into()]);
            }
        }
        args.extend(["--forget-after".into(), self.forget_after_days.to_string()]);
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
        let audio_device = self.audio_device.trim();
        if !audio_device.is_empty() {
            args.extend(["--audio-device".into(), audio_device.into()]);
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
        args.extend(["--gamepad".into(), self.gamepad.clone()]);
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
        if self.allow_remote_update {
            args.push("--allow-remote-update".into());
        }
        args.extend(split_args(&self.extra_args)?);
        Ok(args)
    }

    /// The ports a router forwards to this PC, for UPnP.
    pub fn forwarded_ports(&self) -> Vec<crate::upnp::Port> {
        let ice_tcp = self
            .ice_tcp
            .then(|| self.ice_tcp_port.unwrap_or(self.port.saturating_add(1)));
        crate::upnp::ports(self.port, ice_tcp)
    }

    pub fn serve_options(&self) -> anyhow::Result<ServeOptions> {
        ServeOptions::parse(&self.to_args()?)
    }

    /// The options for `service install`: these settings without what
    /// belongs to the tray app's own host (its web root, data folder,
    /// uploads and updates from the browser), which the service refuses.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn service_args(&self) -> anyhow::Result<Vec<String>> {
        const WITH_VALUE: &[&str] = &["--web-root", "--data-dir", "--upload-dir"];
        const ALONE: &[&str] = &["--allow-upload", "--allow-remote-update", "--pair"];
        let mut out = Vec::new();
        let mut it = self.to_args()?.into_iter();
        while let Some(arg) = it.next() {
            if WITH_VALUE.contains(&arg.as_str()) {
                it.next();
            } else if !ALONE.contains(&arg.as_str()) {
                out.push(arg);
            }
        }
        Ok(out)
    }
}

/// The items of a list typed as `a, b c`.
fn list(s: &str) -> impl Iterator<Item = &str> {
    s.split([' ', ',', ';']).filter(|s| !s.is_empty())
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
    fn defaults_match_the_option_defaults() {
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
            allow_remote_update: true,
            audio_kbps: 256,
            audio_frame_ms: 20,
            ice_tcp_port: Some(9500),
            hostnames: "home.example.net".into(),
            forget_after_days: 7,
            audio_device: "Speakers (Realtek)".into(),
            gamepad: "ds4".into(),
            extra_args: r#"--hostname 192.168.1.20 --data-dir "C:\some dir""#.into(),
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
        assert!(o.media.update_log.is_some());
        assert_eq!(
            Settings::default()
                .serve_options()
                .unwrap()
                .media
                .update_log,
            None
        );
        assert_eq!(o.hostnames, vec!["home.example.net", "192.168.1.20"]);
        assert_eq!(
            o.device_idle_expiry,
            std::time::Duration::from_secs(7 * 24 * 3600)
        );
        assert_eq!(o.media.audio_device.as_deref(), Some("Speakers (Realtek)"));
        assert_eq!(o.media.gamepad, ws_input::GamepadKind::DualShock4);
        assert_eq!(o.data_dir, std::path::PathBuf::from(r"C:\some dir"));

        // What the service may run with.
        let service = s.service_args().unwrap();
        let o = ws_host::service::check_install_args(&service).unwrap();
        assert_eq!((o.port, o.ice_tcp_port), (9000, None));
        assert_eq!(o.media.gamepad, ws_input::GamepadKind::DualShock4);
        assert_eq!(o.hostnames, vec!["home.example.net", "192.168.1.20"]);
        assert_eq!((o.media.upload_dir, o.media.update_log), (None, None));
        assert!(!service.iter().any(|a| a.contains("some dir")));
    }

    #[test]
    fn duckdns_token_goes_to_its_own_file() {
        let dir = tempfile::tempdir().unwrap();
        let s = Settings {
            duckdns: "my-pc".into(),
            duckdns_token: " a7c4d0ad-token \n".into(),
            acme_email: "me@example.com".into(),
            ..Settings::default()
        };
        s.save(dir.path()).unwrap();
        let token = dir.path().join(ws_host::options::DUCKDNS_TOKEN_FILE);
        assert_eq!(std::fs::read_to_string(token).unwrap(), "a7c4d0ad-token");
        let saved = std::fs::read_to_string(dir.path().join(FILE_NAME)).unwrap();
        assert!(!saved.contains("a7c4d0ad"));
        let o = s.serve_options().unwrap().duckdns.unwrap();
        assert_eq!(o.name, "my-pc");
        assert_eq!(o.email.as_deref(), Some("me@example.com"));
    }

    #[test]
    fn bad_settings_are_errors() {
        let bad_size = Settings {
            width: 1921,
            ..Settings::default()
        };
        assert!(bad_size.serve_options().is_err());
        let ice = Settings {
            ice_tcp_port: Some(9500),
            ..Settings::default()
        };
        assert_eq!(ice.serve_options().unwrap().ice_tcp_port, Some(9500));
        let bad_extra = Settings {
            extra_args: "--bogus".into(),
            ..Settings::default()
        };
        assert!(bad_extra.serve_options().is_err());
        let bad_gamepad = Settings {
            gamepad: "wii".into(),
            ..Settings::default()
        };
        assert!(bad_gamepad.serve_options().is_err());
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
