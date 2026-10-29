//! A Cloudflare Tunnel for the web page and signaling: the host runs
//! cloudflared, which connects out to Cloudflare and passes the browser's
//! HTTPS requests back to the host's own port. Browsers see Cloudflare's
//! certificate, so there is nothing to click past, and the HTTPS port needs
//! no forwarding. The tunnel only carries HTTP and WebSockets: the media
//! still goes straight to this PC over UDP (or ICE-TCP), so its address is
//! learned with STUN ([`public_ipv4`]) and put in the offer.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::watch;
use ws_signaling::{Notice, SignalingHandle};

use crate::options::{TunnelKind, TunnelOptions};

/// After cloudflared exits: wait this long, doubling up to [`RETRY_MAX`].
const RETRY_FIRST: Duration = Duration::from_secs(2);
const RETRY_MAX: Duration = Duration::from_secs(60);
/// A run this long resets the wait.
const STABLE: Duration = Duration::from_secs(120);

/// How often the public address is looked up again.
const PUBLIC_IP_EVERY: Duration = Duration::from_secs(5 * 60);

/// Keeps cloudflared running for `opts` until dropped (which ends it), and
/// tells the host user the public address through `signaling`'s notices.
/// Also keeps this PC's public address, for the media.
pub(crate) struct Tunnel {
    cloudflared: tokio::task::JoinHandle<()>,
    lookups: tokio::task::JoinHandle<()>,
    public_ip: watch::Receiver<Option<IpAddr>>,
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.cloudflared.abort();
        self.lookups.abort();
    }
}

impl Tunnel {
    pub fn start(opts: TunnelOptions, port: u16, signaling: SignalingHandle) -> Self {
        let (public_tx, public_ip) = watch::channel(None);
        Self {
            cloudflared: tokio::spawn(run(opts, port, signaling)),
            lookups: tokio::spawn(async move {
                loop {
                    match public_ipv4().await {
                        Some(ip) => {
                            if *public_tx.borrow() != Some(ip) {
                                tracing::info!(%ip, "public address");
                            }
                            public_tx.send_replace(Some(ip));
                        }
                        None => tracing::warn!("could not learn the public address over STUN"),
                    }
                    tokio::time::sleep(PUBLIC_IP_EVERY).await;
                }
            }),
            public_ip,
        }
    }

    /// The public IPv4 address, once known: the browser's way in through a
    /// tunnel says nothing about it.
    pub fn public_ip(&self) -> watch::Receiver<Option<IpAddr>> {
        self.public_ip.clone()
    }
}

async fn run(opts: TunnelOptions, port: u16, signaling: SignalingHandle) {
    let mut wait = RETRY_FIRST;
    loop {
        let started = std::time::Instant::now();
        if let Err(e) = run_once(&opts, port, &signaling).await {
            signaling.notice(Notice::Warning(format!("Cloudflare Tunnel: {e}")));
        }
        signaling.notice(Notice::Tunnel { url: None });
        if started.elapsed() >= STABLE {
            wait = RETRY_FIRST;
        }
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(RETRY_MAX);
    }
}

