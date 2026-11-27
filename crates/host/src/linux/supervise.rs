//! The service's two processes: the media side as root (`service run`), which starts the network
//! side as the unprivileged web-stream user (`service network`) with a socket pair to it, restarts
//! both when the settings change or the network side fails, and serves the control socket.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{broadcast, mpsc};

use super::admin::{self, AdminReply, AdminRequest};
use super::settings::Settings;
use super::{Paths, lookup_user, own_flags, require_root, upnp};
use crate::console::{self, describe};
use crate::options::ServeOptions;
use crate::serve::{Command, Event};
use crate::service::next_backoff;

/// The network side's end of the admin link.
const ADMIN_FD: i32 = 3;

/// `service run`: the media side, until SIGTERM.
pub fn run(paths: &Paths) -> anyhow::Result<()> {
    require_root("the service")?;
    let (uid, gid, _) = lookup_user(&paths.user)?.with_context(|| {
        format!(
            "there is no {} user: run `web-stream service install`",
            paths.user
        )
    })?;
    if uid == 0 {
        bail!("the {} user must not be root", paths.user);
    }
    prepare_dirs(paths, uid, gid)?;
    let exe = std::env::current_exe().context("this program's path")?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(supervise(paths, &exe, uid, gid))
}

/// The data dir (root's), the network side's dir in it (the web-stream user's) and the socket dir.
fn prepare_dirs(paths: &Paths, uid: u32, gid: u32) -> anyhow::Result<()> {
    let mode = |p: &Path, m: u32| std::fs::set_permissions(p, std::fs::Permissions::from_mode(m));
    std::fs::create_dir_all(&paths.data)?;
    mode(&paths.data, 0o755)?;
    let network = paths.network();
    if !network.exists() {
        std::fs::create_dir(&network)?;
        mode(&network, 0o700)?;
    }
    chown_tree(&network, uid, gid).with_context(|| format!("{}", network.display()))?;
    std::fs::create_dir_all(&paths.run)?;
    mode(&paths.run, 0o700)?;
    Ok(())
}

/// Hands `dir` and everything in it to `uid`, never following a link.
fn chown_tree(dir: &Path, uid: u32, gid: u32) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(dir)?;
    if meta.uid() != uid || meta.gid() != gid {
        std::os::unix::fs::lchown(dir, Some(uid), Some(gid))?;
    }
    if meta.is_dir() {
        for entry in std::fs::read_dir(dir)? {
            chown_tree(&entry?.path(), uid, gid)?;
        }
    }
    Ok(())
}

