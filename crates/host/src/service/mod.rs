//! The host as a Windows service, so a browser can also use the sign-in
//! and lock screens, UAC prompts and programs running as administrator.
//! The tray app's exe runs it: `ws-host-gui service ...` (see [`main`]),
//! started elevated from the tray app's settings.
//!
//! The service (LocalSystem, in session 0) only supervises. It runs the
//! host as two processes, linked by a pipe (see [`crate::split`]):
//!
//! - `<exe> service session-host` in the session on the PC's own screen, as
//!   SYSTEM with nearly every privilege removed: capture, encoding, input,
//!   audio. Running as SYSTEM on the user's screen is what lets capture and
//!   input follow Windows onto the secure desktop and into elevated windows.
//! - `<exe> service network-host` as LocalService with no privileges, in
//!   session 0: the signaling server and the peer connection, everything
//!   that parses what comes off the network.
//!
//! It starts both again when that session changes (signing out, switching
//! users) or either exits. If the network host can't be started, the
//! session host runs the whole host, as before, and the log says why. The
//! host has no console; elevated administrators control it through a named
//! pipe: the tray app, run elevated, attaches to it ([`control::attach`]).

// Off Windows only the tests use most of this.
#![cfg_attr(not(windows), allow(dead_code))]

use std::path::Path;
use std::time::Duration;

use crate::options::ServeOptions;
use anyhow::bail;

#[cfg(windows)]
pub mod control;
#[cfg(windows)]
mod launch;
#[cfg(windows)]
mod sas;
#[cfg(windows)]
pub(crate) mod scm;

pub const USAGE: &str = "\
service install [host options]
                 install (or reinstall) the host as a Windows service that
                 starts with Windows and streams the sign-in screen, UAC
                 prompts and programs run as administrator; copies this exe
                 to Program Files. Needs administrator rights.
service update   copy this exe over the installed one and restart the service
service start | stop
service uninstall
";

/// Serve options the service refuses, and why.
const REFUSED: &[(&str, &str)] = &[
    (
        "--cloudflared",
        "the service runs the cloudflared installed next to it: one a user can swap would \
         run their code as the service",
    ),
    (
        "--web-root",
        "the service serves the web client built into it: a folder a user can change would \
         run their code in a browser that controls this PC as SYSTEM",
    ),
    (
        "--data-dir",
        "the service keeps its certificate and paired browsers in SYSTEM's own folder",
    ),
    (
        "--allow-upload",
        "the service can't receive files yet: it would write them as SYSTEM into a folder \
         a user controls",
    ),
    ("--upload-dir", "the service can't receive files yet"),
    (
        "--allow-remote-update",
        "the service runs its own copy of the host from Program Files; update it on the PC \
         with scripts\\update.ps1",
    ),
    (
        "--pair",
        "it would open pairing every time the host starts; pair from the tray app's service \
         console",
    ),
];

/// Runs `service <args>`; what it did, for the administrator who asked
/// (empty for the service and the session host, which run until stopped).
pub fn main(args: &[String]) -> anyhow::Result<String> {
    let rest = args.get(1..).unwrap_or_default();
    match args.first().map(String::as_str) {
        Some("install") => imp::install(rest),
        Some("update") => imp::update(),
        Some("uninstall") => imp::uninstall(),
        Some("start") => imp::start(),
        Some("stop") => imp::stop(),
        // Started by the service control manager.
        Some("run") => imp::run(rest).map(|()| String::new()),
        // Started by the service in the user's session.
        Some("session-host") => imp::session_host(rest).map(|()| String::new()),
        // Started by the service as LocalService.
        Some("network-host") => imp::network_host(rest).map(|()| String::new()),
        Some("-h" | "--help") | None => Ok(USAGE.into()),
        Some(other) => bail!("unknown service command {other:?}\n\n{USAGE}"),
    }
}

/// Logging for `service <args>`: the service writes to service.log (it has
/// no stdout); the session host's stdout is its host.log, the network
/// host's network.log.
pub fn init_logging(args: &[String]) {
    let logs = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,str0m=warn".into()),
        )
        .with_ansi(false);
    #[cfg(windows)]
    if args.first().is_some_and(|a| a == "run") {
        if let Some(file) = service_log() {
            return logs.with_writer(std::sync::Mutex::new(file)).init();
        }
    }
    #[cfg(not(windows))]
    let _ = args;
    logs.init();
}

/// Whether the service is installed, and if so whether it runs. Needs no
/// administrator rights.
pub fn query() -> anyhow::Result<Option<bool>> {
    imp::query()
}

