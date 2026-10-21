//! Command-line options for `ws-host serve`.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};
use ws_input::GamepadKind;
use ws_proto::{DisplayId, VideoCodec, VideoConfig, VideoQuality};

pub const USAGE: &str = "\
ws-host [serve] [options]      run the host (the default)
ws-host displays               list capturable displays
ws-host audio-devices          list playback devices (for --mic-device, --audio-device)
ws-host capture-test [...]     encode a display to a file, see --help there

serve options:
  --port P             HTTPS (TCP) and media (UDP) port, default 8443
  --ice-tcp-port P     ICE-TCP fallback port, default port+1
  --no-ice-tcp         don't listen for ICE-TCP
  --ip IP              local address for media (repeatable; default: every LAN IPv4)
  --public-ip IP       public address that forwards to the host (repeatable)
  --hostname NAME      extra name or IP for the self-signed certificate (repeatable)
  --cert FILE --key FILE   PEM certificate instead of a self-signed one
  --data-dir DIR       certificate and paired devices
  --forget-after DAYS  unpair a browser unused for this long, default 30
  --allow-origin URL   extra origin allowed to use the API, e.g. a reverse
                       proxy's https://name (repeatable)
  --web-root DIR       serve the web client from DIR instead of the one built in
  --display N          display to stream first, default the primary
  --codec h264|h265|av1  --size WxH  --fps F  --kbps K   stream settings
  --quality speed|balanced|clarity  encoder effort, default balanced
  --mic-device NAME    playback device the browser mic plays into; default \"auto\":
                       VB-CABLE's \"CABLE Input\", else Steam Streaming Microphone
  --no-mic             don't play the browser mic
  --no-audio           don't send game audio
  --audio-device NAME  output device to capture game audio from, default the
                       Windows default output (follows it when it changes)
  --audio-kbps K       game audio Opus bitrate, 32-510, default 128
  --audio-frame-ms 10|20  game audio Opus frame; 20 sounds better, 10 ms sooner
  --gamepad xbox360|ds4  virtual controller for browser gamepads, default xbox360
  --allow-resolution-change  let the browser change the streamed display's
                       resolution to match its screen (undone on disconnect)
  --allow-upload       let the browser send files to this PC; they are saved
                       in --upload-dir and never opened by the host
  --upload-dir DIR     where received files go, default Downloads\\web-stream
  --pair               open pairing for two minutes at startup
";

#[derive(Debug, Clone, PartialEq)]
pub struct ServeOptions {
    pub port: u16,
    pub ice_tcp_port: Option<u16>,
    pub ips: Vec<IpAddr>,
    pub public_ips: Vec<IpAddr>,
    pub hostnames: Vec<String>,
    pub cert: Option<(PathBuf, PathBuf)>,
    pub data_dir: PathBuf,
    /// A paired browser unused for this long is unpaired.
    pub device_idle_expiry: Duration,
    /// Origins besides the host's own allowed to call the API.
    pub extra_origins: Vec<String>,
    pub web_root: Option<PathBuf>,
    pub media: MediaOptions,
    pub pair_at_start: bool,
}

/// What [`crate::media`] needs to know.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaOptions {
    pub display: Option<DisplayId>,
    pub video: VideoConfig,
    /// `None` disables the mic.
    pub mic_device: Option<String>,
    pub game_audio: bool,
    /// Substring of the output device game audio is captured from; `None`
    /// follows the default output.
    pub audio_device: Option<String>,
    pub audio_kbps: u32,
    /// Opus frame length, one of `ws_audio::FRAME_MS`.
    pub audio_frame_ms: u32,
    /// Virtual controller plugged in for each browser gamepad.
    pub gamepad: GamepadKind,
    /// The browser may change the host display's resolution.
    pub allow_resolution_change: bool,
    /// Where files from the browser are saved; `None` refuses them.
    pub upload_dir: Option<PathBuf>,
}

impl ServeOptions {
    pub fn bind(&self) -> SocketAddr {
        (IpAddr::from([0, 0, 0, 0]), self.port).into()
    }

