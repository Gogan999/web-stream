//! Windows only: keeps up with the web-stream service. While it is
//! installed, this app never runs a host of its own (they would share the
//! ports, and a browser would reach whichever got them first). Run as an
//! elevated administrator, it attaches to the service's host instead and
//! shows it as its own: pairing, the connected browser, files.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ws_host::serve::Command;

use crate::host::{Host, Status};

/// How often the service is asked about.
const POLL: Duration = Duration::from_secs(3);

/// Cheap to clone.
#[derive(Clone)]
pub struct Watcher {
    /// The service: installed (`Some`), and running.
    installed: Arc<Mutex<Option<bool>>>,
    poke: mpsc::Sender<()>,
    /// Open pairing once attached (`--pair`, or "Pair a browser" in the
    /// copy that handed over).
    pair: Arc<AtomicBool>,
    pub elevated: bool,
}

impl Watcher {
    /// Asks about the service once before returning, so the app knows
    /// whether it may host, then keeps asking on a thread of its own: the
    /// window doesn't run frames while it is hidden in the tray.
    pub fn start(host: Host, pair: bool, repaint: impl Fn() + Send + 'static) -> Self {
        let (poke, pokes) = mpsc::channel();
        let watcher = Self {
            installed: Arc::new(Mutex::new(query())),
            poke,
            pair: Arc::new(AtomicBool::new(pair)),
            elevated: crate::win::is_elevated(),
        };
        let w = watcher.clone();
        let spawned = std::thread::Builder::new()
            .name("ws-service-watch".into())
            .spawn(move || {
                loop {
                    let now = query();
                    let changed = {
                        let mut installed = w.installed.lock().expect("service lock");
                        std::mem::replace(&mut *installed, now) != now
                    };
                    w.act(&host, now);
                    if changed {
                        repaint();
                    }
                    match pokes.recv_timeout(POLL) {
                        Ok(()) | Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
            });
        if let Err(e) = spawned {
            tracing::warn!("could not watch the service: {e}");
        }
        watcher
    }

    /// The service: installed (`Some`), and running.
    pub fn installed(&self) -> Option<bool> {
        *self.installed.lock().expect("service lock")
    }

    /// Asks about the service again now (after starting a change to it).
    pub fn refresh(&self) {
        let _ = self.poke.send(());
    }

    fn act(&self, host: &Host, installed: Option<bool>) {
        match installed {
            None => {
                if host.attached() {
                    host.log("the service was uninstalled");
                    host.stop();
                }
            }
            Some(running) => {
                if host.is_active() && !host.state().service {
                    host.log(
                        "the Windows service is installed and hosts this PC: stopping this \
                         app's own host, so only one runs",
                    );
                    host.stop();
                } else if running && self.elevated && !host.is_active() {
                    host.attach();
                } else if host.attached()
                    && matches!(host.state().status, Status::Running { .. })
                    && self.pair.swap(false, Ordering::Relaxed)
                {
                    // Only once attached: an attempt that fails would lose
                    // the command.
                    host.command(Command::OpenPairing);
                }
            }
        }
    }
}

fn query() -> Option<bool> {
    ws_host::service::query().unwrap_or_else(|e| {
        tracing::warn!("could not ask about the service: {e:#}");
        None
    })
}
