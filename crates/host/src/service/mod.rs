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

use anyhow::bail;
use ws_host::options::ServeOptions;

#[cfg(windows)]
mod control;
#[cfg(windows)]
mod launch;
#[cfg(windows)]
mod scm;

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
// Used by the Windows-only session host and launcher, and the tests.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn check_session_host_options(opts: &ServeOptions) -> anyhow::Result<()> {
    if opts.web_root.is_some() || opts.media.upload_dir.is_some() {
        bail!("the session host never serves a web root or receives files");
    }
    Ok(())
}

/// Quotes one argument for a Windows command line the way
/// `CommandLineToArgvW` and the C runtime split it.
#[cfg_attr(not(windows), allow(dead_code))]
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

    pub fn session_host(args: &[String]) -> anyhow::Result<()> {
        if !ws_desktop::running_as_system() {
            anyhow::bail!("session-host is started by the web-stream service, not by hand");
        }
        let opts = ws_host::options::ServeOptions::parse(args)?;
        super::check_session_host_options(&opts)?;
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(super::control::serve(opts))
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
