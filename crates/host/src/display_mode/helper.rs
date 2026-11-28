//! The helper process: applies requests from the host through the session's display backend and
//! puts every display it changed back when stdin closes or a termination signal arrives.

use std::io::{self, Write};
use std::sync::mpsc;

use super::plan::{self, Head, HeadMode, Target};
use super::{MAX_FRAME, REPLY, REQUEST, Reply, Request, kscreen, mutter, wlr, x11};
use crate::session_helper::{read_frame, write_frame};

/// A way to read and change the session's outputs.
pub trait Backend {
    /// The enabled outputs.
    fn heads(&mut self) -> Result<Vec<Head>, String>;
    /// Sets every head in `heads` (as last read) to its entry in `targets`.
    fn apply(&mut self, heads: &[Head], targets: &[Target]) -> Result<(), String>;
}

/// The backend this session offers.
fn open() -> Result<Box<dyn Backend>, String> {
    let x11_session = std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "x11");
    let mut tried = Vec::new();
    if std::env::var_os("WAYLAND_DISPLAY").is_some() && !x11_session {
        match wlr::Wlr::connect() {
            Ok(Some(b)) => return Ok(Box::new(b)),
            Ok(None) => tried.push("wlroots output management: not offered".to_string()),
            Err(e) => tried.push(format!("Wayland: {e}")),
        }
        match mutter::Mutter::connect() {
            Ok(Some(b)) => return Ok(Box::new(b)),
            Ok(None) => tried.push("GNOME Mutter: not running".to_string()),
            Err(e) => tried.push(format!("GNOME Mutter: {e}")),
        }
        match kscreen::KScreen::find() {
            Some(b) => return Ok(Box::new(b)),
            None => tried.push("KDE kscreen-doctor: not installed".to_string()),
        }
    }
    if std::env::var_os("DISPLAY").is_some() {
        match x11::RandR::connect() {
            Ok(b) => return Ok(Box::new(b)),
            Err(e) => tried.push(format!("X11 RandR: {e}")),
        }
    }
    if tried.is_empty() {
        tried.push("no Wayland or X11 display in the session".into());
    }
    Err(format!(
        "this desktop offers no way to change the display mode ({})",
        tried.join("; ")
    ))
}

/// A display this helper changed.
struct Saved {
    connector: String,
    head: String,
    mode: HeadMode,
    scale: f64,
}

type Open = fn() -> Result<Box<dyn Backend>, String>;

struct State {
    open: Open,
    saved: Vec<Saved>,
    /// Every head's position before the first change.
    positions: Option<Vec<(String, i32, i32)>>,
}

impl State {
    fn new(open: Open) -> Self {
        Self {
            open,
            saved: Vec::new(),
            positions: None,
        }
    }

    fn handle(&mut self, request: Request) -> Reply {
        let result = match request {
            Request::Set {
                connector,
                edid,
                width,
                height,
            } => self.set(connector, edid, width, height),
            Request::Restore { connector } => self.restore(&connector),
        };
        result.unwrap_or_else(Reply::Failed)
    }

    fn set(
        &mut self,
        connector: String,
        edid: Option<Vec<u8>>,
        width: u32,
        height: u32,
    ) -> Result<Reply, String> {
        let mut backend = (self.open)()?;
        let heads = backend.heads()?;
        let index = plan::match_head(&heads, &connector, edid.as_deref()).ok_or_else(|| {
            let names: Vec<&str> = heads.iter().map(|h| h.name.as_str()).collect();
            format!(
                "the desktop has no output that matches {connector} (it has {})",
                names.join(", ")
            )
        })?;
        let head = &heads[index];
        let mode = plan::choose(head, width, height).ok_or_else(|| {
            format!("{connector} has no resolution that fits in {width}x{height}")
        })?;
        let (w, h) = plan::desktop_size(head, &mode);
        let changed = !mode.same(&head.current);
        if changed {
            let targets = plan::plan(&heads, index, mode, None, None);
            backend.apply(&heads, &targets)?;
            if self.saved.is_empty() {
                self.positions = Some(heads.iter().map(|h| (h.name.clone(), h.x, h.y)).collect());
            }
            if !self.saved.iter().any(|s| s.connector == connector) {
                self.saved.push(Saved {
                    connector,
                    head: head.name.clone(),
                    mode: head.current.clone(),
                    scale: head.scale,
                });
            }
        }
        Ok(Reply::Set {
            width: w,
            height: h,
            changed,
            remaining: self.saved.len(),
        })
    }

