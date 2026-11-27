//! The user signed in to the local graphical session, from logind's state under `/run/systemd`
//! (what sd-login reads).

use std::collections::HashMap;
use std::ffi::CStr;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// A signed-in graphical session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    pub uid: u32,
    /// `wayland`, `x11` or `mir`.
    pub kind: String,
    pub runtime_dir: PathBuf,
    /// The X display logind recorded, if any.
    pub display: Option<String>,
}

/// An account from the password database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
}

fn parse(text: &str) -> HashMap<&str, &str> {
    text.lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim(), v.trim()))
        .collect()
}

/// The active session on `seat` given logind's state files under `root`.
fn active_in(root: &Path, seat: &str) -> Option<Session> {
    let seat = std::fs::read_to_string(root.join("seats").join(seat)).ok()?;
    let seat = parse(&seat);
    let id = *seat.get("ACTIVE").filter(|s| !s.is_empty())?;
    let session = std::fs::read_to_string(root.join("sessions").join(id)).ok()?;
    let session = parse(&session);
    let uid: u32 = session.get("UID")?.parse().ok()?;
    let kind = *session.get("TYPE")?;
    let class = session.get("CLASS").copied().unwrap_or("user");
    let active = session.get("ACTIVE") != Some(&"0");
    if !active || class != "user" || !matches!(kind, "wayland" | "x11" | "mir") {
        return None;
    }
    Some(Session {
        id: id.into(),
        uid,
        kind: kind.into(),
        runtime_dir: PathBuf::from(format!("/run/user/{uid}")),
        display: session
            .get("DISPLAY")
            .filter(|d| !d.is_empty())
            .map(|d| d.to_string()),
    })
}

/// The process's own session, when it runs inside one.
fn own() -> Option<Session> {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")?;
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    Some(Session {
        id: std::env::var("XDG_SESSION_ID").unwrap_or_default(),
        uid,
        kind: std::env::var("XDG_SESSION_TYPE").unwrap_or_default(),
        runtime_dir: runtime_dir.into(),
        display: std::env::var("DISPLAY").ok(),
    })
}

/// The session whose user audio and the clipboard belong to: for root, logind's active session on
/// seat0 (`None` while nobody is signed in or the greeter shows); otherwise this process's own.
pub fn active() -> Option<Session> {
    // SAFETY: getuid has no preconditions.
    if unsafe { libc::getuid() } != 0 {
        return own();
    }
    let root = Path::new("/run/systemd");
    if root.join("seats").exists() {
        return active_in(root, "seat0");
    }
    own()
}

/// Looks `uid` up in the password database.
pub fn user(uid: u32) -> Option<User> {
    // SAFETY: getpwuid_r writes into the buffers we pass; the strings are copied out before they
    // go away.
    unsafe {
        let mut pw: libc::passwd = std::mem::zeroed();
        let mut buf = vec![0 as libc::c_char; 16 * 1024];
        let mut out = std::ptr::null_mut();
        let rc = libc::getpwuid_r(uid, &mut pw, buf.as_mut_ptr(), buf.len(), &mut out);
        if rc != 0 || out.is_null() {
            return None;
        }
        Some(User {
            name: CStr::from_ptr(pw.pw_name).to_string_lossy().into_owned(),
            uid: pw.pw_uid,
            gid: pw.pw_gid,
            home: PathBuf::from(CStr::from_ptr(pw.pw_dir).to_string_lossy().into_owned()),
        })
    }
}

/// Variables a program needs to reach the session's display, taken from one of its processes.
const DISPLAY_VARS: [&str; 4] = [
    "WAYLAND_DISPLAY",
    "DISPLAY",
    "XAUTHORITY",
    "DBUS_SESSION_BUS_ADDRESS",
];

