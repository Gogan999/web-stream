//! The commands that talk to the running service over its control socket (the console protocol).

use std::net::IpAddr;
use std::time::Duration;

use anyhow::{Context, bail};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use super::{Paths, admin, qr, settings::Settings};
use crate::console::EVENTS_JSON;
use crate::serve::{Event, Notice, PAIRING_WINDOW};

/// The client's addresses: the tunnel first, then each LAN address.
pub fn origins(port: u16, ips: &[IpAddr], tunnel: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = tunnel
        .map(|t| t.trim_end_matches('/').to_string())
        .into_iter()
        .collect();
    for ip in ips.iter().filter(|ip| !ip.is_loopback()) {
        out.push(match ip {
            IpAddr::V4(v4) => format!("https://{v4}:{port}"),
            IpAddr::V6(v6) => format!("https://[{v6}]:{port}"),
        });
    }
    out
}

async fn connect(paths: &Paths) -> anyhow::Result<UnixStream> {
    let path = paths.control();
    UnixStream::connect(&path)
        .await
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::PermissionDenied => {
                anyhow::anyhow!("controlling the service needs root: run it with sudo")
            }
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
                anyhow::anyhow!(
                    "the web-stream service is not running ({} is missing). Start it with \
             `sudo systemctl start web-stream`, or run `web-stream serve --pair` in the foreground",
                    path.display()
                )
            }
            _ => anyhow::Error::new(e).context(format!("{}", path.display())),
        })
}

/// Sends `command` to the service and hands each event it sends back to `on_event` until that
/// returns `Some` or `timeout` passes.
fn talk(
    paths: &Paths,
    command: &str,
    timeout: Duration,
    mut on_event: impl FnMut(Event) -> Option<anyhow::Result<()>>,
) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let stream = connect(paths).await?;
        let (read, mut write) = stream.into_split();
        write
            .write_all(format!("{EVENTS_JSON}\n{command}\n").as_bytes())
            .await?;
        let mut lines = BufReader::new(read).lines();
        let work = async {
            while let Some(line) = lines.next_line().await? {
                let Ok(e) = serde_json::from_str::<Event>(&line) else {
                    continue;
                };
                if let Some(done) = on_event(e) {
                    return done;
                }
            }
            bail!("the service closed the connection")
        };
        tokio::time::timeout(timeout, work)
            .await
            .unwrap_or_else(|_| bail!("no answer from the service in time"))
    })
}

/// `web-stream pair`.
pub fn pair(paths: &Paths) -> anyhow::Result<()> {
    let mut port_ips = None;
    let mut tunnel = None;
    talk(
        paths,
        "pair",
        PAIRING_WINDOW + Duration::from_secs(5),
        |e| {
            match e {
                Event::Started { port, ips, .. } => port_ips = Some((port, ips)),
                Event::Notice(Notice::Tunnel { url }) => tunnel = url,
                Event::PairingOpened { link, .. } => {
                    let (port, ips) = port_ips.clone().unwrap_or((8443, Vec::new()));
                    let all = origins(port, &ips, tunnel.as_deref());
                    println!("Pairing is open for two minutes. On the device, open:");
                    for origin in &all {
                        println!("  {origin}/#pair={link}");
                    }
                    if let Some(code) = all
                        .first()
                        .and_then(|o| qr::text(&format!("{o}/#pair={link}")))
                    {
                        println!("or scan:\n{code}");
                    }
                    println!(
                        "Or open the address without #pair=…, choose Pair, and type the code \
                         shown here."
                    );
                }
                Event::Notice(Notice::PairingCode {
                    code, device_name, ..
                }) => println!("\n  Code for {device_name:?}: {code}\n"),
                Event::Notice(Notice::PairingFailed { attempts_left }) => {
                    println!("Wrong code; {attempts_left} attempts left.")
                }
                Event::Notice(Notice::PairingLocked) => {
                    return Some(Err(anyhow::anyhow!(
                        "too many wrong codes: run `web-stream pair` again to unlock"
                    )));
                }
                Event::Notice(Notice::Paired { device_name }) => {
                    println!("Paired {device_name:?}.");
                    return Some(Ok(()));
                }
                Event::Error(e) => return Some(Err(anyhow::anyhow!(e))),
                _ => {}
            }
            None
        },
    )
    .map_err(|e| {
        if e.to_string().contains("in time") {
            anyhow::anyhow!("pairing closed: no device paired in two minutes")
        } else {
            e
        }
    })
}

/// `web-stream unpair-all`.
pub fn unpair_all(paths: &Paths) -> anyhow::Result<()> {
    talk(paths, "unpair-all", Duration::from_secs(10), |e| match e {
        Event::UnpairedAll => {
            println!("Every paired device was removed.");
            Some(Ok(()))
        }
        Event::Error(e) => Some(Err(anyhow::anyhow!(e))),
        _ => None,
    })
}

/// `web-stream settings`: the service's settings page, or a foreground host's.
pub fn settings_url(paths: &Paths) -> anyhow::Result<()> {
    let service = paths.network().join(admin::TOKEN_FILE);
    let (dir, token) = match std::fs::read_to_string(&service) {
        Ok(t) => (paths.data.clone(), t),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            bail!("the service's settings key is readable only by root: run it with sudo")
        }
        Err(_) => {
            let dir = crate::options::default_data_dir();
            let token =
                std::fs::read_to_string(dir.join(admin::TOKEN_FILE)).with_context(|| {
                    format!(
                        "no settings key in {} or {}: has the service (or `web-stream serve`) \
                     started yet?",
                        service.display(),
                        dir.display()
                    )
                })?;
            (dir, token)
        }
    };
    let port = Settings::load(&dir).map_or(8443, |s| s.port);
    println!(
        "Settings page (the key in it works like a password, keep it to yourself):\n\n  {}\n\n\
         Open it in a browser on this PC. From another computer, forward the port first:\n  \
         ssh -L {port}:localhost:{port} <this PC>\nthen open the same address there. A paired \
         device can also open https://<this PC>:{port}/admin and confirm.",
        super::admin_url(port, token.trim())
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_put_the_tunnel_first_and_skip_loopback() {
        let ips: Vec<IpAddr> = vec![
            "127.0.0.1".parse().unwrap(),
            "192.168.1.20".parse().unwrap(),
            "fd00::5".parse().unwrap(),
        ];
        assert_eq!(
            origins(8443, &ips, Some("https://pc.example.com/")),
            [
                "https://pc.example.com",
                "https://192.168.1.20:8443",
                "https://[fd00::5]:8443"
            ]
        );
    }
}
