//! Updating and restarting the host from the browser
//! (`--allow-remote-update`): runs `scripts\update.ps1` from the checkout
//! the running host was built in. The script pulls `main`, builds beside
//! the running host and restarts the tray app once the build succeeded, so
//! the browser drops for a few seconds and reconnects to the new build.
//!
//! Only the tray app can be restarted by the script, so it says so with
//! [`enable`]; the console host and the service answer with an error.
//! The browser chooses nothing: no flags, no branch, no path.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use ws_proto::UpdateState;

static ENABLED: AtomicBool = AtomicBool::new(false);
/// An update runs; another is refused until it exits.
static RUNNING: AtomicBool = AtomicBool::new(false);

/// Called by the tray app: update.ps1 knows how to restart it.
pub fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
}

/// Whether this host can update itself at all (it still needs
/// `--allow-remote-update`).
pub fn available() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Starts update.ps1 with its output in `log`. `done` gets how it ended if
/// the script exits without restarting this host (nothing to update, or the
/// build failed); a restart ends this process first. The error and the
/// returned message are for the browser.
pub fn start(
    log: &Path,
    done: impl FnOnce(UpdateState, String) + Send + 'static,
) -> Result<String, String> {
    if !available() {
        return Err(
            "only the tray app (ws-host-gui) can update and restart itself; \
                    the console host and the service are updated with scripts\\update.ps1 on the PC"
                .into(),
        );
    }
    let exe = std::env::current_exe().map_err(|e| format!("can't find the host program: {e}"))?;
    let script = script_for(&exe)?;
    if RUNNING.swap(true, Ordering::AcqRel) {
        return Err("an update is already running on the host".into());
    }
    match spawn(&script, log) {
        Ok(child) => {
            tracing::info!(script = %script.display(), log = %log.display(), "browser started an update");
            let log_path = log.to_path_buf();
            std::thread::Builder::new()
                .name("ws-update".into())
                .spawn(move || {
                    let status = wait(child);
                    RUNNING.store(false, Ordering::Release);
                    let (state, message) = outcome(status, &log_path);
                    tracing::info!(?state, message, "update script exited");
                    done(state, message);
                })
                .map_err(|e| {
                    RUNNING.store(false, Ordering::Release);
                    format!("couldn't watch the update: {e}")
                })?;
            Ok(format!(
                "Building the latest main on the host. It restarts when the build is done, \
                 which can take several minutes; the stream keeps running until then. \
                 Log: {}",
                log.display()
            ))
        }
        Err(e) => {
            RUNNING.store(false, Ordering::Release);
            Err(e)
        }
    }
}

/// `scripts\update.ps1` of the checkout that built `exe`, which lives in its
/// `target\release`. Never from the working directory or the environment.
fn script_for(exe: &Path) -> Result<PathBuf, String> {
    let not_checkout = || {
        format!(
            "{} wasn't built by install.ps1 or update.ps1 in a git clone (it isn't in \
             <clone>\\target\\release), so it can't update itself",
            exe.display()
        )
    };
    let bin = exe.parent().ok_or_else(not_checkout)?;
    if bin.file_name().is_none_or(|n| n != "release") {
        return Err(not_checkout());
    }
    let repo = bin
        .parent()
        .filter(|t| t.file_name().is_some_and(|n| n == "target"))
        .and_then(Path::parent)
        .ok_or_else(not_checkout)?;
    let script = repo.join("scripts").join("update.ps1");
    if !repo.join("Cargo.toml").is_file() || !script.is_file() {
        return Err(not_checkout());
    }
    if !repo.join(".git").exists() {
        return Err(format!(
            "{} is not a git clone (a downloaded ZIP has no .git folder), so it can't be updated",
            repo.display()
        ));
    }
    Ok(script)
}

#[cfg(windows)]
fn spawn(script: &Path, log: &Path) -> Result<std::process::Child, String> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let powershell = system_dir()?.join(r"WindowsPowerShell\v1.0\powershell.exe");
    let out =
        std::fs::File::create(log).map_err(|e| format!("can't write {}: {e}", log.display()))?;
    let err = out
        .try_clone()
        .map_err(|e| format!("can't write {}: {e}", log.display()))?;
    let repo = script.parent().and_then(Path::parent).unwrap_or(script);
    // A process group of its own and no console: the script outlives this
    // host, which it stops before starting the new build.
    Command::new(powershell)
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(script)
        .current_dir(repo)
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("couldn't start update.ps1: {e}"))
}