    pub fn parse(args: &[String]) -> anyhow::Result<Self> {
        let mut port = 8443u16;
        let mut ice_tcp_port = None;
        let mut no_ice_tcp = false;
        let mut ips = Vec::new();
        let mut public_ips = Vec::new();
        let mut hostnames = Vec::new();
        let (mut cert, mut key) = (None, None);
        let mut data_dir = None;
        let mut web_root = None;
        let mut forget_after_days = 30u32;
        let mut extra_origins = Vec::new();
        let mut media = MediaOptions {
            display: None,
            video: VideoConfig::default(),
            mic_device: Some(ws_audio::AUTO_MIC_DEVICE.into()),
            game_audio: true,
            audio_device: None,
            audio_kbps: DEFAULT_AUDIO_KBPS,
            audio_frame_ms: 10,
            gamepad: GamepadKind::Xbox360,
            allow_resolution_change: false,
            upload_dir: None,
        };
        let mut no_mic = false;
        let mut allow_upload = false;
        let mut upload_dir = None;
        let mut pair_at_start = false;

        let mut it = args.iter();
        while let Some(flag) = it.next() {
            let mut value = || it.next().with_context(|| format!("{flag} needs a value"));
            match flag.as_str() {
                "--port" => port = value()?.parse().context("--port")?,
                "--ice-tcp-port" => {
                    ice_tcp_port = Some(value()?.parse().context("--ice-tcp-port")?)
                }
                "--no-ice-tcp" => no_ice_tcp = true,
                "--ip" => ips.push(value()?.parse().context("--ip")?),
                "--public-ip" => public_ips.push(value()?.parse().context("--public-ip")?),
                "--hostname" => hostnames.push(value()?.clone()),
                "--cert" => cert = Some(PathBuf::from(value()?)),
                "--key" => key = Some(PathBuf::from(value()?)),
                "--data-dir" => data_dir = Some(PathBuf::from(value()?)),
                "--web-root" => web_root = Some(PathBuf::from(value()?)),
                "--forget-after" => {
                    forget_after_days = value()?.parse().context("--forget-after")?
                }
                "--allow-origin" => extra_origins.push(parse_origin(value()?)?),
                "--display" => {
                    media.display = Some(DisplayId(value()?.parse().context("--display")?))
                }
                "--codec" => media.video.codec = parse_codec(value()?)?,
                "--size" => {
                    let (w, h) = parse_size(value()?)?;
                    media.video.width = w;
                    media.video.height = h;
                }
                "--fps" => media.video.fps = value()?.parse().context("--fps")?,
                "--kbps" => media.video.bitrate_kbps = value()?.parse().context("--kbps")?,
                "--quality" => media.video.quality = parse_quality(value()?)?,
                "--mic-device" => media.mic_device = Some(value()?.clone()),
                "--no-mic" => no_mic = true,
                "--no-audio" => media.game_audio = false,
                "--audio-device" => media.audio_device = Some(value()?.clone()),
                "--audio-kbps" => media.audio_kbps = value()?.parse().context("--audio-kbps")?,
                "--audio-frame-ms" => {
                    media.audio_frame_ms = value()?.parse().context("--audio-frame-ms")?
                }
                "--gamepad" => media.gamepad = parse_gamepad(value()?)?,
                "--allow-resolution-change" => media.allow_resolution_change = true,
                "--allow-upload" => allow_upload = true,
                "--upload-dir" => upload_dir = Some(PathBuf::from(value()?)),
                "--pair" => pair_at_start = true,
                f => bail!("unknown flag {f}\n\n{USAGE}"),
            }
        }
        if no_mic {
            media.mic_device = None;
        }
        if allow_upload {
            media.upload_dir = Some(upload_dir.unwrap_or_else(default_upload_dir));
        }
        let cert = match (cert, key) {
            (Some(c), Some(k)) => Some((c, k)),
            (None, None) => None,
            _ => bail!("--cert and --key go together"),
        };
        let ice_tcp_port = if no_ice_tcp {
            None
        } else {
            Some(match ice_tcp_port {
                Some(p) => p,
                None => port
                    .checked_add(1)
                    .context("--port too high for ICE-TCP on port+1")?,
            })
        };
        if !(32..=510).contains(&media.audio_kbps) {
            bail!("--audio-kbps is 32 to 510");
        }
        if !ws_audio::FRAME_MS.contains(&media.audio_frame_ms) {
            bail!("--audio-frame-ms is 10 or 20");
        }
        if !(1..=3650).contains(&forget_after_days) {
            bail!("--forget-after is 1 to 3650 days");
        }
        let clamped = media.video.clamped();
        if clamped != media.video {
            bail!(
                "stream settings out of range: sizes are even, 320x180 to 7680x4320, \
                 1-240 fps, 500-200000 kbps"
            );
        }
        Ok(Self {
            port,
            ice_tcp_port,
            ips,
            public_ips,
            hostnames,
            cert,
            data_dir: data_dir.unwrap_or_else(default_data_dir),
            device_idle_expiry: Duration::from_secs(u64::from(forget_after_days) * 24 * 3600),
            extra_origins,
            web_root,
            media,
            pair_at_start,
        })
    }
}

