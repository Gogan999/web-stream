//! Helper processes that run as the signed-in user inside their graphical session, talking to the
//! host in length-prefixed frames over stdin and stdout.

use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

use ws_audio::session::{self, Session};

/// `exe arg`, as the session's user (when running as root), with only the environment that
/// reaches the session's display and bus, and piped stdin and stdout.
pub fn command(exe: &Path, arg: &str, session: &Session) -> Command {
    let mut cmd = Command::new(exe);
    cmd.arg(arg)
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("XDG_RUNTIME_DIR", &session.runtime_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if !session.kind.is_empty() {
        cmd.env("XDG_SESSION_TYPE", &session.kind);
    }
    for (k, v) in session::display_env(session) {
        cmd.env(k, v);
    }
    if let Some(user) = session::user(session.uid) {
        cmd.env("HOME", &user.home).env("USER", &user.name);
        // SAFETY: getuid has no preconditions.
        if unsafe { libc::getuid() } == 0 {
            cmd.uid(user.uid).gid(user.gid);
        }
    }
    cmd
}

/// Writes one frame: kind, little-endian length, payload.
pub fn write_frame(w: &mut impl Write, kind: u8, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len()).map_err(|_| io::ErrorKind::InvalidInput)?;
    let mut head = [0u8; 5];
    head[0] = kind;
    head[1..].copy_from_slice(&len.to_le_bytes());
    w.write_all(&head)?;
    w.write_all(payload)?;
    w.flush()
}

/// Reads one frame; payloads over `max` bytes are refused.
pub fn read_frame(r: &mut impl Read, max: usize) -> io::Result<(u8, Vec<u8>)> {
    let mut head = [0u8; 5];
    r.read_exact(&mut head)?;
    let len = u32::from_le_bytes([head[1], head[2], head[3], head[4]]) as usize;
    if len > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok((head[0], payload))
}
