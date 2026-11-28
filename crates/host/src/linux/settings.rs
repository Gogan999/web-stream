//! The Linux host's settings, saved as JSON in the data dir and edited on the settings page.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

use crate::options::ServeOptions;

pub const FILE_NAME: &str = "settings.json";

/// The longest text a setting may hold.
const MAX_TEXT: usize = 512;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub port: u16,
    pub ice_tcp: bool,
    /// `None`: the port after `port`.
    pub ice_tcp_port: Option<u16>,
    /// More UDP media ports, separated by spaces or commas.
    pub extra_udp_ports: String,
    /// Public addresses that forward to this PC, separated like `extra_udp_ports`.
    pub public_ips: String,
    /// Names or IPs browsers reach this PC by, for the self-signed certificate.
    pub hostnames: String,
    /// `off`, `named` or `quick`.
    pub tunnel: String,
    /// The named tunnel's public name; empty: the one cloudflared reports.
    pub tunnel_hostname: String,
    /// A paired browser unused for this many days is unpaired.
    pub forget_after_days: u32,
    /// Other origins (`https://name`) allowed to use the API.
    pub allow_origins: String,
    /// `None`: the primary display.
    pub display: Option<u32>,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub kbps: u32,
    pub quality: String,
    pub game_audio: bool,
    /// Output device game audio is captured from; empty: the default output.
    pub audio_device: String,
    pub audio_kbps: u32,
    pub audio_frame_ms: u32,
    pub mic: bool,
    pub mic_device: String,
    /// `xbox360` or `ds4`.
    pub gamepad: String,
    pub allow_resolution_change: bool,
    pub clipboard: bool,
    /// Let the browser send files to this PC: the signed-in user's Downloads, else the data dir's `uploads`.
    pub allow_upload: bool,
    /// Ask the router to forward the ports (UPnP).
    pub upnp: bool,
    /// Let paired devices open the settings page.
    pub remote_admin: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            port: 8443,
            ice_tcp: true,
            ice_tcp_port: None,
            extra_udp_ports: String::new(),
            public_ips: String::new(),
            hostnames: String::new(),
            tunnel: "off".into(),
            tunnel_hostname: String::new(),
            forget_after_days: 30,
            allow_origins: String::new(),
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
            clipboard: true,
            allow_upload: false,
            upnp: false,
            remote_admin: true,
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

    /// Checks the settings, then saves them in `dir`, readable only by this user.
    pub fn save(&self, dir: &Path) -> anyhow::Result<()> {
        self.check(dir)?;
        let path = dir.join(FILE_NAME);
        let json = serde_json::to_vec_pretty(self)?;
        ws_signaling::write_private(&path, &json).with_context(|| format!("{}", path.display()))
    }

    /// Fails unless every setting is in range.
    pub fn check(&self, dir: &Path) -> anyhow::Result<()> {
        for (name, text) in [
            ("extra UDP ports", &self.extra_udp_ports),
            ("public addresses", &self.public_ips),
            ("host names", &self.hostnames),
            ("tunnel", &self.tunnel),
            ("tunnel host name", &self.tunnel_hostname),
            ("allowed origins", &self.allow_origins),
            ("codec", &self.codec),
            ("quality", &self.quality),
            ("audio device", &self.audio_device),
            ("mic device", &self.mic_device),
            ("gamepad", &self.gamepad),
        ] {
            if text.len() > MAX_TEXT || text.chars().any(char::is_control) {
                bail!("{name}: too long or not plain text");
            }
        }
        let bad_name = |n: &&str| {
            n.starts_with('-')
                || !n
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':'))
        };
        if let Some(name) = list(&self.hostnames).find(bad_name) {
            bail!("host names: {name:?} is not a host name or address");
        }
        if !matches!(self.tunnel.as_str(), "off" | "named" | "quick") {
            bail!("tunnel is off, named or quick");
        }
        if self.mic && self.mic_device.trim().is_empty() {
            bail!("mic device: give a name or \"auto\"");
        }
        self.serve_options(dir)?;
        Ok(())
    }

    /// The host options these settings stand for; with nobody signed in, files go to `dir`'s `uploads`.
    pub fn to_args(&self, dir: &Path) -> Vec<String> {
        let mut args = vec!["--port".to_string(), self.port.to_string()];
        if !self.ice_tcp {
            args.push("--no-ice-tcp".into());
        } else if let Some(p) = self.ice_tcp_port {
            args.extend(["--ice-tcp-port".into(), p.to_string()]);
        }
        for p in list(&self.extra_udp_ports) {
            args.extend(["--extra-udp-port".into(), p.into()]);
        }
        for ip in list(&self.public_ips) {
            args.extend(["--public-ip".into(), ip.into()]);
        }
        for name in list(&self.hostnames) {
            args.extend(["--hostname".into(), name.into()]);
        }
        if self.tunnel != "off" {
            args.extend(["--tunnel".into(), self.tunnel.clone()]);
            let name = self.tunnel_hostname.trim();
            if self.tunnel == "named" && !name.is_empty() {
                args.extend(["--tunnel-hostname".into(), name.into()]);
            }
        }
        for origin in list(&self.allow_origins) {
            args.extend(["--allow-origin".into(), origin.into()]);
        }
        args.extend(["--forget-after".into(), self.forget_after_days.to_string()]);
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
            args.extend(["--mic-device".into(), self.mic_device.trim().into()]);
        } else {
            args.push("--no-mic".into());
        }
        args.extend(["--gamepad".into(), self.gamepad.clone()]);
        if self.allow_resolution_change {
            args.push("--allow-resolution-change".into());
        }
        if !self.clipboard {
            args.push("--no-clipboard".into());
        }
        if self.allow_upload {
            args.extend([
                "--allow-upload".into(),
                "--upload-dir".into(),
                upload_dir(dir).display().to_string(),
            ]);
        }
        args
    }

    pub fn serve_options(&self, dir: &Path) -> anyhow::Result<ServeOptions> {
        let mut args = self.to_args(dir);
        args.extend(["--data-dir".into(), dir.display().to_string()]);
        ServeOptions::parse(&args)
    }

    /// The ports a router forwards to this PC, for UPnP.
    pub fn forwarded_ports(opts: &ServeOptions) -> Vec<(bool, u16)> {
        let mut out = Vec::new();
        if opts.tunnel.is_none() {
            out.push((true, opts.port));
        }
        out.push((false, opts.port));
        out.extend(opts.ice_tcp_port.map(|p| (true, p)));
        for &p in &opts.extra_udp_ports {
            if !out.contains(&(false, p)) {
                out.push((false, p));
            }
        }
        out
    }
}