/// Opus bitrate for game audio, as `ws_audio::DEFAULT_BITRATE`.
pub const DEFAULT_AUDIO_KBPS: u32 = 128;

/// An origin as browsers send it: `https://name[:port]`, no path.
fn parse_origin(s: &str) -> anyhow::Result<String> {
    let s = s.trim_end_matches('/');
    let Some(rest) = s.strip_prefix("https://") else {
        bail!("--allow-origin {s:?}: must start with https://");
    };
    if rest.is_empty() || rest.contains(['/', '?', '#', '@', ' ']) {
        bail!("--allow-origin {s:?}: give just https://name[:port]");
    }
    Ok(s.to_ascii_lowercase())
}

pub fn parse_codec(s: &str) -> anyhow::Result<VideoCodec> {
    Ok(match s {
        "h264" => VideoCodec::H264,
        "h265" | "hevc" => VideoCodec::H265,
        "av1" => VideoCodec::Av1,
        c => bail!("unknown codec {c}"),
    })
}

pub fn parse_quality(s: &str) -> anyhow::Result<VideoQuality> {
    Ok(match s {
        "speed" => VideoQuality::Speed,
        "balanced" => VideoQuality::Balanced,
        "clarity" => VideoQuality::Clarity,
        q => bail!("unknown quality {q}"),
    })
}

pub fn parse_gamepad(s: &str) -> anyhow::Result<GamepadKind> {
    Ok(match s {
        "xbox360" | "xbox" => GamepadKind::Xbox360,
        "ds4" | "dualshock4" => GamepadKind::DualShock4,
        g => bail!("unknown gamepad {g}: xbox360 or ds4"),
    })
}

pub fn parse_size(s: &str) -> anyhow::Result<(u32, u32)> {
    let (w, h) = s.split_once('x').context("size is WxH")?;
    Ok((w.parse()?, h.parse()?))
}

/// `%LOCALAPPDATA%\web-stream` on Windows, the XDG data dir elsewhere.
pub fn default_data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("XDG_DATA_HOME"))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    match base {
        Some(b) => b.join("web-stream"),
        None => PathBuf::from("ws-data"),
    }
}

