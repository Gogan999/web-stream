//! `ws-host service`: the host as a Windows service, so a browser can also
//! use the sign-in and lock screens, UAC prompts and programs running as
//! administrator.
//!
//! The service (LocalSystem, in session 0) only supervises. It starts
//! `ws-host service session-host` in the session on the PC's own screen,
//! as SYSTEM with nearly every privilege removed, and starts it again when
//! that session changes (signing out, switching users) or the host exits.
//! Running as SYSTEM on the user's screen is what lets capture and input
//! follow Windows onto the secure desktop and into elevated windows. The
//! host has no console; elevated administrators control it through a named
//! pipe (`ws-host service pair`, `ws-host service console`).

// Off Windows only the tests use most of this.
#![cfg_attr(not(windows), allow(dead_code))]

use std::path::Path;
use std::time::Duration;

use anyhow::bail;
use ws_host::options::ServeOptions;

#[cfg(windows)]
mod control;
#[cfg(windows)]
mod launch;
#[cfg(windows)]
pub(crate) mod scm;

pub const USAGE: &str = "\
ws-host service install [serve options]
                       install (or reinstall) the host as a Windows service that
                       starts with Windows and streams the sign-in screen, UAC
                       prompts and programs run as administrator; copies this
                       ws-host.exe to Program Files. Administrator prompt.
ws-host service update copy this ws-host.exe over the installed one and restart
ws-host service uninstall
ws-host service pair   open pairing and show the code (administrator prompt)
ws-host service console  type host commands (pair, drop, unpair-all, send <path>)
";

/// Serve options the service refuses, and why.
const REFUSED: &[(&str, &str)] = &[
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
        "--pair",
        "it would open pairing every time the host starts; use `ws-host service pair`",
    ),
];

pub fn main(args: &[String]) -> anyhow::Result<()> {
    let rest = args.get(1..).unwrap_or_default();
    match args.first().map(String::as_str) {
        Some("install") => imp::install(rest),
        Some("update") => imp::update(),
        Some("uninstall") => imp::uninstall(),
        Some("pair") => imp::console(Some("pair")),
        Some("console") => imp::console(None),
        // Started by the service control manager.
        Some("run") => imp::run(rest),
        // Started by the service in the user's session.
        Some("session-host") => imp::session_host(rest),
        Some("-h" | "--help") | None => {
            print!("{USAGE}");
            Ok(())
        }
        Some(other) => bail!("unknown service command {other:?}\n\n{USAGE}"),
    }
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

/// A fresh control pipe name.
pub fn random_pipe_name() -> anyhow::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("random pipe name: {e}"))?;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!("{PIPE_PREFIX}{hex}"))
}

/// The session host's arguments (after the exe), as the service builds
/// them: the pipe, the installed serve options, and the service's data
/// folder.
pub fn session_host_args(pipe: &str, serve_args: &[String], data_dir: &Path) -> Vec<String> {
    let mut args = vec!["service".into(), "session-host".into(), pipe.into()];
    args.extend(serve_args.iter().cloned());
    args.push("--data-dir".into());
    args.push(data_dir.display().to_string());
    args
}

/// Splits what follows `service session-host` into the pipe name and the
/// serve options, checking both.
pub fn parse_session_host_args(args: &[String]) -> anyhow::Result<(String, ServeOptions)> {
    let Some((pipe, rest)) = args.split_first() else {
        bail!("session-host needs a pipe name");
    };
    if !valid_pipe_name(pipe) {
        bail!("bad control pipe name {pipe:?}");
    }
    let opts = ServeOptions::parse(rest)?;
    check_session_host_options(&opts)?;
    Ok((pipe.clone(), opts))
}

pub fn valid_pipe_name(name: &str) -> bool {
    name.strip_prefix(PIPE_PREFIX)
        .is_some_and(|rest| rest.len() == 32 && rest.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Replaces the value after `flag` (the first one) with `value`.
pub fn replace_value(args: &[String], flag: &str, value: &str) -> Vec<String> {
    let mut out = args.to_vec();
    if let Some(i) = out.iter().position(|a| a == flag) {
        if let Some(v) = out.get_mut(i + 1) {
            *v = value.to_string();
        }
    }
    out
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
        bail!("the session host never serves a web root or receives files");
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
    pub use super::control::console;
    pub use super::scm::{install, run, uninstall, update};

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
        let (pipe, opts) = super::parse_session_host_args(args)?;
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(super::control::serve(pipe, opts))
    }
}

#[cfg(not(windows))]
mod imp {
    fn windows_only() -> anyhow::Result<()> {
        anyhow::bail!("the host service is Windows only")
    }
    pub fn install(args: &[String]) -> anyhow::Result<()> {
        super::check_install_args(args)?;
        windows_only()
    }
    pub fn update() -> anyhow::Result<()> {
        windows_only()
    }
    pub fn uninstall() -> anyhow::Result<()> {
        windows_only()
    }
    pub fn console(_: Option<&str>) -> anyhow::Result<()> {
        windows_only()
    }
    pub fn run(_: &[String]) -> anyhow::Result<()> {
        windows_only()
    }
    pub fn session_host(_: &[String]) -> anyhow::Result<()> {
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
        assert!(check_install_args(&args(&["--bogus"])).is_err());
        let o = check_install_args(&args(&["--port", "9000", "--codec", "av1"])).unwrap();
        assert_eq!(o.port, 9000);
        assert!(check_session_host_options(&o).is_ok());
        let o = ServeOptions::parse(&args(&["--allow-upload", "--upload-dir", "x"])).unwrap();
        assert!(check_session_host_options(&o).is_err());
    }

    #[test]
    fn session_host_args_round_trip() {
        let pipe = random_pipe_name().unwrap();
        assert!(valid_pipe_name(&pipe));
        let dir = Path::new(r"C:\Windows\System32\config\systemprofile\App Data\web-stream");
        let built = session_host_args(&pipe, &args(&["--port", "9000", "--codec", "av1"]), dir);
        assert_eq!(&built[..2], ["service", "session-host"]);
        let (p, o) = parse_session_host_args(&built[2..]).unwrap();
        assert_eq!(p, pipe);
        assert_eq!(o.port, 9000);
        assert_eq!(o.data_dir, dir);

        assert!(parse_session_host_args(&[]).is_err());
        let squat = format!("{PIPE_PREFIX}x");
        assert!(parse_session_host_args(&args(&[&squat])).is_err());
        assert!(!valid_pipe_name(r"\\.\pipe\other"));
        let uploads = session_host_args(&pipe, &args(&["--allow-upload"]), dir);
        assert!(parse_session_host_args(&uploads[2..]).is_err());
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
