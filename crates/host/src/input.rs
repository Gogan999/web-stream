//! Input injection on its own thread, so a slow `SendInput` or a ViGEm
//! plug-in never holds up the media loop. One thread per connection:
//! dropping [`InputThread`] releases every held key, button and stick and
//! unplugs the virtual gamepads.

use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ws_input::{DisplayRect, InputHandler};
use ws_proto::HostControl;

use crate::platform;

/// Messages waiting for injection. Past this the client is far ahead of what
/// the OS accepts and new messages are dropped.
const QUEUE: usize = 1024;
/// How often stale gamepads are centred (the handler requires a timer).
const TICK: Duration = Duration::from_millis(100);
/// Injection errors (usually an elevated window in front) are logged at most
/// this often.
const ERROR_LOG_EVERY: Duration = Duration::from_secs(5);

enum Msg {
    Data(Vec<u8>),
    Display(DisplayRect),
}

pub struct InputThread {
    tx: Option<SyncSender<Msg>>,
    thread: Option<JoinHandle<()>>,
}

impl InputThread {
    /// `rumble` is called from another thread with messages for the
    /// `control` channel.
    pub fn start(
        display: Option<DisplayRect>,
        rumble: impl Fn(HostControl) + Send + Sync + 'static,
    ) -> std::io::Result<Self> {
        let (tx, rx) = sync_channel(QUEUE);
        let thread = std::thread::Builder::new()
            .name("ws-input".into())
            .spawn(move || run(rx, display, rumble))?;
        Ok(Self {
            tx: Some(tx),
            thread: Some(thread),
        })
    }

    /// One `input` or `input-motion` message.
    pub fn send(&self, data: Vec<u8>) {
        self.push(Msg::Data(data));
    }

    /// The streamed display moved; absolute mouse positions follow it.
    pub fn set_display(&self, rect: DisplayRect) {
        self.push(Msg::Display(rect));
    }

    fn push(&self, msg: Msg) {
        if let Some(tx) = &self.tx {
            if let Err(TrySendError::Full(_)) = tx.try_send(msg) {
                tracing::warn!("input queue full; dropping input");
            }
        }
    }
}

impl Drop for InputThread {
    fn drop(&mut self) {
        // Closing the channel ends the thread, which releases everything.
        self.tx.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(
    rx: Receiver<Msg>,
    display: Option<DisplayRect>,
    rumble: impl Fn(HostControl) + Send + Sync + 'static,
) {
    let mut handler = InputHandler::new(platform::input_sink(rumble));
    if let Some(rect) = display {
        platform::set_display_rect(handler.sink_mut(), rect);
    }
    let mut last_error_log: Option<Instant> = None;
    let mut next_tick = Instant::now() + TICK;
    loop {
        let wait = next_tick.saturating_duration_since(Instant::now());
        match rx.recv_timeout(wait) {
            Ok(Msg::Data(data)) => {
                if let Err(e) = handler.handle(&data) {
                    if last_error_log.is_none_or(|t| t.elapsed() >= ERROR_LOG_EVERY) {
                        tracing::warn!("input: {e}");
                        last_error_log = Some(Instant::now());
                    }
                }
            }
            Ok(Msg::Display(rect)) => platform::set_display_rect(handler.sink_mut(), rect),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        let now = Instant::now();
        if now >= next_tick {
            handler.expire_stale_gamepads(now);
            next_tick = now + TICK;
        }
    }
    // Dropping the handler releases keys and unplugs pads.
    drop(handler);
}
