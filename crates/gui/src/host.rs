//! Runs the host on its own thread and keeps what the window shows.

use std::collections::VecDeque;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use ws_host::options::ServeOptions;
use ws_host::serve::{self, Command, Event, FileEvent, FileId, Notice};

const LOG_LINES: usize = 200;
const PAIRING_OPEN: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Stopped,
    Starting,
    Running {
        port: u16,
        ips: Vec<IpAddr>,
        fingerprint: String,
        web_root: Option<PathBuf>,
    },
    Stopping,
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct PairingCode {
    pub code: String,
    pub device_name: String,
    pub expires: Instant,
}

/// A file shared with the connected browser.
#[derive(Debug, Clone, PartialEq)]
pub struct SharedFile {
    pub id: FileId,
    pub name: String,
    pub size: u64,
    pub status: FileStatus,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FileStatus {
    Offered,
    /// The browser has stored this many bytes.
    Sending(u64),
    Sent,
    Cancelled,
    Failed(String),
}

#[derive(Debug)]
pub struct State {
    pub status: Status,
    /// When pairing closes again, while it is open.
    pub pairing_until: Option<Instant>,
    pub code: Option<PairingCode>,
    pub pairing_locked: bool,
    /// The browser streaming right now.
    pub connected: Option<String>,
    /// Files on offer to the connected browser.
    pub shared: Vec<SharedFile>,
    pub log: VecDeque<String>,
}

impl State {
    fn log(&mut self, line: impl Into<String>) {
        let line = line.into();
        tracing::info!("{line}");
        if self.log.len() == LOG_LINES {
            self.log.pop_front();
        }
        self.log.push_back(line);
    }

    fn apply(&mut self, event: Event) {
        match event {
            Event::Started {
                port,
                ips,
                fingerprint,
                web_root,
                ..
            } => {
                self.log(format!("hosting on port {port}"));
                if web_root.is_none() {
                    self.log("no web client found: set the web client folder in Settings");
                }
                self.status = Status::Running {
                    port,
                    ips,
                    fingerprint,
                    web_root,
                };
            }
            Event::PairingOpened => {
                self.pairing_until = Some(Instant::now() + PAIRING_OPEN);
                self.pairing_locked = false;
                self.log("pairing is open for two minutes: request a code in the browser");
            }
            Event::UnpairedAll => self.log("every paired browser was removed"),
            Event::Error(e) => self.log(e),
            Event::File(f) => self.file(f),
            Event::Notice(n) => match n {
                Notice::PairingCode {
                    code,
                    device_name,
                    expires_in_secs,
                } => {
                    self.log(format!("pairing code for {device_name:?}"));
                    self.code = Some(PairingCode {
                        code,
                        device_name,
                        expires: Instant::now() + Duration::from_secs(expires_in_secs),
                    });
                }
                Notice::Paired { device_name } => {
                    self.code = None;
                    self.pairing_until = None;
                    self.log(format!("paired {device_name:?}"));
                }
                Notice::PairingFailed { attempts_left } => {
                    self.log(format!("wrong pairing code; {attempts_left} attempts left"))
                }
                Notice::PairingLocked => {
                    self.code = None;
                    self.pairing_until = None;
                    self.pairing_locked = true;
                    self.log("too many wrong codes: pairing is locked until you open it again");
                }
                Notice::SessionStarted { device_name } => {
                    self.log(format!("{device_name:?} connected"));
                    self.connected = Some(device_name);
                }
                Notice::SessionEnded { device_name } => {
                    self.log(format!("{device_name:?} disconnected"));
                    if self.connected.as_ref() == Some(&device_name) {
                        self.connected = None;
                    }
                }
            },
        }
    }

    fn file(&mut self, event: FileEvent) {
        let set = |shared: &mut Vec<SharedFile>, id: FileId, status: FileStatus| {
            if let Some(f) = shared.iter_mut().find(|f| f.id == id) {
                f.status = status;
            }
        };
        match event {
            FileEvent::Shared { id, name, size } => {
                self.log(format!("sharing {name:?} with the browser"));
                self.shared.push(SharedFile {
                    id,
                    name,
                    size,
                    status: FileStatus::Offered,
                });
            }
            FileEvent::Progress { id, received, .. } => {
                set(&mut self.shared, id, FileStatus::Sending(received))
            }
            FileEvent::Sent { id, name } => {
                self.log(format!("sent {name:?}"));
                set(&mut self.shared, id, FileStatus::Sent);
            }
            FileEvent::Cancelled { id, .. } => set(&mut self.shared, id, FileStatus::Cancelled),
            FileEvent::Failed { id, name, message } => {
                self.log(format!("sending {name:?} failed: {message}"));
                set(&mut self.shared, id, FileStatus::Failed(message));
            }
            FileEvent::Withdrawn { id, .. } => self.shared.retain(|f| f.id != id),
            FileEvent::Cleared => self.shared.clear(),
        }
    }

    /// Drops pairing state that has run out.
    pub fn expire(&mut self, now: Instant) {
        if self.pairing_until.is_some_and(|t| t <= now) {
            self.pairing_until = None;
        }
        if self.code.as_ref().is_some_and(|c| c.expires <= now) {
            self.code = None;
        }
    }

