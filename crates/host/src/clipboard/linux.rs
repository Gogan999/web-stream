//! Linux: a helper process in the signed-in user's session owns the Wayland or X11 clipboard and
//! talks to the host over its stdin and stdout.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use super::*;
use ws_audio::session::{self, Session};

/// Argument that runs the binary as the clipboard helper.
pub const HELPER_ARG: &str = "clipboard-helper";

/// Host to helper: put this text on the clipboard.
pub const SET: u8 = b'S';
/// Helper to host: the clipboard now holds this text.
pub const CHANGED: u8 = b'C';
/// Helper to host: the clipboard holds text over [`MAX_BYTES`].
pub const TOO_LARGE: u8 = b'L';
/// Helper to host: a [`SET`] worked (1) or not (0).
pub const WRITTEN: u8 = b'W';

/// How often the manager checks the session and whether a browser is connected.
const CHECK: Duration = Duration::from_millis(250);
/// Wait before starting a helper again after it failed.
const RESPAWN: Duration = Duration::from_secs(5);

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

/// Reads one frame; payloads over [`MAX_BYTES`] plus a little are refused.
pub fn read_frame(r: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut head = [0u8; 5];
    r.read_exact(&mut head)?;
    let len = u32::from_le_bytes([head[1], head[2], head[3], head[4]]) as usize;
    if len > MAX_BYTES + 64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok((head[0], payload))
}

type Done = Box<dyn FnOnce(bool) + Send>;

enum Msg {
    Set(String, Done),
    /// A frame from the helper of generation `.0`.
    Frame(u64, u8, Vec<u8>),
    Exited(u64),
    Stop,
}

pub struct Handle {
    tx: Sender<Msg>,
}

impl Handle {
    pub fn set(&self, text: String, done: Done) {
        if let Err(mpsc::SendError(Msg::Set(_, done))) = self.tx.send(Msg::Set(text, done)) {
            done(false);
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Stop);
    }
}

pub fn start(
    state: Arc<Mutex<SyncState>>,
    active: Arc<AtomicBool>,
    on_change: Box<dyn Fn(String) + Send>,
) -> Result<Handle, String> {
    let exe = std::env::current_exe().map_err(|e| format!("clipboard helper: {e}"))?;
    let (tx, rx) = mpsc::channel();
    let manager = Manager {
        exe,
        state,
        active,
        on_change,
        tx: tx.clone(),
        helper: None,
        generation: 0,
        failed: None,
        pending: VecDeque::new(),
    };
    std::thread::Builder::new()
        .name("ws-clipboard".into())
        .spawn(move || manager.run(rx))
        .map_err(|e| format!("clipboard thread: {e}"))?;
    Ok(Handle { tx })
}

struct Helper {
    child: Child,
    stdin: ChildStdin,
    session: Session,
}

impl Drop for Helper {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Manager {
    exe: std::path::PathBuf,
    state: Arc<Mutex<SyncState>>,
    active: Arc<AtomicBool>,
    on_change: Box<dyn Fn(String) + Send>,
    tx: Sender<Msg>,
    helper: Option<Helper>,
    generation: u64,
    failed: Option<Instant>,
    /// Writes sent to the helper, answered in order.
    pending: VecDeque<Done>,
}

impl Manager {
    fn run(mut self, rx: Receiver<Msg>) {
        loop {
            match rx.recv_timeout(CHECK) {
                Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(Msg::Set(text, done)) => self.set(text, done),
                Ok(Msg::Frame(generation, kind, payload)) if generation == self.generation => {
                    self.frame(kind, payload)
                }
                Ok(Msg::Exited(generation)) if generation == self.generation => {
                    if self.helper.take().is_some() {
                        tracing::warn!("clipboard helper stopped");
                        self.failed = Some(Instant::now());
                    }
                    self.fail_pending();
                }
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => {}
            }
            self.check();
        }
        self.helper = None;
        self.fail_pending();
    }

    fn fail_pending(&mut self) {
        for done in self.pending.drain(..) {
            done(false);
        }
    }

    /// Starts, stops or moves the helper to match the session and the browser.
    fn check(&mut self) {
        let wanted = if self.active.load(Ordering::Relaxed) {
            session::active()
        } else {
            None
        };
        let current = self.helper.as_ref().map(|h| &h.session);
        if current == wanted.as_ref() {
            return;
        }
        if self.helper.take().is_some() {
            self.fail_pending();
        }
        let Some(session) = wanted else { return };
        if self.failed.is_some_and(|t| t.elapsed() < RESPAWN) {
            return;
        }
        match self.spawn(session) {
            Ok(h) => {
                self.helper = Some(h);
                self.failed = None;
            }
            Err(e) => {
                tracing::warn!("clipboard helper: {e}");
                self.failed = Some(Instant::now());
            }
        }
    }

