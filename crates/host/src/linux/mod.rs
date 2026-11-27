//! The headless Linux host: `web-stream serve`, the systemd service with its privilege split, the
//! settings page and the commands that control the service.

pub mod admin;
mod control;
mod qr;
pub mod settings;
mod supervise;
mod systemd;
mod upnp;

use std::io::{BufRead, IsTerminal};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use tokio::sync::mpsc;

use crate::console::{CONSOLE_HELP, Line, describe, pairing_link_line, parse_line};
use crate::options::{self, ServeOptions};
use crate::serve::{self, Command, Event};
use settings::Settings;

pub const USAGE: &str = "\
usage: web-stream <command>

  serve [--upnp] [host options]
                 run the host in the foreground as you, with the saved settings
                 and these options; type `help` for its console
  pair           let a device pair with the service for two minutes: prints the
                 address, a QR code and the code to type
  unpair-all     forget every device paired with the service
  settings       print the address of the service's settings page, with its key
  service install | uninstall [--purge] | start | stop | status
                 the systemd service (needs root): install copies this program
                 to /usr/local/lib/web-stream, creates the web-stream user, the
                 udev rule and the unit, and starts it
  version        print the version
";

/// Where the service keeps its program, data and sockets, and the user its network side runs as.
#[derive(Debug, Clone)]
pub struct Paths {
    pub lib: PathBuf,
    pub data: PathBuf,
    pub run: PathBuf,
    pub user: String,
}

impl Paths {
    /// The installed layout; `WEB_STREAM_*` variables move it for tests.
    pub fn get() -> Self {
        let var = |name: &str, default: &str| {
            std::env::var_os(name)
                .filter(|v| !v.is_empty())
                .map_or_else(|| PathBuf::from(default), PathBuf::from)
        };
        Self {
            lib: var("WEB_STREAM_LIB_DIR", "/usr/local/lib/web-stream"),
            data: var("WEB_STREAM_DATA_DIR", "/var/lib/web-stream"),
            run: var("WEB_STREAM_RUN_DIR", "/run/web-stream"),
            user: std::env::var("WEB_STREAM_USER")
                .ok()
                .filter(|u| !u.is_empty())
                .unwrap_or_else(|| "web-stream".into()),
        }
    }

    pub fn exe(&self) -> PathBuf {
        self.lib.join("web-stream")
    }

    pub fn network(&self) -> PathBuf {
        self.data.join("network")
    }

    pub fn control(&self) -> PathBuf {
        self.run.join("control.sock")
    }
}

/// Runs `web-stream <args>`.
pub fn main(args: Vec<String>) -> anyhow::Result<()> {
    let rest = args.get(1..).unwrap_or_default();
    match args.first().map(String::as_str) {
        Some("serve") => serve_foreground(rest),
        Some("pair") => control::pair(&Paths::get()),
        Some("unpair-all") => control::unpair_all(&Paths::get()),
        Some("settings") => control::settings_url(&Paths::get()),
        Some("service") => service(rest),
        Some("version" | "--version" | "-V") => {
            println!("{}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("-h" | "--help" | "help") | None => {
            print!("{USAGE}\n{}", options::USAGE);
            Ok(())
        }
        Some(other) => bail!("unknown command {other:?}\n\n{USAGE}"),
    }
}

fn service(args: &[String]) -> anyhow::Result<()> {
    let paths = Paths::get();
    let rest = args.get(1..).unwrap_or_default();
    match args.first().map(String::as_str) {
        Some("install") => systemd::install(&paths, rest),
        Some("uninstall") => systemd::uninstall(&paths, rest),
        Some("start") => systemd::systemctl(&["start", systemd::UNIT]),
        Some("stop") => systemd::systemctl(&["stop", systemd::UNIT]),
        Some("status") => systemd::status(&paths),
        // Started by systemd.
        Some("run") => {
            init_logging();
            supervise::run(&paths)
        }
        // Started by `service run` as the web-stream user.
        Some("network") => {
            init_logging();
            supervise::network(rest)
        }
        _ => {
            bail!("usage: web-stream service install | uninstall [--purge] | start | stop | status")
        }
    }
}

fn init_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,str0m=warn".into()),
        )
        .with_ansi(false)
        .without_time()
        .with_writer(std::io::stderr)
        .init();
}