/// System32, from the shell rather than `%SystemRoot%`, which the
/// environment could point elsewhere.
#[cfg(windows)]
fn system_dir() -> Result<PathBuf, String> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{FOLDERID_System, KF_FLAG_DEFAULT, SHGetKnownFolderPath};

    // SAFETY: the returned string is freed below.
    let p = unsafe { SHGetKnownFolderPath(&FOLDERID_System, KF_FLAG_DEFAULT, None) }
        .map_err(|e| format!("can't find System32: {e}"))?;
    // SAFETY: a valid NUL-terminated string from the shell.
    let path = unsafe { p.to_string() }.map(PathBuf::from);
    // SAFETY: allocated by the shell with CoTaskMemAlloc.
    unsafe { CoTaskMemFree(Some(p.0.cast())) };
    path.map_err(|e| format!("can't find System32: {e}"))
}

#[cfg(not(windows))]
fn spawn(_script: &Path, _log: &Path) -> Result<std::process::Child, String> {
    Err("updating from the browser needs a Windows host".into())
}

fn wait(mut child: std::process::Child) -> Option<i32> {
    child.wait().ok().and_then(|s| s.code())
}

/// How a script that exited (while this host still runs) went: its last
/// line of output, which says why it didn't restart the host.
fn outcome(code: Option<i32>, log: &Path) -> (UpdateState, String) {
    let lines: Vec<String> = std::fs::File::open(log)
        .map(|f| {
            BufReader::new(f)
                .lines()
                .map_while(Result::ok)
                .map(|l| l.trim().to_owned())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default();
    summarize(code, &lines, log)
}

fn summarize(code: Option<i32>, lines: &[String], log: &Path) -> (UpdateState, String) {
    let clip = |s: &str| s.chars().take(300).collect::<String>();
    match code {
        Some(0) => {
            let last = lines.last().map_or("", String::as_str);
            (UpdateState::Finished, clip(last))
        }
        _ => {
            // A PowerShell error prints its message first, then where it
            // happened; the first line that isn't one of update.ps1's own
            // "==> step" lines or the location is the message.
            let message = lines
                .iter()
                .rev()
                .find(|l| !l.starts_with("==>") && !l.starts_with('+') && !l.starts_with("At "))
                .map_or("", String::as_str);
            let code = code.map_or("killed".into(), |c| format!("exit code {c}"));
            (
                UpdateState::Failed,
                format!(
                    "the update failed ({code}): {}. The old build keeps running. Log: {}",
                    clip(message),
                    log.display()
                ),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_only_from_the_checkout_that_built_the_exe() {
        let root = std::env::temp_dir().join(format!("ws-update-test-{}", std::process::id()));
        let bin = root.join("target").join("release");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        let exe = bin.join("ws-host-gui.exe");

        // No Cargo.toml or script: not a checkout.
        assert!(script_for(&exe).is_err());
        std::fs::write(root.join("Cargo.toml"), "").unwrap();
        std::fs::write(root.join("scripts").join("update.ps1"), "").unwrap();
        // A ZIP download has no .git.
        assert!(script_for(&exe).unwrap_err().contains("not a git clone"));
        std::fs::create_dir_all(root.join(".git")).unwrap();
        assert_eq!(
            script_for(&exe).unwrap(),
            root.join("scripts").join("update.ps1")
        );
        // Anywhere else (a copy in Program Files, a debug build) is refused.
        assert!(script_for(&root.join("target").join("debug").join("x.exe")).is_err());
        assert!(script_for(&root.join("x.exe")).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn outcomes() {
        let log = Path::new("update.log");
        let lines = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            summarize(
                Some(0),
                &lines(&["==> Fetching main", "Already up to date (abcd1234)."]),
                log
            ),
            (
                UpdateState::Finished,
                "Already up to date (abcd1234).".into()
            )
        );
        let (state, message) = summarize(
            Some(1),
            &lines(&[
                "==> Building the host (release)",
                "cargo build failed. The previous build is back in place and the host was left running.",
                "At C:\\ws\\scripts\\update.ps1:300 char:5",
                "+     throw 'cargo build failed.'",
                "+ FullyQualifiedErrorId : cargo build failed.",
            ]),
            log,
        );
        assert_eq!(state, UpdateState::Failed);
        assert!(message.contains("exit code 1"), "{message}");
        assert!(message.contains("cargo build failed."), "{message}");
        assert!(!message.contains("char:5"), "{message}");
    }
}
