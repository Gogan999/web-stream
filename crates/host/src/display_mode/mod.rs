//! Linux display modes: a helper in the signed-in user's session asks the compositor or X server
//! (X11 RandR, wlroots output management, GNOME Mutter, KDE kscreen-doctor), keeps each display's
//! original mode and puts it back when asked, when its stdin closes or on SIGTERM.

mod helper;
mod kscreen;
mod mutter;
mod plan;
mod wlr;
mod x11;

use std::io;
use std::os::fd::AsRawFd;
use std::process::{Child, ChildStdin, ChildStdout};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use ws_audio::session::{self, Session};
use ws_capture::{CaptureError, ModeChange};
use ws_proto::DisplayId;

use crate::session_helper::{self, read_frame, write_frame};

pub use helper::main as helper_main;

/// Argument that runs the binary as the display mode helper.
pub const HELPER_ARG: &str = "display-helper";

/// Host to helper: a JSON [`Request`].
const REQUEST: u8 = b'Q';
/// Helper to host: a JSON [`Reply`].
const REPLY: u8 = b'A';
/// Largest frame either way.
const MAX_FRAME: usize = 64 * 1024;
/// How long the helper gets to answer.
const REPLY_TIMEOUT: Duration = Duration::from_secs(15);
/// How long a stopping helper gets to put displays back.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Request {
    /// Switch `connector` to the largest mode that fits `width` x `height`.
    Set {
        connector: String,
        edid: Option<Vec<u8>>,
        width: u32,
        height: u32,
    },
    /// Put `connector` back to the mode it had before the first [`Request::Set`].
    Restore { connector: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reply {
    /// The desktop size chosen; `remaining` displays are still changed.
    Set {
        width: u32,
        height: u32,
        changed: bool,
        remaining: usize,
    },
    Restored {
        remaining: usize,
    },
    Failed(String),
}

/// A running helper.
struct Running {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: ChildStdout,
    session: Session,
    /// Displays it changed, with their connector names.
    changed: Vec<(DisplayId, String)>,
}

impl Running {
    fn spawn(session: Session) -> io::Result<Self> {
        let exe = std::env::current_exe()?;
        let mut child = session_helper::command(&exe, HELPER_ARG, &session).spawn()?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().ok_or(io::ErrorKind::BrokenPipe)?;
        tracing::info!(
            uid = session.uid,
            kind = session.kind,
            "display helper started"
        );
        Ok(Self {
            child,
            stdin,
            stdout,
            session,
            changed: Vec::new(),
        })
    }

    fn call(&mut self, request: &Request) -> io::Result<Reply> {
        let stdin = self.stdin.as_mut().ok_or(io::ErrorKind::BrokenPipe)?;
        let body = serde_json::to_vec(request)?;
        write_frame(stdin, REQUEST, &body)?;
        if !readable(self.stdout.as_raw_fd(), REPLY_TIMEOUT)? {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the display helper did not answer",
            ));
        }
        let (kind, body) = read_frame(&mut self.stdout, MAX_FRAME)?;
        if kind != REPLY {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unexpected frame from the display helper",
            ));
        }
        Ok(serde_json::from_slice(&body)?)
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stdin = None;
        let mut sink = [0u8; 256];
        while readable(self.stdout.as_raw_fd(), STOP_TIMEOUT).unwrap_or(false) {
            match io::Read::read(&mut self.stdout, &mut sink) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        tracing::info!("display helper stopped");
    }
}

/// Waits until `fd` is readable or closed, up to `timeout`.
fn readable(fd: i32, timeout: Duration) -> io::Result<bool> {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    loop {
        // SAFETY: `pfd` is one valid pollfd.
        let r = unsafe { libc::poll(&mut pfd, 1, ms) };
        if r >= 0 {
            return Ok(r > 0);
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

static HELPER: Mutex<Option<Running>> = Mutex::new(None);

fn platform(msg: impl Into<String>) -> CaptureError {
    CaptureError::Platform(msg.into())
}

/// Switches `display` through the signed-in user's session; see
/// [`ws_capture::set_display_mode`].
pub fn set_display_mode(
    display: DisplayId,
    width: u32,
    height: u32,
) -> ws_capture::Result<ModeChange> {
    let connector = ws_capture::connector(display)?;
    let session =
        session::active().ok_or_else(|| platform("nobody is signed in to a graphical session"))?;
    let mut helper = HELPER.lock().unwrap_or_else(|e| e.into_inner());
    if helper.as_ref().is_some_and(|h| h.session != session) {
        *helper = None;
    }
    if helper.is_none() {
        *helper =
            Some(Running::spawn(session).map_err(|e| platform(format!("display helper: {e}")))?);
    }
    let running = helper.as_mut().expect("helper running");
    let request = Request::Set {
        connector: connector.name.clone(),
        edid: connector.edid,
        width,
        height,
    };
    let reply = match running.call(&request) {
        Ok(r) => r,
        Err(e) => {
            *helper = None;
            return Err(platform(format!("display helper: {e}")));
        }
    };
    match reply {
        Reply::Set {
            width,
            height,
            changed,
            remaining,
        } => {
            if changed && !running.changed.iter().any(|(id, _)| *id == display) {
                running.changed.push((display, connector.name));
            }
            if remaining == 0 {
                *helper = None;
            }
            Ok(ModeChange {
                width,
                height,
                changed,
            })
        }
        Reply::Failed(e) => {
            if running.changed.is_empty() {
                *helper = None;
            }
            Err(platform(e))
        }
        Reply::Restored { .. } => Err(platform("display helper: unexpected reply")),
    }
}

/// Puts `display` back to the mode it had before [`set_display_mode`] changed it.
pub fn restore_display_mode(display: DisplayId) -> ws_capture::Result<()> {
    let mut helper = HELPER.lock().unwrap_or_else(|e| e.into_inner());
    let Some(running) = helper.as_mut() else {
        return Ok(());
    };
    let Some(at) = running.changed.iter().position(|(id, _)| *id == display) else {
        return Ok(());
    };
    let (_, connector) = running.changed.remove(at);
    let result = match running.call(&Request::Restore { connector }) {
        Ok(Reply::Restored { remaining: 0 }) => {
            *helper = None;
            Ok(())
        }
        Ok(Reply::Restored { .. }) => Ok(()),
        Ok(Reply::Failed(e)) => Err(platform(e)),
        Ok(Reply::Set { .. }) => Err(platform("display helper: unexpected reply")),
        Err(e) => {
            *helper = None;
            Err(platform(format!("display helper: {e}")))
        }
    };
    if helper.as_ref().is_some_and(|h| h.changed.is_empty()) {
        *helper = None;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip_as_json() {
        let set = Request::Set {
            connector: "HDMI-A-1".into(),
            edid: Some(vec![0, 255]),
            width: 1920,
            height: 1080,
        };
        let json = serde_json::to_string(&set).unwrap();
        assert_eq!(
            json,
            r#"{"set":{"connector":"HDMI-A-1","edid":[0,255],"width":1920,"height":1080}}"#
        );
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), set);
        let restore = Request::Restore {
            connector: "DP-1".into(),
        };
        assert_eq!(
            serde_json::to_string(&restore).unwrap(),
            r#"{"restore":{"connector":"DP-1"}}"#
        );
        for reply in [
            Reply::Set {
                width: 1280,
                height: 720,
                changed: true,
                remaining: 1,
            },
            Reply::Restored { remaining: 0 },
            Reply::Failed("no".into()),
        ] {
            let json = serde_json::to_vec(&reply).unwrap();
            assert_eq!(serde_json::from_slice::<Reply>(&json).unwrap(), reply);
        }
    }
}
