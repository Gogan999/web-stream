//! Input injection on its own thread, so a slow `SendInput` or a ViGEm
//! plug-in never holds up the media loop. One thread per connection:
//! dropping [`InputThread`] releases every held key, button and stick and
//! unplugs the virtual gamepads.
//!
//! Presses and releases (the reliable `input` channel) are never dropped: a
//! lost key-up would leave the key held. Motion (`input-motion`) is never
//! queued: pending motion is merged into one slot (relative deltas summed,
//! the newest absolute position and the newest state per gamepad kept), so
//! a backlog cannot turn into lag.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ws_input::{DisplayRect, InputHandler, InputSink};
use ws_proto::{GamepadState, HostControl, InputEvent, MAX_GAMEPADS};

use crate::platform;

/// How often stale gamepads are centred (the handler requires a timer).
const TICK: Duration = Duration::from_millis(100);
/// Injection errors (usually an elevated window in front) are logged at most
/// this often.
const ERROR_LOG_EVERY: Duration = Duration::from_secs(5);

enum Msg {
    Event(InputEvent),
    /// New motion is waiting in the slot.
    Motion,
    Display(DisplayRect),
}

/// Motion not yet injected.
#[derive(Debug, Default, PartialEq)]
struct Motion {
    dx: i32,
    dy: i32,
    abs: Option<(u16, u16)>,
    pads: [Option<GamepadState>; MAX_GAMEPADS as usize],
}

impl Motion {
    /// Merges a motion event; hands back anything that is not motion.
    fn merge(&mut self, event: InputEvent) -> Option<InputEvent> {
        match event {
            InputEvent::MouseMove { dx, dy } => {
                self.dx = self.dx.saturating_add(dx.into());
                self.dy = self.dy.saturating_add(dy.into());
            }
            InputEvent::MouseAbs { x, y } => self.abs = Some((x, y)),
            InputEvent::Gamepad { index, state } if index < MAX_GAMEPADS => {
                self.pads[index as usize] = Some(state);
            }
            other => return Some(other),
        }
        None
    }

    /// The merged motion as events, relative deltas split to fit `i16`.
    fn drain(&mut self) -> Vec<InputEvent> {
        let m = std::mem::take(self);
        let mut out = Vec::new();
        if let Some((x, y)) = m.abs {
            out.push(InputEvent::MouseAbs { x, y });
        }
        let (mut dx, mut dy) = (m.dx, m.dy);
        while dx != 0 || dy != 0 {
            let step = |v: i32| v.clamp(i16::MIN.into(), i16::MAX.into());
            let (sx, sy) = (step(dx), step(dy));
            out.push(InputEvent::MouseMove {
                dx: sx as i16,
                dy: sy as i16,
            });
            dx -= sx;
            dy -= sy;
        }
        for (index, state) in m.pads.into_iter().enumerate() {
            if let Some(state) = state {
                out.push(InputEvent::Gamepad {
                    index: index as u8,
                    state,
                });
            }
        }
        out
    }
}

struct Shared {
    motion: Mutex<Motion>,
    /// A `Msg::Motion` wake-up is queued; don't queue another.
    woken: AtomicBool,
}

pub struct InputThread {
    tx: Option<Sender<Msg>>,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl InputThread {
    /// `rumble` is called from another thread with messages for the
    /// `control` channel.
    pub fn start(
        display: Option<DisplayRect>,
        rumble: impl Fn(HostControl) + Send + Sync + 'static,
    ) -> std::io::Result<Self> {
        // Unbounded so presses are never dropped; the transport's own bounded
        // event queue limits how much can arrive.
        let (tx, rx) = channel();
        let shared = Arc::new(Shared {
            motion: Mutex::new(Motion::default()),
            woken: AtomicBool::new(false),
        });
        let thread = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("ws-input".into())
                .spawn(move || run(rx, shared, display, rumble))?
        };
        Ok(Self {
            tx: Some(tx),
            shared,
            thread: Some(thread),
        })
    }