/// Runs cloudflared once, until it exits.
async fn run_once(
    opts: &TunnelOptions,
    port: u16,
    signaling: &SignalingHandle,
) -> anyhow::Result<()> {
    let mut cmd = Command::new(&opts.cloudflared);
    // Its own output is all on stderr; it reads nothing.
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut hostname = match &opts.kind {
        TunnelKind::Named {
            token_file,
            hostname,
        } => {
            let token = std::fs::read_to_string(token_file)
                .map_err(|e| {
                    anyhow::anyhow!(
                        "no tunnel token in {} ({e}): paste it in the settings",
                        token_file.display()
                    )
                })?
                .trim()
                .to_string();
            anyhow::ensure!(
                !token.is_empty(),
                "the tunnel token in {} is empty",
                token_file.display()
            );
            // In the environment, not on the command line, where any
            // program on the PC could read it.
            cmd.env("TUNNEL_TOKEN", token)
                .args(["tunnel", "--no-autoupdate", "run"]);
            hostname.clone()
        }
        TunnelKind::Quick => {
            cmd.args(["tunnel", "--no-autoupdate", "--url"])
                .arg(format!("https://127.0.0.1:{port}"))
                // The host's own certificate is self-signed; cloudflared
                // reaches it on this PC only.
                .arg("--no-tls-verify");
            None
        }
    };
    let mut child = cmd.spawn().map_err(|e| {
        anyhow::anyhow!(
            "could not start {}: {e}. Reinstall web-stream, or turn the tunnel off",
            opts.cloudflared.display()
        )
    })?;
    let _job = job::contain(&child);
    let stderr = child.stderr.take().expect("stderr is piped");
    let mut lines = BufReader::new(stderr).lines();
    let mut announced = false;
    let mut last_error = None;
    while let Ok(Some(line)) = lines.next_line().await {
        tracing::debug!(target: "cloudflared", "{line}");
        if hostname.is_none() {
            hostname = match opts.kind {
                TunnelKind::Quick => quick_hostname(&line),
                TunnelKind::Named { .. } => configured_hostname(&line),
            };
        }
        if line.contains(" ERR ") {
            last_error = Some(line.clone());
        }
        if !announced && line.contains("Registered tunnel connection") {
            announced = true;
            let url = hostname.as_ref().map(|h| format!("https://{h}"));
            tracing::info!(url = url.as_deref().unwrap_or("?"), "Cloudflare Tunnel up");
            signaling.notice(Notice::Tunnel { url });
        } else if announced && hostname.is_some() && line.contains("Updated to new configuration") {
            // The name was changed in the dashboard.
            signaling.notice(Notice::Tunnel {
                url: hostname.as_ref().map(|h| format!("https://{h}")),
            });
        }
    }
    let status = child.wait().await?;
    match last_error {
        Some(e) => anyhow::bail!("cloudflared stopped ({status}): {}", short(&e)),
        None => anyhow::bail!("cloudflared stopped ({status})"),
    }
}

/// A log line without its timestamp and level.
fn short(line: &str) -> &str {
    line.split_once(" ERR ")
        .map_or(line, |(_, rest)| rest.trim())
}

/// The quick tunnel's name, from the line that announces it.
fn quick_hostname(line: &str) -> Option<String> {
    line.split(|c: char| c.is_whitespace() || c == '|')
        .filter_map(|word| word.strip_prefix("https://"))
        .find(|host| host.ends_with(".trycloudflare.com") && host_ok(host))
        .map(str::to_ascii_lowercase)
}

/// The first public name of a dashboard-made tunnel, from the
/// configuration cloudflared logs (`"hostname":"pc.example.com"`, quotes
/// escaped in the log).
fn configured_hostname(line: &str) -> Option<String> {
    if !line.contains("Updated to new configuration") {
        return None;
    }
    let plain = line.replace('\\', "");
    plain
        .split("\"hostname\":\"")
        .skip(1)
        .filter_map(|rest| rest.split('"').next())
        .find(|host| host_ok(host))
        .map(str::to_ascii_lowercase)
}

fn host_ok(host: &str) -> bool {
    !host.is_empty()
        && host.contains('.')
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
}

/// Where Cloudflare's STUN server is.
const STUN_SERVER: (&str, u16) = ("stun.cloudflare.com", 3478);
const STUN_MAGIC: u32 = 0x2112_A442;

/// This PC's public IPv4 address as a STUN server sees it, or `None` when
/// the server can't be reached in time.
pub(crate) async fn public_ipv4() -> Option<IpAddr> {
    let lookup = tokio::net::lookup_host(STUN_SERVER);
    let server = tokio::time::timeout(Duration::from_secs(2), lookup)
        .await
        .ok()?
        .ok()?
        .find(SocketAddr::is_ipv4)?;
    let socket = tokio::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .await
        .ok()?;
    let mut id = [0u8; 12];
    getrandom::fill(&mut id).ok()?;
    let mut request = Vec::with_capacity(20);
    request.extend_from_slice(&0x0001u16.to_be_bytes());
    request.extend_from_slice(&0u16.to_be_bytes());
    request.extend_from_slice(&STUN_MAGIC.to_be_bytes());
    request.extend_from_slice(&id);
    let mut buf = [0u8; 512];
    for _ in 0..3 {
        socket.send_to(&request, server).await.ok()?;
        let reply = tokio::time::timeout(Duration::from_millis(700), socket.recv_from(&mut buf));
        if let Ok(Ok((n, from))) = reply.await {
            if from == server {
                if let Some(ip) = mapped_address(&buf[..n], &id) {
                    return Some(IpAddr::V4(ip));
                }
            }
        }
    }
    None
}