    fn spawn(&mut self, session: Session) -> Result<Helper, String> {
        let mut cmd = Command::new(&self.exe);
        cmd.arg(HELPER_ARG)
            .env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("XDG_RUNTIME_DIR", &session.runtime_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (k, v) in session::display_env(&session) {
            cmd.env(k, v);
        }
        if let Some(user) = session::user(session.uid) {
            cmd.env("HOME", &user.home).env("USER", &user.name);
            // SAFETY: getuid has no preconditions.
            if unsafe { libc::getuid() } == 0 {
                cmd.uid(user.uid).gid(user.gid);
            }
        }
        let mut child = cmd.spawn().map_err(|e| format!("start: {e}"))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let mut stdout = child.stdout.take().ok_or("no stdout")?;
        self.generation += 1;
        let generation = self.generation;
        let tx = self.tx.clone();
        std::thread::Builder::new()
            .name("ws-clipboard-read".into())
            .spawn(move || {
                while let Ok((kind, payload)) = read_frame(&mut stdout) {
                    if tx.send(Msg::Frame(generation, kind, payload)).is_err() {
                        return;
                    }
                }
                let _ = tx.send(Msg::Exited(generation));
            })
            .map_err(|e| format!("reader thread: {e}"))?;
        tracing::info!(
            uid = session.uid,
            kind = session.kind,
            "clipboard helper started"
        );
        Ok(Helper {
            child,
            stdin,
            session,
        })
    }

    fn set(&mut self, text: String, done: Done) {
        let Some(helper) = &mut self.helper else {
            return done(false);
        };
        match write_frame(&mut helper.stdin, SET, text.as_bytes()) {
            Ok(()) => self.pending.push_back(done),
            Err(e) => {
                tracing::debug!("clipboard helper write: {e}");
                done(false);
            }
        }
    }

    fn frame(&mut self, kind: u8, payload: Vec<u8>) {
        match kind {
            WRITTEN => {
                if let Some(done) = self.pending.pop_front() {
                    done(payload.first() == Some(&1));
                }
            }
            TOO_LARGE => tracing::info!("clipboard text over the size limit, not synced"),
            CHANGED if self.active.load(Ordering::Relaxed) => {
                let text = String::from_utf8_lossy(&payload).into_owned();
                let change = self
                    .state
                    .lock()
                    .expect("clipboard state lock")
                    .local_changed(text);
                match change {
                    Local::Forward(text) => (self.on_change)(text),
                    Local::TooLarge => {
                        tracing::info!("clipboard text over the size limit, not synced")
                    }
                    Local::Echo | Local::Empty => {}
                }
            }
            _ => {}
        }
    }
}

/// Runs the helper: bridges the session's clipboard to frames on stdin and stdout.
pub fn helper_main() -> i32 {
    let result = match super::wayland::run() {
        Err(super::wayland::Unavailable(why)) => {
            tracing::debug!("no Wayland data-control: {why}");
            super::x11::run()
        }
        Ok(r) => r,
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("clipboard helper: {e}");
            1
        }
    }
}

/// Text MIME types and X11 targets, most wanted first.
pub const TEXT_TYPES: [&str; 5] = [
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "STRING",
    "TEXT",
];

/// The type password managers offer alongside text they want kept out of clipboard history.
pub const SECRET_HINT: &str = "x-kde-passwordManagerHint";

/// Whether the hint's content marks the text secret.
pub fn is_secret(hint: &[u8]) -> bool {
    hint.trim_ascii() == b"secret"
}

/// Sends what the helper read from the clipboard to the host.
pub fn report(out: &mut impl Write, text: Option<Vec<u8>>) -> io::Result<()> {
    match text {
        Some(t) if std::str::from_utf8(&t).is_ok() => write_frame(out, CHANGED, &t),
        Some(t) => write_frame(out, CHANGED, String::from_utf8_lossy(&t).as_bytes()),
        None => write_frame(out, TOO_LARGE, &[]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, SET, "h\u{e9}llo\nworld".as_bytes()).unwrap();
        write_frame(&mut buf, WRITTEN, &[1]).unwrap();
        let mut r = &buf[..];
        assert_eq!(
            read_frame(&mut r).unwrap(),
            (SET, "h\u{e9}llo\nworld".as_bytes().to_vec())
        );
        assert_eq!(read_frame(&mut r).unwrap(), (WRITTEN, vec![1]));
        assert!(read_frame(&mut r).is_err());
    }

    #[test]
    fn oversized_frames_are_refused() {
        let mut head = vec![CHANGED];
        head.extend_from_slice(&(MAX_BYTES as u32 + 1000).to_le_bytes());
        assert!(read_frame(&mut &head[..]).is_err());
    }

    #[test]
    fn reports_text_or_too_large() {
        let mut out = Vec::new();
        report(&mut out, Some(b"hi".to_vec())).unwrap();
        report(&mut out, Some(vec![0xff])).unwrap();
        report(&mut out, None).unwrap();
        let mut r = &out[..];
        assert_eq!(read_frame(&mut r).unwrap(), (CHANGED, b"hi".to_vec()));
        assert_eq!(
            read_frame(&mut r).unwrap(),
            (CHANGED, "\u{fffd}".as_bytes().to_vec())
        );
        assert_eq!(read_frame(&mut r).unwrap(), (TOO_LARGE, vec![]));
    }

    #[test]
    fn password_manager_hint() {
        assert!(is_secret(b"secret"));
        assert!(is_secret(b"secret\n"));
        assert!(!is_secret(b"public"));
    }
}