/// Flags of this program's own before the host options: (UPnP on, paired devices may open the
/// settings page, the rest).
fn own_flags(args: &[String]) -> (bool, bool, Vec<String>) {
    let mut upnp = false;
    let mut remote_admin = true;
    let mut rest = Vec::new();
    for a in args {
        match a.as_str() {
            "--upnp" => upnp = true,
            "--no-remote-admin" => remote_admin = false,
            _ => rest.push(a.clone()),
        }
    }
    (upnp, remote_admin, rest)
}

/// The value after the last `flag` in `args`.
fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.windows(2)
        .rev()
        .find(|w| w[0] == flag)
        .map(|w| w[1].as_str())
}

/// The settings page's address with its key.
pub fn admin_url(port: u16, token: &str) -> String {
    format!("https://localhost:{port}/admin#key={token}")
}

/// `web-stream serve`: the whole host in this process, restarted in place when the settings page
/// saves.
fn serve_foreground(args: &[String]) -> anyhow::Result<()> {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!(
            "usage: web-stream serve [--upnp] [host options]\n\n{}",
            options::USAGE
        );
        return Ok(());
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,str0m=warn".into()),
        )
        .with_ansi(std::io::stdout().is_terminal())
        .init();
    let (upnp_flag, remote_admin_flag, cli) = own_flags(args);
    let dir = flag_value(&cli, "--data-dir").map_or_else(options::default_data_dir, PathBuf::from);
    // SAFETY: no arguments.
    if unsafe { libc::geteuid() } == 0 {
        tracing::warn!(
            "running the whole host as root: `web-stream service install` runs the part that \
             reads the network as an unprivileged user"
        );
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let (commands, mut commands_rx) = mpsc::channel(8);
        let (events_tx, events) = mpsc::unbounded_channel();
        let (restart_tx, mut restart_rx) = mpsc::unbounded_channel();
        tokio::spawn(print_events(events));
        console(commands.clone());
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                let _ = commands.send(Command::Quit).await;
            }
        });
        let mut upnp = None;
        loop {
            let saved = Settings::load(&dir)?;
            let mut full = saved.to_args(&dir);
            full.extend(["--data-dir".into(), dir.display().to_string()]);
            full.extend(cli.iter().cloned());
            let opts = ServeOptions::parse(&full)?;
            if upnp.is_none() && (saved.upnp || upnp_flag) {
                upnp = Some(upnp::start(Settings::forwarded_ports(&opts)));
            }
            let admin = admin::Admin::new(
                admin::Backend::Local {
                    dir: dir.clone(),
                    restart: restart_tx.clone(),
                },
                &opts,
                saved.remote_admin && remote_admin_flag,
                upnp.clone(),
                false,
            )?;
            let token = admin::load_or_create_token(&opts.data_dir)?;
            println!("settings page: {}", admin_url(opts.port, &token));
            let (run_tx, run_rx) = mpsc::channel(8);
            let host = serve::serve_with(opts, run_rx, events_tx.clone(), Some(Box::new(admin)));
            match drive(host, &run_tx, &mut commands_rx, &mut restart_rx).await {
                (_, true) => {
                    println!("restarting with the new settings");
                    continue;
                }
                (result, false) => return result,
            }
        }
    })
}

/// Runs `host`, passing it `commands`; a message on `restart` stops it. Returns its result and
/// whether a restart was asked for.
async fn drive(
    host: impl std::future::Future<Output = anyhow::Result<()>>,
    run: &mpsc::Sender<Command>,
    commands: &mut mpsc::Receiver<Command>,
    restart: &mut mpsc::UnboundedReceiver<()>,
) -> (anyhow::Result<()>, bool) {
    tokio::pin!(host);
    let mut restarting = false;
    loop {
        tokio::select! {
            r = &mut host => return (r, restarting),
            Some(cmd) = commands.recv() => {
                let _ = run.send(cmd).await;
            }
            Some(()) = restart.recv() => {
                restarting = true;
                let _ = run.send(Command::Quit).await;
            }
        }
    }
}