/// `Downloads\web-stream` in the user's profile (the home directory
/// elsewhere).
pub fn default_upload_dir() -> PathBuf {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    match home {
        Some(h) => PathBuf::from(h).join("Downloads").join("web-stream"),
        None => PathBuf::from("web-stream-received"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> anyhow::Result<ServeOptions> {
        ServeOptions::parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn defaults() {
        let o = parse(&[]).unwrap();
        assert_eq!((o.port, o.ice_tcp_port), (8443, Some(8444)));
        assert_eq!(o.media.mic_device.as_deref(), Some("auto"));
        assert!(o.media.game_audio && !o.pair_at_start);
        assert!(!o.media.allow_resolution_change);
        assert_eq!(o.media.video, VideoConfig::default());
        assert_eq!(o.media.audio_device, None);
        assert_eq!(o.media.audio_kbps * 1000, ws_audio::DEFAULT_BITRATE);
        assert_eq!(o.media.audio_frame_ms, 10);
        assert_eq!(o.media.gamepad, GamepadKind::Xbox360);
        assert_eq!(
            parse(&["--gamepad", "ds4"]).unwrap().media.gamepad,
            GamepadKind::DualShock4
        );
        assert!(parse(&["--gamepad", "wii"]).is_err());
        assert_eq!(
            parse(&["--audio-frame-ms", "20"])
                .unwrap()
                .media
                .audio_frame_ms,
            20
        );
        assert!(parse(&["--audio-frame-ms", "5"]).is_err());
        assert_eq!(o.device_idle_expiry, Duration::from_secs(30 * 24 * 3600));
        assert!(o.extra_origins.is_empty());
    }

    #[test]
    fn uploads_are_off_unless_allowed() {
        assert_eq!(parse(&[]).unwrap().media.upload_dir, None);
        assert_eq!(
            parse(&["--upload-dir", "x"]).unwrap().media.upload_dir,
            None
        );
        assert_eq!(
            parse(&["--allow-upload", "--upload-dir", "x"])
                .unwrap()
                .media
                .upload_dir,
            Some(PathBuf::from("x"))
        );
        let dir = parse(&["--allow-upload"])
            .unwrap()
            .media
            .upload_dir
            .unwrap();
        assert!(dir.ends_with("Downloads/web-stream") || dir.ends_with("web-stream-received"));
    }

    #[test]
    fn flags() {
        let o = parse(&[
            "--port",
            "9000",
            "--no-mic",
            "--public-ip",
            "203.0.113.5",
            "--codec",
            "av1",
            "--size",
            "2560x1440",
            "--display",
            "2",
            "--pair",
            "--allow-resolution-change",
            "--quality",
            "speed",
            "--audio-device",
            "Speakers",
            "--audio-kbps",
            "256",
            "--forget-after",
            "7",
            "--allow-origin",
            "https://Stream.example.net/",
        ])
        .unwrap();
        assert_eq!(o.media.audio_device.as_deref(), Some("Speakers"));
        assert_eq!(o.media.audio_kbps, 256);
        assert_eq!(o.device_idle_expiry, Duration::from_secs(7 * 24 * 3600));
        assert_eq!(o.extra_origins, vec!["https://stream.example.net"]);
        assert!(o.media.allow_resolution_change);
        assert_eq!(o.media.video.quality, VideoQuality::Speed);
        assert_eq!((o.port, o.ice_tcp_port), (9000, Some(9001)));
        assert_eq!(o.media.mic_device, None);
        assert_eq!(o.public_ips, vec![IpAddr::from([203, 0, 113, 5])]);
        assert_eq!(o.media.video.codec, VideoCodec::Av1);
        assert_eq!((o.media.video.width, o.media.video.height), (2560, 1440));
        assert_eq!(o.media.display, Some(DisplayId(2)));
        assert!(o.pair_at_start);
        assert_eq!(parse(&["--no-ice-tcp"]).unwrap().ice_tcp_port, None);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(parse(&["--cert", "a.pem"]).is_err());
        assert!(parse(&["--size", "1921x1080"]).is_err());
        assert!(parse(&["--port", "65535"]).is_err());
        assert!(parse(&["--bogus"]).is_err());
        assert!(parse(&["--quality", "max"]).is_err());
        assert!(parse(&["--audio-kbps", "8"]).is_err());
        assert!(parse(&["--forget-after", "0"]).is_err());
        assert!(parse(&["--allow-origin", "http://x.example"]).is_err());
        assert!(parse(&["--allow-origin", "https://x.example/path"]).is_err());
    }
}
