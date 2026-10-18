//! Command-line options for `ws-host serve`.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use anyhow::{Context, bail};
use ws_proto::{DisplayId, VideoCodec, VideoConfig};

pub const USAGE: &str = "\
ws-host [serve] [options]      run the host (the default)
ws-host displays               list capturable displays
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
  --web-root DIR       web client to serve, default <exe dir>/web
  --display N          display to stream first, default the primary
  --codec h264|h265|av1  --size WxH  --fps F  --kbps K   stream settings
  --mic-device NAME    playback device the browser mic plays into, default \"CABLE Input\"
  --no-mic             don't play the browser mic
  --no-audio           don't send game audio
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
        let mut media = MediaOptions {
            display: None,
            video: VideoConfig::default(),
            mic_device: Some("CABLE Input".into()),
            game_audio: true,
        };
        let mut no_mic = false;
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
                "--mic-device" => media.mic_device = Some(value()?.clone()),
                "--no-mic" => no_mic = true,
                "--no-audio" => media.game_audio = false,
                "--pair" => pair_at_start = true,
                f => bail!("unknown flag {f}\n\n{USAGE}"),
            }
        }
        if no_mic {
            media.mic_device = None;
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
            web_root: web_root.or_else(default_web_root),
            media,
            pair_at_start,
        })
    }
}

pub fn parse_codec(s: &str) -> anyhow::Result<VideoCodec> {
    Ok(match s {
        "h264" => VideoCodec::H264,
        "h265" | "hevc" => VideoCodec::H265,
        "av1" => VideoCodec::Av1,
        c => bail!("unknown codec {c}"),
    })
}

pub fn parse_size(s: &str) -> anyhow::Result<(u32, u32)> {
    let (w, h) = s.split_once('x').context("size is WxH")?;
    Ok((w.parse()?, h.parse()?))
}

/// `%LOCALAPPDATA%\web-stream` on Windows, the XDG data dir elsewhere.
fn default_data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("XDG_DATA_HOME"))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    match base {
        Some(b) => b.join("web-stream"),
        None => PathBuf::from("ws-data"),
    }
}

/// `web/` next to the executable. Never the working directory: whatever is
/// served there runs as the host's own origin, with the paired cookie.
fn default_web_root() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?.join("web");
    dir.join("index.html").is_file().then_some(dir)
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
        assert_eq!(o.media.mic_device.as_deref(), Some("CABLE Input"));
        assert!(o.media.game_audio && !o.pair_at_start);
        assert_eq!(o.media.video, VideoConfig::default());
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
        ])
        .unwrap();
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
    }
}