    /// One `input` or `input-motion` message.
    pub fn send(&self, data: &[u8]) {
        let event = match InputEvent::decode(data) {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!("bad input message: {e}");
                return;
            }
        };
        let rest = self.shared.motion.lock().expect("motion lock").merge(event);
        match rest {
            Some(event) => self.push(Msg::Event(event)),
            None => {
                if !self.shared.woken.swap(true, Ordering::AcqRel) {
                    self.push(Msg::Motion);
                }
            }
        }
    }

    /// The streamed display moved; absolute mouse positions follow it.
    pub fn set_display(&self, rect: DisplayRect) {
        self.push(Msg::Display(rect));
    }

    fn push(&self, msg: Msg) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(msg);
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
    shared: Arc<Shared>,
    display: Option<DisplayRect>,
    rumble: impl Fn(HostControl) + Send + Sync + 'static,
) {
    let mut handler = InputHandler::new(platform::input_sink(rumble));
    if let Some(rect) = display {
        platform::set_display_rect(handler.sink_mut(), rect);
    }
    let mut errors = ErrorLog::default();
    let mut next_tick = Instant::now() + TICK;
    loop {
        let wait = next_tick.saturating_duration_since(Instant::now());
        match rx.recv_timeout(wait) {
            // Motion that arrived before a press is injected before it, so a
            // click lands where the pointer was moved.
            Ok(Msg::Event(event)) => {
                flush_motion(&shared, &mut handler, &mut errors);
                errors.check(handler.apply(event, Instant::now()));
            }
            Ok(Msg::Motion) => flush_motion(&shared, &mut handler, &mut errors),
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

fn flush_motion<S: InputSink>(
    shared: &Shared,
    handler: &mut InputHandler<S>,
    errors: &mut ErrorLog,
) {
    shared.woken.store(false, Ordering::Release);
    let events = shared.motion.lock().expect("motion lock").drain();
    let now = Instant::now();
    for event in events {
        errors.check(handler.apply(event, now));
    }
}

#[derive(Default)]
struct ErrorLog {
    last: Option<Instant>,
}

impl ErrorLog {
    fn check<E: std::fmt::Display>(&mut self, result: Result<(), E>) {
        if let Err(e) = result {
            if self.last.is_none_or(|t| t.elapsed() >= ERROR_LOG_EVERY) {
                tracing::warn!("input: {e}");
                self.last = Some(Instant::now());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn motion_merges_and_presses_pass_through() {
        let mut m = Motion::default();
        assert!(
            m.merge(InputEvent::MouseMove { dx: 30_000, dy: -5 })
                .is_none()
        );
        assert!(
            m.merge(InputEvent::MouseMove { dx: 30_000, dy: -5 })
                .is_none()
        );
        assert!(m.merge(InputEvent::MouseAbs { x: 1, y: 2 }).is_none());
        assert!(m.merge(InputEvent::MouseAbs { x: 3, y: 4 }).is_none());
        let pad = |buttons| InputEvent::Gamepad {
            index: 1,
            state: GamepadState {
                buttons,
                ..Default::default()
            },
        };
        assert!(m.merge(pad(1)).is_none());
        assert!(m.merge(pad(2)).is_none());
        let key = InputEvent::Key {
            usage: 4,
            down: false,
        };
        assert_eq!(m.merge(key.clone()), Some(key));

        assert_eq!(
            m.drain(),
            vec![
                InputEvent::MouseAbs { x: 3, y: 4 },
                InputEvent::MouseMove {
                    dx: i16::MAX,
                    dy: -10
                },
                InputEvent::MouseMove {
                    dx: (60_000 - i32::from(i16::MAX)) as i16,
                    dy: 0
                },
                pad(2),
            ]
        );
        assert_eq!(m, Motion::default());
    }
}