/// Where files from the browser go.
pub fn upload_dir(dir: &Path) -> PathBuf {
    dir.join("uploads")
}

/// The items of a list typed as `a, b c`.
fn list(s: &str) -> impl Iterator<Item = &str> {
    s.split([' ', ',', ';']).filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_option_defaults() {
        let dir = Path::new("/var/lib/web-stream");
        let ours = Settings::default().serve_options(dir).unwrap();
        let cli = ServeOptions::parse(&["--data-dir".into(), dir.display().to_string()]).unwrap();
        assert_eq!(ours, cli);
    }

    #[test]
    fn settings_become_options() {
        let dir = Path::new("/var/lib/web-stream");
        let s = Settings {
            port: 9000,
            ice_tcp: false,
            extra_udp_ports: "443, 3478 443".into(),
            public_ips: "203.0.113.5".into(),
            tunnel: "named".into(),
            tunnel_hostname: "pc.example.com".into(),
            codec: "av1".into(),
            mic: false,
            allow_upload: true,
            clipboard: false,
            gamepad: "ds4".into(),
            ..Settings::default()
        };
        let o = s.serve_options(dir).unwrap();
        assert_eq!((o.port, o.ice_tcp_port), (9000, None));
        assert_eq!(o.extra_udp_ports, vec![443, 3478]);
        assert_eq!(o.media.upload_dir, Some(dir.join("uploads")));
        assert_eq!(o.media.mic_device, None);
        assert!(!o.media.clipboard);
        assert_eq!(o.media.gamepad, ws_input::GamepadKind::DualShock4);
        assert!(o.tunnel.is_some());
        assert_eq!(
            Settings::forwarded_ports(&o),
            vec![(false, 9000), (false, 443), (false, 3478)]
        );
        assert_eq!(
            Settings::forwarded_ports(&Settings::default().serve_options(dir).unwrap()),
            vec![(true, 8443), (false, 8443), (true, 8444)]
        );
    }

    #[test]
    fn bad_settings_are_refused() {
        let dir = Path::new("d");
        for bad in [
            Settings {
                width: 1921,
                ..Settings::default()
            },
            Settings {
                tunnel: "other".into(),
                ..Settings::default()
            },
            Settings {
                hostnames: "a\nb".into(),
                ..Settings::default()
            },
            Settings {
                extra_udp_ports: "nope".into(),
                ..Settings::default()
            },
            Settings {
                audio_device: "x".repeat(MAX_TEXT + 1),
                ..Settings::default()
            },
            Settings {
                mic_device: " ".into(),
                ..Settings::default()
            },
            Settings {
                hostnames: "ok.example --web-root".into(),
                ..Settings::default()
            },
            Settings {
                gamepad: "wii".into(),
                ..Settings::default()
            },
        ] {
            assert!(bad.check(dir).is_err(), "{bad:?}");
        }
        assert!(Settings::default().check(dir).is_ok());
    }

    #[test]
    fn saves_and_loads() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Settings::load(dir.path()).unwrap(), Settings::default());
        let s = Settings {
            port: 9100,
            upnp: true,
            ..Settings::default()
        };
        s.save(dir.path()).unwrap();
        assert_eq!(Settings::load(dir.path()).unwrap(), s);
        std::fs::write(
            dir.path().join(FILE_NAME),
            r#"{"port": 9200, "unknown": 1}"#,
        )
        .unwrap();
        let loaded = Settings::load(dir.path()).unwrap();
        assert_eq!((loaded.port, loaded.fps), (9200, 60));
        let bad = Settings {
            fps: 0,
            ..Settings::default()
        };
        assert!(bad.save(dir.path()).is_err());
        assert_eq!(Settings::load(dir.path()).unwrap().port, 9200);
    }
}