    fn restore(&mut self, connector: &str) -> Result<Reply, String> {
        let Some(at) = self.saved.iter().position(|s| s.connector == connector) else {
            return Ok(Reply::Restored {
                remaining: self.saved.len(),
            });
        };
        let result = self.put_back(at);
        self.saved.remove(at);
        if self.saved.is_empty() {
            self.positions = None;
        }
        result.map(|()| Reply::Restored {
            remaining: self.saved.len(),
        })
    }

    fn put_back(&self, at: usize) -> Result<(), String> {
        let saved = &self.saved[at];
        let mut backend = (self.open)()?;
        let heads = backend.heads()?;
        let Some(index) = heads.iter().position(|h| h.name == saved.head) else {
            return Err(format!("{} is gone", saved.head));
        };
        let head = &heads[index];
        let mode = match head.modes.iter().find(|m| m.same(&saved.mode)) {
            Some(m) if !head.custom => m.clone(),
            _ if head.custom => HeadMode {
                id: String::new(),
                ..saved.mode.clone()
            },
            _ => {
                return Err(format!(
                    "{} no longer offers {}x{}",
                    saved.head, saved.mode.width, saved.mode.height
                ));
            }
        };
        let positions = (self.saved.len() == 1)
            .then_some(self.positions.as_deref())
            .flatten();
        let targets = plan::plan(&heads, index, mode, Some(saved.scale), positions);
        if plan::unchanged(&heads, &targets) {
            return Ok(());
        }
        backend.apply(&heads, &targets)
    }

    fn restore_all(&mut self) {
        while let Some(s) = self.saved.last() {
            let connector = s.connector.clone();
            if let Err(e) = self.restore(&connector) {
                eprintln!("display helper: restore {connector}: {e}");
            }
        }
    }
}

enum Event {
    Request(Vec<u8>),
    /// Stdin closed or a termination signal arrived.
    Stop,
}

/// Blocks the termination signals in this thread and every thread it starts.
fn block_signals() -> libc::sigset_t {
    // SAFETY: the set is initialised by sigemptyset before use; pthread_sigmask only changes this
    // thread's mask.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::sigaddset(&mut set, sig);
        }
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        set
    }
}

