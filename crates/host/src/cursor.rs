//! Watches the host pointer on its own thread while a browser is connected.
//! The thread keeps only the newest state in a `watch` channel; the media
//! loop sends the browser what changed, so a slow `control` channel never
//! builds a backlog of old positions.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use tokio::sync::watch;
use ws_input::cursor::CursorState;

use crate::platform;

/// About two polls per 60 Hz frame; one `GetCursorInfo` call when the
/// pointer hasn't changed shape.
const POLL: Duration = Duration::from_millis(8);

pub type CursorWatch = watch::Sender<Option<CursorState>>;

pub struct CursorThread {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl CursorThread {
    /// `None` where the platform can't read the pointer.
    pub fn start(out: CursorWatch) -> Option<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name("ws-cursor".into())
            .spawn(move || {
                let Some(mut reader) = platform::cursor_reader() else {
                    return;
                };
                while !flag.load(Ordering::Relaxed) {
                    if let Some(state) = reader() {
                        out.send_if_modified(|cur| {
                            let changed = cur.as_ref() != Some(&state);
                            if changed {
                                *cur = Some(state);
                            }
                            changed
                        });
                    }
                    std::thread::sleep(POLL);
                }
            })
            .inspect_err(|e| tracing::warn!("cursor thread: {e}"))
            .ok()?;
        Some(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for CursorThread {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