async fn supervise(paths: &Paths, exe: &Path, uid: u32, gid: u32) -> anyhow::Result<()> {
    let (commands_tx, mut commands) = mpsc::channel::<Command>(8);
    let (events_tx, mut events) = mpsc::unbounded_channel::<Event>();
    let (restart_tx, mut restart) = mpsc::unbounded_channel::<()>();
    let (sent, _) = broadcast::channel::<Event>(256);
    let snapshot = Arc::new(Mutex::new(console::Snapshot::default()));

    let listener = control_socket(&paths.control())?;
    tokio::spawn(accept(
        listener,
        commands_tx,
        sent.clone(),
        snapshot.clone(),
    ));
    tokio::spawn(async move {
        while let Some(e) = events.recv().await {
            if let Some(text) = describe(e.clone()) {
                tracing::info!("{text}");
            }
            snapshot.lock().unwrap().update(&e);
            let _ = sent.send(e);
        }
    });

    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let mut backoff = Duration::ZERO;
    loop {
        let settings = Settings::load(&paths.data).unwrap_or_else(|e| {
            tracing::error!("{e:#}; using the default settings");
            Settings::default()
        });
        let serve_args = settings.to_args(&paths.data);
        let mut media_args = serve_args.clone();
        media_args.extend(["--data-dir".into(), paths.data.display().to_string()]);
        let opts = match ServeOptions::parse(&media_args) {
            Ok(o) => o,
            Err(e) => {
                tracing::error!("{e:#}; using the default settings");
                Settings::default().serve_options(&paths.data)?
            }
        };
        let mut network_args = vec!["service".to_string(), "network".into()];
        if settings.upnp {
            network_args.push("--upnp".into());
        }
        if !settings.remote_admin {
            network_args.push("--no-remote-admin".into());
        }
        network_args.extend(serve_args);
        network_args.extend(["--data-dir".into(), paths.network().display().to_string()]);

        let started = Instant::now();
        let (bridge, bridge_child) = StdUnixStream::pair()?;
        let (admin_link, admin_child) = StdUnixStream::pair()?;
        let mut child = spawn_network(
            exe,
            &network_args,
            bridge_child,
            admin_child,
            uid,
            gid,
            paths,
        )
        .context("start the network side")?;
        tracing::info!(pid = child.id(), "network side started as {}", paths.user);
        bridge.set_nonblocking(true)?;
        admin_link.set_nonblocking(true)?;
        let bridge = UnixStream::from_std(bridge)?;
        let admin_task = tokio::spawn(serve_admin(
            UnixStream::from_std(admin_link)?,
            paths.data.clone(),
            restart_tx.clone(),
        ));

        let (run_tx, run_rx) = mpsc::channel(8);
        let media = crate::split::serve_media(opts, bridge, run_rx, events_tx.clone());
        tokio::pin!(media);
        let mut why = Stop::Ended;
        let result = loop {
            tokio::select! {
                r = &mut media => break r,
                Some(cmd) = commands.recv() => {
                    let _ = run_tx.send(cmd).await;
                }
                Some(()) = restart.recv() => {
                    why = Stop::Restart;
                    let _ = run_tx.send(Command::Quit).await;
                }
                _ = term.recv() => {
                    why = Stop::Exit;
                    let _ = run_tx.send(Command::Quit).await;
                }
                _ = int.recv() => {
                    why = Stop::Exit;
                    let _ = run_tx.send(Command::Quit).await;
                }
            }
        };
        admin_task.abort();
        if tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .is_err()
        {
            tracing::warn!("the network side did not stop; killing it");
            let _ = child.kill().await;
        }
        match (why, result) {
            (Stop::Exit, _) => {
                tracing::info!("stopped");
                let _ = std::fs::remove_file(paths.control());
                return Ok(());
            }
            (Stop::Restart, _) => backoff = Duration::ZERO,
            (Stop::Ended, Ok(())) => backoff = Duration::ZERO,
            (Stop::Ended, Err(e)) => {
                backoff = next_backoff(backoff, started.elapsed());
                tracing::warn!("{e:#}; starting again in {}s", backoff.as_secs());
                tokio::select! {
                    () = tokio::time::sleep(backoff) => {}
                    _ = term.recv() => return Ok(()),
                    _ = int.recv() => return Ok(()),
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Stop {
    Ended,
    Restart,
    Exit,
}

/// The control socket, for root only.
fn control_socket(path: &Path) -> anyhow::Result<UnixListener> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path).with_context(|| format!("{}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

async fn accept(
    listener: UnixListener,
    commands: mpsc::Sender<Command>,
    sent: broadcast::Sender<Event>,
    snapshot: Arc<Mutex<console::Snapshot>>,
) {
    loop {
        let stream = match listener.accept().await {
            Ok((s, _)) => s,
            Err(e) => {
                tracing::warn!("control socket: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let (greeting, events) = {
            let snapshot = snapshot.lock().unwrap();
            (snapshot.events(), sent.subscribe())
        };
        tokio::spawn(console::session(stream, commands.clone(), events, greeting));
    }
}

/// Answers the settings page's requests from the network side.
async fn serve_admin(
    link: UnixStream,
    dir: std::path::PathBuf,
    restart: mpsc::UnboundedSender<()>,
) {
    let (read, mut write) = link.into_split();
    let mut read = BufReader::new(read);
    loop {
        let line = match admin::read_line(&mut read).await {
            Ok(Some(l)) => l,
            Ok(None) => return,
            Err(e) => return tracing::warn!("settings link: {e:#}"),
        };
        let reply = match serde_json::from_str::<AdminRequest>(&line) {
            Ok(req) => {
                let dir = dir.clone();
                let restart = restart.clone();
                match tokio::task::spawn_blocking(move || admin::handle(&dir, req, &restart)).await
                {
                    Ok(r) => r,
                    Err(e) => AdminReply::Error(e.to_string()),
                }
            }
            Err(e) => AdminReply::Error(format!("bad request: {e}")),
        };
        let mut out = serde_json::to_vec(&reply).expect("replies serialize");
        out.push(b'\n');
        if write.write_all(&out).await.is_err() {
            return;
        }
    }
}

/// Starts `exe args` as `uid`:`gid`, no supplementary groups and no way back to more rights, its
/// stdin `bridge` and fd 3 `admin`.
fn spawn_network(
    exe: &Path,
    args: &[String],
    bridge: StdUnixStream,
    admin: StdUnixStream,
    uid: u32,
    gid: u32,
    paths: &Paths,
) -> std::io::Result<tokio::process::Child> {
    let admin_fd = admin.as_raw_fd();
    let mut cmd = tokio::process::Command::new(exe);
    cmd.args(args)
        .env_clear()
        .env("HOME", paths.network())
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("LANG", "C.UTF-8")
        .current_dir(paths.network())
        .stdin(Stdio::from(OwnedFd::from(bridge)))
        .uid(uid)
        .gid(gid)
        .kill_on_drop(true);
    if let Ok(filter) = std::env::var("RUST_LOG") {
        cmd.env("RUST_LOG", filter);
    }
    // SAFETY: the closure runs in the child between fork and exec, after the uid change, and only
    // makes async-signal-safe calls.
    unsafe {
        cmd.pre_exec(move || {
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
                || libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM, 0, 0, 0) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            if admin_fd == ADMIN_FD {
                if libc::fcntl(ADMIN_FD, libc::F_SETFD, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            } else if libc::dup2(admin_fd, ADMIN_FD) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn();
    drop(admin);
    child
}

/// The effective capabilities of this process, from `/proc/self/status`.
fn effective_caps() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find_map(|l| l.strip_prefix("CapEff:"))?;
    u64::from_str_radix(line.trim(), 16).ok()
}

/// `service network`: the network side, on the links `service run` hands it.
pub fn network(args: &[String]) -> anyhow::Result<()> {
    // SAFETY: no arguments.
    let (uid, euid) = unsafe { (libc::getuid(), libc::geteuid()) };
    if uid == 0 || euid == 0 {
        bail!("the network side must not run as root");
    }
    if effective_caps() != Some(0) {
        bail!("the network side must run without capabilities");
    }
    let (upnp_on, remote_admin, rest) = own_flags(args);
    let opts = ServeOptions::parse(&rest)?;
    if opts.web_root.is_some() || opts.media.update_log.is_some() || opts.pair_at_start {
        bail!("the service's network side never serves a web root, updates or pairs at start");
    }
    let upnp = upnp_on.then(|| upnp::start(Settings::forwarded_ports(&opts)));
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            // SAFETY: `service run` handed this process the bridge as stdin and the admin link as
            // fd 3, and nothing else here uses them.
            let (bridge, admin_link) = unsafe {
                (
                    StdUnixStream::from_raw_fd(0),
                    StdUnixStream::from_raw_fd(ADMIN_FD),
                )
            };
            bridge.set_nonblocking(true)?;
            admin_link.set_nonblocking(true)?;
            let backend = admin::Backend::remote(UnixStream::from_std(admin_link)?);
            let page = admin::Admin::new(backend, &opts, remote_admin, upnp, true)?;
            crate::split::serve_network_with(
                opts,
                UnixStream::from_std(bridge)?,
                Some(Box::new(page)),
            )
            .await
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chown_tree_leaves_links_alone() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(dir.path().join("a"), b"x").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        let me = std::fs::metadata(dir.path()).unwrap();
        chown_tree(dir.path(), me.uid(), me.gid()).unwrap();
        assert_eq!(effective_caps().is_some(), cfg!(target_os = "linux"));
    }
}