/// Runs the helper until stdin closes or a termination signal, then puts displays back.
pub fn main() -> i32 {
    let signals = block_signals();
    let (tx, rx) = mpsc::channel();
    let stop = tx.clone();
    let spawned = std::thread::Builder::new()
        .name("ws-display-signal".into())
        .spawn(move || {
            let mut sig = 0;
            // SAFETY: `signals` is a valid set and `sig` a valid out pointer.
            unsafe { libc::sigwait(&signals, &mut sig) };
            let _ = stop.send(Event::Stop);
        })
        .and_then(|_| {
            std::thread::Builder::new()
                .name("ws-display-stdin".into())
                .spawn(move || {
                    let mut stdin = io::stdin().lock();
                    while let Ok((kind, body)) = read_frame(&mut stdin, MAX_FRAME) {
                        if kind == REQUEST && tx.send(Event::Request(body)).is_err() {
                            return;
                        }
                    }
                    let _ = tx.send(Event::Stop);
                })
        });
    if let Err(e) = spawned {
        eprintln!("display helper: {e}");
        return 1;
    }
    let mut state = State::new(open);
    let mut stdout = io::stdout().lock();
    while let Ok(Event::Request(body)) = rx.recv() {
        let reply = match serde_json::from_slice::<Request>(&body) {
            Ok(request) => state.handle(request),
            Err(e) => Reply::Failed(format!("bad request: {e}")),
        };
        let sent = serde_json::to_vec(&reply)
            .map_err(io::Error::other)
            .and_then(|body| write_frame(&mut stdout, REPLY, &body));
        if sent.is_err() {
            break;
        }
    }
    state.restore_all();
    let _ = stdout.flush();
    0
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// The fake desktop's heads, and how many configurations were applied to it.
    static DESKTOP: Mutex<(Vec<Head>, usize)> = Mutex::new((Vec::new(), 0));

    struct Fake;

    impl Backend for Fake {
        fn heads(&mut self) -> Result<Vec<Head>, String> {
            Ok(DESKTOP.lock().unwrap().0.clone())
        }

        fn apply(&mut self, heads: &[Head], targets: &[Target]) -> Result<(), String> {
            let mut desktop = DESKTOP.lock().unwrap();
            assert_eq!(desktop.0, heads);
            for (h, t) in desktop.0.iter_mut().zip(targets) {
                h.current = t.mode.clone();
                (h.x, h.y, h.scale) = (t.x, t.y, t.scale);
            }
            desktop.1 += 1;
            Ok(())
        }
    }

    fn mode(width: u32, height: u32) -> HeadMode {
        HeadMode {
            width,
            height,
            refresh_mhz: 60000,
            id: format!("{width}x{height}"),
            scales: Vec::new(),
        }
    }

    fn head(name: &str, x: i32, current: HeadMode) -> Head {
        Head {
            name: name.into(),
            edid: None,
            x,
            y: 0,
            scale: 1.0,
            rotated: false,
            logical: false,
            modes: vec![mode(2560, 1440), mode(1920, 1080), mode(1280, 720)],
            current,
            custom: false,
        }
    }

    fn set(connector: &str, width: u32, height: u32) -> Request {
        Request::Set {
            connector: connector.into(),
            edid: None,
            width,
            height,
        }
    }

    fn restore(connector: &str) -> Request {
        Request::Restore {
            connector: connector.into(),
        }
    }

    #[test]
    fn changes_and_puts_back_every_display() {
        let start = vec![
            head("DP-1", 0, mode(2560, 1440)),
            head("HDMI-1", 2560, mode(1920, 1080)),
        ];
        *DESKTOP.lock().unwrap() = (start.clone(), 0);
        let mut state = State::new(|| Ok(Box::new(Fake)));

        assert_eq!(
            state.handle(set("DP-1", 1920, 1200)),
            Reply::Set {
                width: 1920,
                height: 1080,
                changed: true,
                remaining: 1
            }
        );
        assert_eq!(DESKTOP.lock().unwrap().0[1].x, 1920);
        let reply = state.handle(set("HDMI-A-1", 1280, 720));
        assert!(matches!(reply, Reply::Set { remaining: 2, .. }));
        let reply = state.handle(set("DP-1", 1280, 800));
        assert!(matches!(reply, Reply::Set { remaining: 2, .. }));
        assert_eq!(DESKTOP.lock().unwrap().0[1].x, 1280);

        assert_eq!(
            state.handle(restore("DP-1")),
            Reply::Restored { remaining: 1 }
        );
        assert_eq!(DESKTOP.lock().unwrap().0[0], start[0]);
        assert_eq!(
            state.handle(restore("DP-1")),
            Reply::Restored { remaining: 1 }
        );
        state.restore_all();
        assert!(state.saved.is_empty());
        assert_eq!(DESKTOP.lock().unwrap().0, start);

        let applied = DESKTOP.lock().unwrap().1;
        let reply = state.handle(set("DP-1", 3840, 2160));
        assert!(matches!(
            reply,
            Reply::Set {
                changed: false,
                remaining: 0,
                ..
            }
        ));
        assert_eq!(DESKTOP.lock().unwrap().1, applied);
        assert!(matches!(
            state.handle(set("DP-1", 640, 480)),
            Reply::Failed(_)
        ));
        assert!(matches!(
            state.handle(set("VGA-1", 1280, 720)),
            Reply::Failed(_)
        ));
    }
}