/// The control pipe's name starts with this; the rest is random, picked by
/// the service each time it starts the host and written to [`PIPE_FILE`]
/// in its data folder, which only administrators can read. A program that
/// can't read it can't take the name before the host does.
pub const PIPE_PREFIX: &str = r"\\.\pipe\web-stream-host-";
pub const PIPE_FILE: &str = "control-pipe.txt";

/// A host that ran at least this long was healthy: restart it right away.
pub const HEALTHY_RUN: Duration = Duration::from_secs(30);
pub const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// How long to wait before starting the host again, after waiting `prev`
/// last time for a host that then ran for `ran`: one second after a
/// healthy run, doubling up to [`MAX_BACKOFF`] while it keeps failing.
pub fn next_backoff(prev: Duration, ran: Duration) -> Duration {
    if ran >= HEALTHY_RUN {
        Duration::from_secs(1)
    } else {
        (prev * 2).clamp(Duration::from_secs(1), MAX_BACKOFF)
    }
}

/// 32 random hex digits.
pub fn random_hex() -> anyhow::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("random name: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// A fresh control pipe name.
pub fn random_pipe_name() -> anyhow::Result<String> {
    Ok(format!("{PIPE_PREFIX}{}", random_hex()?))
}

/// The session host's arguments (after the exe), as the service builds
/// them: the pipe, the Ctrl+Alt+Del event handle and the link to the
/// network host (0: none, run the whole host) it inherits, the installed
/// serve options, and the host's data folder.
pub fn session_host_args(
    pipe: &str,
    sas_event: usize,
    link: usize,
    serve_args: &[String],
    data_dir: &Path,
) -> Vec<String> {
    let mut args = vec![
        "service".into(),
        "session-host".into(),
        pipe.into(),
        sas_event.to_string(),
        link.to_string(),
    ];
    args.extend(serve_args.iter().cloned());
    args.push("--data-dir".into());
    args.push(data_dir.display().to_string());
    args
}

/// The network host's arguments: the link and the alive pipe (see
/// `launch::alive_pipe`) it inherits, and as for [`session_host_args`].
pub fn network_host_args(
    link: usize,
    alive: usize,
    serve_args: &[String],
    data_dir: &Path,
) -> Vec<String> {
    let mut args = vec![
        "service".into(),
        "network-host".into(),
        link.to_string(),
        alive.to_string(),
    ];
    args.extend(serve_args.iter().cloned());
    args.push("--data-dir".into());
    args.push(data_dir.display().to_string());
    args
}

/// What the session host is started with.
#[derive(Debug)]
pub struct SessionHost {
    pub pipe: String,
    /// The inherited event the host sets to ask the service for
    /// Ctrl+Alt+Del; 0 for none.
    pub sas_event: usize,
    /// The inherited end of the pipe to the network host; 0 for none.
    pub link: usize,
    pub opts: ServeOptions,
}

/// Parses what follows `service session-host`, checking it.
pub fn parse_session_host_args(args: &[String]) -> anyhow::Result<SessionHost> {
    let [pipe, sas_event, link, rest @ ..] = args else {
        bail!("session-host needs a pipe name and two handles");
    };
    if !valid_pipe_name(pipe) {
        bail!("bad control pipe name {pipe:?}");
    }
    let opts = ServeOptions::parse(rest)?;
    check_session_host_options(&opts)?;
    Ok(SessionHost {
        pipe: pipe.clone(),
        sas_event: parse_handle(sas_event)?,
        link: parse_handle(link)?,
        opts,
    })
}

/// Parses what follows `service network-host`: the link and the options.
pub fn parse_network_host_args(args: &[String]) -> anyhow::Result<(usize, usize, ServeOptions)> {
    let [link, alive, rest @ ..] = args else {
        bail!("network-host needs two handles");
    };
    let (link, alive) = (parse_handle(link)?, parse_handle(alive)?);
    if link == 0 || alive == 0 {
        bail!("network-host needs its link to the session host and its alive pipe");
    }
    let opts = ServeOptions::parse(rest)?;
    check_session_host_options(&opts)?;
    Ok((link, alive, opts))
}

fn parse_handle(text: &str) -> anyhow::Result<usize> {
    text.parse()
        .map_err(|_| anyhow::anyhow!("bad handle {text:?}"))
}

/// Files the network host keeps, which the service kept in its own data
/// folder before the host was split: moved into the network host's folder.
pub const NETWORK_FILES: &[&str] = &[
    "tls-cert.pem",
    "tls-key.pem",
    "tls-cert.json",
    "devices.json",
    "pairing.json",
    CERT_COPY,
    KEY_COPY,
    TOKEN_COPY,
];

/// The service's copies of `--cert`, `--key` and `--tunnel-token-file`.
pub const CERT_COPY: &str = "service-cert.pem";
pub const KEY_COPY: &str = "service-key.pem";
pub const TOKEN_COPY: &str = "service-tunnel-token.txt";

/// Points the file flags that name a file directly in `from` (an install
/// from before the split) at the same file in `to`.
pub fn relocate_files(args: &[String], from: &Path, to: &Path) -> Vec<String> {
    let mut out = args.to_vec();
    for i in 1..out.len() {
        if !matches!(
            out[i - 1].as_str(),
            "--cert" | "--key" | "--tunnel-token-file"
        ) {
            continue;
        }
        let path = Path::new(&out[i]);
        if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
            if parent == from {
                out[i] = to.join(name).display().to_string();
            }
        }
    }
    out
}