/// `WAYLAND_DISPLAY`, `DISPLAY`, `XAUTHORITY` and the session bus of `session`, from the
/// environment of a process of its user that has a display.
pub fn display_env(session: &Session) -> Vec<(String, String)> {
    let mut best: Vec<(String, String)> = Vec::new();
    let mut best_score = 0;
    if let Ok(dir) = std::fs::read_dir("/proc") {
        for entry in dir.flatten() {
            let path = entry.path();
            if entry.metadata().map(|m| m.uid()).ok() != Some(session.uid) {
                continue;
            }
            let Ok(raw) = std::fs::read(path.join("environ")) else {
                continue;
            };
            let env: HashMap<&str, &str> = raw
                .split(|&b| b == 0)
                .filter_map(|kv| std::str::from_utf8(kv).ok()?.split_once('='))
                .collect();
            if env.get("XDG_SESSION_ID").is_some_and(|s| *s != session.id) {
                continue;
            }
            let score = usize::from(env.contains_key("WAYLAND_DISPLAY")) * 2
                + usize::from(env.contains_key("DISPLAY"))
                + usize::from(env.contains_key("XAUTHORITY"));
            if score > best_score {
                best_score = score;
                best = DISPLAY_VARS
                    .iter()
                    .filter_map(|k| Some((k.to_string(), env.get(k)?.to_string())))
                    .collect();
            }
        }
    }
    let has = |best: &[(String, String)], k: &str| best.iter().any(|(n, _)| n == k);
    if !has(&best, "WAYLAND_DISPLAY") && session.runtime_dir.join("wayland-0").exists() {
        best.push(("WAYLAND_DISPLAY".into(), "wayland-0".into()));
    }
    if !has(&best, "DISPLAY") {
        if let Some(d) = &session.display {
            best.push(("DISPLAY".into(), d.clone()));
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(seat: &str, session: &str) -> tempdir::Dir {
        let dir = tempdir::Dir::new();
        std::fs::create_dir_all(dir.0.join("seats")).unwrap();
        std::fs::create_dir_all(dir.0.join("sessions")).unwrap();
        std::fs::write(dir.0.join("seats/seat0"), seat).unwrap();
        std::fs::write(dir.0.join("sessions/3"), session).unwrap();
        dir
    }

    mod tempdir {
        pub struct Dir(pub std::path::PathBuf);

        impl Dir {
            pub fn new() -> Self {
                use std::sync::atomic::{AtomicU32, Ordering};
                static N: AtomicU32 = AtomicU32::new(0);
                let path = std::env::temp_dir().join(format!(
                    "ws-session-{}-{}",
                    std::process::id(),
                    N.fetch_add(1, Ordering::Relaxed)
                ));
                std::fs::create_dir_all(&path).unwrap();
                Self(path)
            }
        }

        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn active_user_session_is_found() {
        let d = state(
            "IS_SEAT0=1\nACTIVE=3\nACTIVE_UID=1000\n",
            "UID=1000\nUSER=alice\nACTIVE=1\nTYPE=wayland\nCLASS=user\nSEAT=seat0\n",
        );
        let s = active_in(&d.0, "seat0").unwrap();
        assert_eq!(s.uid, 1000);
        assert_eq!(s.kind, "wayland");
        assert_eq!(s.runtime_dir, PathBuf::from("/run/user/1000"));
        assert_eq!(s.display, None);
    }

    #[test]
    fn greeter_tty_and_empty_seats_are_nobody() {
        let greeter = state(
            "ACTIVE=3\n",
            "UID=120\nACTIVE=1\nTYPE=wayland\nCLASS=greeter\n",
        );
        assert_eq!(active_in(&greeter.0, "seat0"), None);
        let tty = state("ACTIVE=3\n", "UID=1000\nACTIVE=1\nTYPE=tty\nCLASS=user\n");
        assert_eq!(active_in(&tty.0, "seat0"), None);
        let empty = state("ACTIVE=\n", "");
        assert_eq!(active_in(&empty.0, "seat0"), None);
    }

    #[test]
    fn x11_session_keeps_its_display() {
        let d = state(
            "ACTIVE=3\n",
            "UID=1001\nACTIVE=1\nTYPE=x11\nCLASS=user\nDISPLAY=:1\n",
        );
        assert_eq!(
            active_in(&d.0, "seat0").unwrap().display.as_deref(),
            Some(":1")
        );
    }

    #[test]
    fn root_is_in_the_password_database() {
        assert_eq!(user(0).map(|u| u.home), Some(PathBuf::from("/root")));
    }
}