    fn stopped(&mut self, result: anyhow::Result<()>) {
        self.pairing_until = None;
        self.code = None;
        self.connected = None;
        self.shared.clear();
        match result {
            Ok(()) => {
                self.status = Status::Stopped;
                self.log("host stopped");
            }
            Err(e) => {
                let e = format!("{e:#}");
                self.log(format!("host stopped: {e}"));
                self.status = Status::Failed(e);
            }
        }
    }
}

/// Called on the host's thread after each event, and with `None` when the
/// host has stopped, for the window to repaint (and to come forward when a
/// pairing code arrives).
pub type OnChange = Arc<dyn Fn(Option<&Event>) + Send + Sync>;

struct Shared {
    state: Mutex<State>,
    commands: Mutex<Option<mpsc::Sender<Command>>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

/// Cheap to clone; the tray menu holds one too.
#[derive(Clone)]
pub struct Host {
    shared: Arc<Shared>,
    on_change: OnChange,
}

impl Host {
    pub fn new(on_change: OnChange) -> Self {
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    status: Status::Stopped,
                    pairing_until: None,
                    code: None,
                    pairing_locked: false,
                    connected: None,
                    shared: Vec::new(),
                    log: VecDeque::new(),
                }),
                commands: Mutex::new(None),
                thread: Mutex::new(None),
            }),
            on_change,
        }
    }

    pub fn state(&self) -> MutexGuard<'_, State> {
        self.shared.state.lock().expect("host state lock")
    }

    pub fn log(&self, line: impl Into<String>) {
        self.state().log(line);
    }

    /// True while a host thread exists (starting, running or stopping).
    pub fn is_active(&self) -> bool {
        let mut thread = self.shared.thread.lock().expect("host thread lock");
        match thread.as_ref() {
            Some(t) if t.is_finished() => {
                if let Some(t) = thread.take() {
                    let _ = t.join();
                }
                false
            }
            Some(_) => true,
            None => false,
        }
    }

    /// Starts the host unless one is still running.
    pub fn start(&self, opts: ServeOptions) {
        if self.is_active() {
            return;
        }
        let (commands, commands_rx) = mpsc::channel(8);
        *self.shared.commands.lock().expect("host commands lock") = Some(commands);
        self.state().status = Status::Starting;
        let host = self.clone();
        let thread = std::thread::Builder::new()
            .name("ws-host".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    host.run(opts, commands_rx)
                }))
                .unwrap_or_else(|_| Err(anyhow::anyhow!("the host crashed (see the log)")));
                host.state().stopped(result);
                (host.on_change)(None);
            })
            .expect("spawn host thread");
        *self.shared.thread.lock().expect("host thread lock") = Some(thread);
    }

    /// One runtime per run: dropping it closes every socket the host opened,
    /// so the next start can bind the same ports.
    fn run(&self, opts: ServeOptions, commands: mpsc::Receiver<Command>) -> anyhow::Result<()> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        let (events_tx, mut events) = mpsc::unbounded_channel::<Event>();
        let host = self.clone();
        let pump = rt.spawn(async move {
            while let Some(e) = events.recv().await {
                host.state().apply(e.clone());
                (host.on_change)(Some(&e));
            }
        });
        let result = rt.block_on(serve::serve(opts, commands, events_tx));
        // Show what the host said last (a final error, say) before stopping.
        rt.block_on(async {
            let _ = tokio::time::timeout(Duration::from_millis(500), pump).await;
        });
        rt.shutdown_timeout(Duration::from_secs(2));
        result
    }

    pub fn command(&self, cmd: Command) {
        if let Some(c) = self
            .shared
            .commands
            .lock()
            .expect("host commands lock")
            .as_ref()
        {
            if let Err(e) = c.try_send(cmd) {
                tracing::warn!(cmd = ?e.into_inner(), "host is not taking commands");
            }
        }
    }

    /// Asks the host to stop; [`Host::is_active`] turns false when it has.
    pub fn stop(&self) {
        if !self.is_active() {
            return;
        }
        self.state().status = Status::Stopping;
        self.command(Command::Quit);
        self.shared
            .commands
            .lock()
            .expect("host commands lock")
            .take();
    }

    /// Stops the host and waits for it, so capture is stopped and a changed
    /// display resolution put back before the app exits.
    pub fn stop_and_wait(&self, timeout: Duration) {
        self.stop();
        let end = Instant::now() + timeout;
        while self.is_active() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_serves_and_stops_twice_on_the_same_port() {
        let dir = std::env::temp_dir().join(format!("ws-gui-host-{}", std::process::id()));
        let changes = Arc::new(Mutex::new(0usize));
        let counter = changes.clone();
        let host = Host::new(Arc::new(move |_| *counter.lock().unwrap() += 1));
        let args: Vec<String> = [
            "--port",
            "18943",
            "--ip",
            "127.0.0.1",
            "--no-mic",
            "--no-audio",
            "--data-dir",
        ]
        .iter()
        .map(|s| s.to_string())
        .chain([dir.display().to_string()])
        .collect();
        let opts = ServeOptions::parse(&args).unwrap();

        for _ in 0..2 {
            host.start(opts.clone());
            let end = Instant::now() + Duration::from_secs(20);
            while !matches!(host.state().status, Status::Running { .. }) {
                assert!(
                    Instant::now() < end,
                    "never started: {:?}",
                    host.state().status
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            host.command(Command::OpenPairing);
            let end = Instant::now() + Duration::from_secs(5);
            while host.state().pairing_until.is_none() {
                assert!(Instant::now() < end, "pairing never opened");
                std::thread::sleep(Duration::from_millis(20));
            }
            host.stop_and_wait(Duration::from_secs(10));
            assert!(!host.is_active());
            assert_eq!(host.state().status, Status::Stopped);
        }
        assert!(*changes.lock().unwrap() >= 4);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