pub fn valid_pipe_name(name: &str) -> bool {
    name.strip_prefix(PIPE_PREFIX)
        .is_some_and(|rest| rest.len() == 32 && rest.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Replaces the value after every `flag` with `value` (the parser keeps
/// the last one, so none may be left behind).
pub fn replace_value(args: &[String], flag: &str, value: &str) -> Vec<String> {
    let mut out = args.to_vec();
    let mut i = 0;
    while i + 1 < out.len() {
        if out[i] == flag {
            out[i + 1] = value.to_string();
            i += 1;
        }
        i += 1;
    }
    out
}

/// What [`PIPE_FILE`] holds: the pipe's name and the host process serving
/// it, so a console can check it talks to the host itself.
pub fn pipe_file_contents(pipe: &str, pid: u32) -> String {
    format!("{pipe}\n{pid}\n")
}

pub fn parse_pipe_file(text: &str) -> Option<(String, u32)> {
    let mut lines = text.lines();
    let pipe = lines.next()?.trim();
    let pid = lines.next()?.trim().parse().ok()?;
    valid_pipe_name(pipe).then(|| (pipe.to_string(), pid))
}

/// Checks the serve options given to `service install`.
pub fn check_install_args(args: &[String]) -> anyhow::Result<ServeOptions> {
    for arg in args {
        if let Some((flag, why)) = REFUSED.iter().find(|(f, _)| f == arg) {
            bail!("{flag} can't be used with the service: {why}");
        }
    }
    ServeOptions::parse(args)
}

/// Checks the options the service hands the session host, which adds
/// `--data-dir` itself.
pub fn check_session_host_options(opts: &ServeOptions) -> anyhow::Result<()> {
    if opts.web_root.is_some() || opts.media.upload_dir.is_some() {
        bail!("the service's host never serves a web root or receives files");
    }
    if opts
        .tunnel
        .as_ref()
        .is_some_and(|t| t.cloudflared != crate::options::default_cloudflared())
    {
        bail!("the service only runs the cloudflared installed next to it");
    }
    // Refused at install too; checked again in case the service's command
    // line was edited.
    if opts.media.update_log.is_some() || opts.pair_at_start {
        bail!("the service's host never updates from the browser or pairs at start");
    }
    Ok(())
}

/// Quotes one argument for a Windows command line the way
/// `CommandLineToArgvW` and the C runtime split it.
pub fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '\u{b}', '"']) {
        return arg.to_string();
    }
    let mut out = String::from('"');
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                // Escape the backslashes before a quote, and the quote.
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            c => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    // Backslashes before the closing quote are doubled.
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

#[cfg(windows)]
pub use scm::service_log;

#[cfg(windows)]
mod imp {
    pub use super::scm::{install, query, run, start, stop, uninstall, update};

    /// Running as SYSTEM, load DLLs only from System32 and the exe's
    /// folder, never the current directory or PATH.
    pub fn harden_dll_search() {
        use windows::Win32::System::LibraryLoader::{
            LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, SetDefaultDllDirectories,
        };
        // SAFETY: no pointer arguments.
        if let Err(e) = unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_DEFAULT_DIRS) } {
            tracing::warn!("SetDefaultDllDirectories: {e}");
        }
    }

    pub fn session_host(args: &[String]) -> anyhow::Result<()> {
        harden_dll_search();
        if !ws_desktop::running_as_system() {
            anyhow::bail!("session-host is started by the web-stream service, not by hand");
        }
        let host = super::parse_session_host_args(args)?;
        if host.sas_event != 0 {
            let event = host.sas_event;
            crate::sas::install(move || super::sas::request(event));
        }
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(super::control::serve(host.pipe, host.link, host.opts))
    }

    pub fn network_host(args: &[String]) -> anyhow::Result<()> {
        harden_dll_search();
        // The point of this process is that it isn't SYSTEM.
        if ws_desktop::running_as_system() {
            anyhow::bail!("network-host must not run as SYSTEM");
        }
        let (link, alive, opts) = super::parse_network_host_args(args)?;
        // Before anything from the network can reach this process: tells the
        // service its code ran (see `scm::STARTUP_FAILED`).
        {
            use std::io::Write;
            use std::os::windows::io::FromRawHandle;
            // SAFETY: the service handed this process the pipe's write end,
            // and nothing else here owns it.
            let mut alive = unsafe { std::fs::File::from_raw_handle(alive as *mut _) };
            alive
                .write_all(&[1])
                .map_err(|e| anyhow::anyhow!("write the alive pipe: {e}"))?;
        }
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(async move {
                // SAFETY: the service handed this process the pipe's client
                // end, overlapped, and nothing else here owns it.
                let link = unsafe {
                    tokio::net::windows::named_pipe::NamedPipeClient::from_raw_handle(
                        link as *mut std::ffi::c_void,
                    )
                }?;
                crate::split::serve_network(opts, link).await
            })
    }
}