/// The (XOR-)MAPPED-ADDRESS of a STUN Binding success response to `id`.
fn mapped_address(msg: &[u8], id: &[u8; 12]) -> Option<Ipv4Addr> {
    if msg.len() < 20
        || msg[0..2] != [0x01, 0x01]
        || msg[4..8] != STUN_MAGIC.to_be_bytes()
        || msg[8..20] != id[..]
    {
        return None;
    }
    let len = usize::from(u16::from_be_bytes([msg[2], msg[3]]));
    let attrs = msg.get(20..20 + len)?;
    let mut plain = None;
    let mut at = 0;
    while at + 4 <= attrs.len() {
        let kind = u16::from_be_bytes([attrs[at], attrs[at + 1]]);
        let size = usize::from(u16::from_be_bytes([attrs[at + 2], attrs[at + 3]]));
        let value = attrs.get(at + 4..at + 4 + size)?;
        // Family 1 is IPv4: reserved, family, port, address.
        if value.len() == 8 && value[1] == 1 {
            let raw = u32::from_be_bytes([value[4], value[5], value[6], value[7]]);
            match kind {
                0x0020 => return Some(Ipv4Addr::from(raw ^ STUN_MAGIC)),
                0x0001 => plain = Some(Ipv4Addr::from(raw)),
                _ => {}
            }
        }
        at += 4 + size.div_ceil(4) * 4;
    }
    plain
}

/// cloudflared is ended with the host even when the host is killed: on
/// Windows it goes in a job that closes with this process.
#[cfg(windows)]
mod job {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows::core::PCWSTR;

    pub struct Job(HANDLE);

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: the handle is this struct's own.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    pub fn contain(child: &tokio::process::Child) -> Option<Job> {
        let process = HANDLE(child.raw_handle()?);
        // SAFETY: an unnamed job, closed by `Job`; `process` is the child's
        // handle, which tokio keeps open while `child` lives.
        unsafe {
            let job = Job(CreateJobObjectW(None, PCWSTR::null()).ok()?);
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&raw const info).cast(),
                size_of_val(&info) as u32,
            )
            .ok()?;
            AssignProcessToJobObject(job.0, process).ok()?;
            Some(job)
        }
    }
}

#[cfg(not(windows))]
mod job {
    pub fn contain(_child: &tokio::process::Child) -> Option<()> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_quick_tunnel_name() {
        let line = "2026-10-29T13:00:00Z INF |  https://some-random-words.trycloudflare.com                       |";
        assert_eq!(
            quick_hostname(line).as_deref(),
            Some("some-random-words.trycloudflare.com")
        );
        assert_eq!(
            quick_hostname("INF Requesting new quick Tunnel on trycloudflare.com..."),
            None
        );
        assert_eq!(
            quick_hostname("INF |  https://evil.example/x.trycloudflare.com |"),
            None
        );
    }

    #[test]
    fn reads_the_named_tunnel_name() {
        let line = r#"2026-10-29T13:00:00Z INF Updated to new configuration config="{\"ingress\":[{\"hostname\":\"PC.example.com\",\"originRequest\":{\"noTLSVerify\":true},\"service\":\"https://localhost:8443\"},{\"service\":\"http_status:404\"}],\"warp-routing\":{\"enabled\":false}}" version=3"#;
        assert_eq!(configured_hostname(line).as_deref(), Some("pc.example.com"));
        assert_eq!(
            configured_hostname(
                r#"INF Updated to new configuration config="{\"ingress\":[{\"service\":\"http_status:404\"}]}""#
            ),
            None
        );
        assert_eq!(
            configured_hostname("INF Registered tunnel connection"),
            None
        );
    }

    #[test]
    fn reads_the_stun_answer() {
        let id = [7u8; 12];
        let ip = Ipv4Addr::new(203, 0, 113, 5);
        let mut msg = vec![0x01, 0x01, 0, 20];
        msg.extend_from_slice(&STUN_MAGIC.to_be_bytes());
        msg.extend_from_slice(&id);
        // SOFTWARE first (3 bytes, padded), then XOR-MAPPED-ADDRESS.
        msg.extend_from_slice(&[0x80, 0x22, 0, 3, b'a', b'b', b'c', 0]);
        msg.extend_from_slice(&[0x00, 0x20, 0, 8, 0, 1]);
        msg.extend_from_slice(&(8443u16 ^ 0x2112).to_be_bytes());
        msg.extend_from_slice(&(u32::from(ip) ^ STUN_MAGIC).to_be_bytes());
        assert_eq!(mapped_address(&msg, &id), Some(ip));
        assert_eq!(mapped_address(&msg, &[8u8; 12]), None);
        assert_eq!(mapped_address(&msg[..30], &id), None);
    }
}