async fn print_events(mut events: mpsc::UnboundedReceiver<Event>) {
    let mut origin = None;
    while let Some(e) = events.recv().await {
        if let Event::Started { port, ips, .. } = &e {
            origin = control::origins(*port, ips, None).into_iter().next();
        }
        if let Some(line) = pairing_link_line(&e) {
            println!("{line}");
        }
        if let (Event::PairingOpened { link, .. }, Some(origin)) = (&e, &origin) {
            let url = format!("{origin}/#pair={link}");
            println!("  {url}\n{}", qr::text(&url).unwrap_or_default());
        }
        if let Some(text) = describe(e) {
            println!("{text}");
        }
    }
}

/// Reads commands typed on stdin.
fn console(commands: mpsc::Sender<Command>) {
    let spawned = std::thread::Builder::new()
        .name("ws-console".into())
        .spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                let cmd = match parse_line(&line) {
                    Line::Command(cmd) => cmd,
                    Line::Empty => continue,
                    Line::Help => {
                        println!("{CONSOLE_HELP}");
                        continue;
                    }
                };
                if commands.blocking_send(cmd).is_err() {
                    break;
                }
            }
        });
    if let Err(e) = spawned {
        tracing::warn!("no console: {e}");
    }
}

/// The user `name`: (uid, gid, home).
pub fn lookup_user(name: &str) -> anyhow::Result<Option<(u32, u32, PathBuf)>> {
    use std::ffi::{CStr, CString};
    use std::os::unix::ffi::OsStrExt;
    let cname = CString::new(name).context("user name")?;
    let mut buf = vec![0 as libc::c_char; 16 * 1024];
    // SAFETY: zeroed passwd is a valid out parameter.
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result = std::ptr::null_mut();
    // SAFETY: every pointer is valid for the call and `buf` outlives the use of `pwd`.
    let rc = unsafe {
        libc::getpwnam_r(
            cname.as_ptr(),
            &mut pwd,
            buf.as_mut_ptr(),
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::from_raw_os_error(rc)).context("getpwnam_r");
    }
    if result.is_null() {
        return Ok(None);
    }
    // SAFETY: getpwnam_r filled `pwd` with strings in `buf`.
    let home = unsafe { CStr::from_ptr(pwd.pw_dir) };
    let home = PathBuf::from(std::ffi::OsStr::from_bytes(home.to_bytes()));
    Ok(Some((pwd.pw_uid, pwd.pw_gid, home)))
}

/// Whether this process runs as root.
pub fn is_root() -> bool {
    // SAFETY: no arguments.
    unsafe { libc::geteuid() == 0 }
}

/// Fails unless this process runs as root.
fn require_root(what: &str) -> anyhow::Result<()> {
    if !is_root() {
        bail!("{what} needs root: run it with sudo");
    }
    Ok(())
}

/// Copies `from` over `to` through a temporary file beside it, mode 0755.
fn replace_file(from: &Path, to: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let tmp = to.with_extension("new");
    std::fs::copy(from, &tmp).with_context(|| format!("copy to {}", tmp.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&tmp, to).with_context(|| format!("{}", to.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn own_flags_come_off_the_host_options() {
        let (upnp, remote, rest) = own_flags(&args(&["--upnp", "--port", "9000"]));
        assert!(upnp && remote);
        assert_eq!(rest, args(&["--port", "9000"]));
        let (_, remote, _) = own_flags(&args(&["--no-remote-admin"]));
        assert!(!remote);
        let a = args(&["--data-dir", "a", "--data-dir", "b"]);
        assert_eq!(flag_value(&a, "--data-dir"), Some("b"));
        assert_eq!(flag_value(&a, "--port"), None);
    }

    #[test]
    fn paths() {
        let p = Paths {
            lib: "/l".into(),
            data: "/d".into(),
            run: "/r".into(),
            user: "u".into(),
        };
        assert_eq!(p.exe(), Path::new("/l/web-stream"));
        assert_eq!(p.network(), Path::new("/d/network"));
        assert_eq!(p.control(), Path::new("/r/control.sock"));
        assert_eq!(admin_url(8443, "k"), "https://localhost:8443/admin#key=k");
        assert_eq!(lookup_user("root").unwrap().unwrap().0, 0);
        assert!(lookup_user("no-such-user-here").unwrap().is_none());
    }
}