#[cfg(not(windows))]
mod imp {
    fn windows_only() -> anyhow::Result<()> {
        anyhow::bail!("the host service is Windows only")
    }
    pub fn install(args: &[String]) -> anyhow::Result<String> {
        super::check_install_args(args)?;
        windows_only().map(|()| String::new())
    }
    pub fn update() -> anyhow::Result<String> {
        windows_only().map(|()| String::new())
    }
    pub fn uninstall() -> anyhow::Result<String> {
        windows_only().map(|()| String::new())
    }
    pub fn start() -> anyhow::Result<String> {
        windows_only().map(|()| String::new())
    }
    pub fn stop() -> anyhow::Result<String> {
        windows_only().map(|()| String::new())
    }
    pub fn query() -> anyhow::Result<Option<bool>> {
        Ok(None)
    }
    pub fn run(_: &[String]) -> anyhow::Result<()> {
        windows_only()
    }
    pub fn session_host(_: &[String]) -> anyhow::Result<()> {
        windows_only()
    }
    pub fn network_host(_: &[String]) -> anyhow::Result<()> {
        windows_only()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn install_refuses_what_would_hand_out_system() {
        assert!(check_install_args(&args(&["--pair"])).is_err());
        for flag in ["--web-root", "--data-dir", "--upload-dir"] {
            let e = check_install_args(&args(&[flag, "x"])).unwrap_err();
            assert!(e.to_string().contains(flag), "{e}");
        }
        assert!(check_install_args(&args(&["--allow-upload"])).is_err());
        let swapped = args(&["--tunnel", "quick", "--cloudflared", "x.exe"]);
        assert!(check_install_args(&swapped).is_err());
        let o = ServeOptions::parse(&swapped).unwrap();
        assert!(check_session_host_options(&o).is_err());
        let o = check_install_args(&args(&["--tunnel", "quick"])).unwrap();
        assert!(check_session_host_options(&o).is_ok());
        assert!(check_install_args(&args(&["--bogus"])).is_err());
        let o = check_install_args(&args(&["--port", "9000", "--codec", "av1"])).unwrap();
        assert_eq!(o.port, 9000);
        assert!(check_session_host_options(&o).is_ok());
        for refused in [
            &["--allow-upload", "--upload-dir", "x"][..],
            &["--allow-remote-update"],
            &["--pair"],
        ] {
            let o = ServeOptions::parse(&args(refused)).unwrap();
            assert!(check_session_host_options(&o).is_err(), "{refused:?}");
        }
    }

    #[test]
    fn session_host_args_round_trip() {
        let pipe = random_pipe_name().unwrap();
        assert!(valid_pipe_name(&pipe));
        let dir = Path::new(r"C:\Windows\System32\config\systemprofile\App Data\web-stream");
        let serve = args(&["--port", "9000", "--codec", "av1"]);
        let built = session_host_args(&pipe, 0x1a4, 0x2b8, &serve, dir);
        assert_eq!(&built[..2], ["service", "session-host"]);
        let host = parse_session_host_args(&built[2..]).unwrap();
        assert_eq!(host.pipe, pipe);
        assert_eq!(host.sas_event, 0x1a4);
        assert_eq!(host.link, 0x2b8);
        assert_eq!(host.opts.port, 9000);
        assert_eq!(host.opts.data_dir, dir);
        assert!(parse_session_host_args(&args(&[&pipe, "x", "0"])).is_err());
        assert!(parse_session_host_args(&args(&[&pipe, "0"])).is_err());

        assert!(parse_session_host_args(&[]).is_err());
        let squat = format!("{PIPE_PREFIX}x");
        assert!(parse_session_host_args(&args(&[&squat, "0", "0"])).is_err());
        assert!(!valid_pipe_name(r"\\.\pipe\other"));
        let uploads = session_host_args(&pipe, 0, 0, &args(&["--allow-upload"]), dir);
        assert!(parse_session_host_args(&uploads[2..]).is_err());

        let built = network_host_args(0x2bc, 0x2c0, &serve, dir);
        assert_eq!(&built[..2], ["service", "network-host"]);
        let (link, alive, opts) = parse_network_host_args(&built[2..]).unwrap();
        assert_eq!((link, alive), (0x2bc, 0x2c0));
        assert_eq!((opts.port, &*opts.data_dir), (9000, dir));
        assert!(parse_network_host_args(&network_host_args(0, 1, &serve, dir)[2..]).is_err());
        assert!(parse_network_host_args(&network_host_args(1, 0, &serve, dir)[2..]).is_err());
        let uploads = network_host_args(1, 1, &args(&["--allow-upload"]), dir);
        assert!(parse_network_host_args(&uploads[2..]).is_err());
    }

    #[test]
    fn files_from_before_the_split_are_found_in_the_network_folder() {
        let old = Path::new("data");
        let new = old.join("network");
        let other = Path::new("keys").join("k.pem");
        let in_old = |name| old.join(name).display().to_string();
        let a = vec![
            "--cert".into(),
            in_old(CERT_COPY),
            "--key".into(),
            other.display().to_string(),
            "--tunnel-token-file".into(),
            in_old(TOKEN_COPY),
            "--web-root".into(),
            in_old("x"),
        ];
        let moved = relocate_files(&a, old, &new);
        assert_eq!(moved[1], new.join(CERT_COPY).display().to_string());
        assert_eq!(moved[3], other.display().to_string());
        assert_eq!(moved[5], new.join(TOKEN_COPY).display().to_string());
        assert_eq!(moved[7], in_old("x"));
    }

    #[test]
    fn backoff() {
        let mut b = Duration::ZERO;
        let quick = Duration::from_secs(2);
        let mut seen = Vec::new();
        for _ in 0..8 {
            b = next_backoff(b, quick);
            seen.push(b.as_secs());
        }
        assert_eq!(seen, [1, 2, 4, 8, 16, 32, 60, 60]);
        assert_eq!(
            next_backoff(MAX_BACKOFF, HEALTHY_RUN),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn replacing_a_value() {
        let a = args(&["--cert", "a.pem", "--key", "b.pem"]);
        assert_eq!(
            replace_value(&a, "--key", "c.pem"),
            args(&["--cert", "a.pem", "--key", "c.pem"])
        );
        assert_eq!(replace_value(&a, "--port", "1"), a);
        let twice = args(&["--cert", "a", "--cert", "b", "--key", "k"]);
        assert_eq!(
            replace_value(&twice, "--cert", "c"),
            args(&["--cert", "c", "--cert", "c", "--key", "k"])
        );
    }

    #[test]
    fn pipe_file() {
        let pipe = random_pipe_name().unwrap();
        let text = pipe_file_contents(&pipe, 4242);
        assert_eq!(parse_pipe_file(&text), Some((pipe.clone(), 4242)));
        assert_eq!(parse_pipe_file(&pipe), None);
        assert_eq!(parse_pipe_file("\\\\.\\pipe\\fake\n1\n"), None);
    }

    #[test]
    fn quoting() {
        assert_eq!(quote_arg("plain"), "plain");
        assert_eq!(quote_arg(""), r#""""#);
        assert_eq!(quote_arg(r"C:\Program Files\x"), r#""C:\Program Files\x""#);
        assert_eq!(quote_arg(r"C:\a b\"), r#""C:\a b\\""#);
        assert_eq!(quote_arg(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(quote_arg(r#"a\"b"#), r#""a\\\"b""#);
        assert_eq!(quote_arg(r"no\space"), r"no\space");
    }
}
