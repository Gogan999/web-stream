//! Input injection on its own thread, so a slow `SendInput` or a ViGEm
//! plug-in never holds up the media loop. One thread per connection:
//! dropping [`InputThread`] releases every held key, button and stick and
//! unplugs the virtual gamepads.
//!
//! Presses and releases (the reliable `input` channel) are never dropped: a
//! lost key-up would leave the key held. Motion (`input-motion`) is never
//! replayed as a backlog: the thread takes everything waiting at once and
//! merges each run of motion between two presses (relative deltas summed,
//! the newest absolute position and the newest state per gamepad kept), so
//! order against presses holds and a backlog cannot turn into lag.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ws_input::{DisplayRect, InputHandler};
use ws_proto::{GamepadState, HostControl, InputEvent, MAX_GAMEPADS};

use crate::platform;

/// How often stale gamepads are centred (the handler requires a timer).
const TICK: Duration = Duration::from_millis(100);
/// Injection errors (usually an elevated window in front) are logged at most
/// this often.
const ERROR_LOG_EVERY: Duration = Duration::from_secs(5);

#[derive(Debug, PartialEq)]
enum Msg {
    Event(InputEvent),
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

pub struct InputThread {
    tx: Option<Sender<Msg>>,
    thread: Option<JoinHandle<()>>,
}

impl InputThread {
    /// `rumble` is called from another thread with messages for the
    /// `control` channel.
    pub fn start(
        display: Option<DisplayRect>,
        rumble: impl Fn(HostControl) + Send + Sync + 'static,
    ) -> std::io::Result<Self> {
        // Unbounded so presses are never dropped. It only grows while the
        // thread is stuck inside an injection call, and the transport's own
        // bounded event queue limits how fast anything arrives.
        let (tx, rx) = channel();
        let thread = std::thread::Builder::new()
            .name("ws-input".into())
            .spawn(move || run(rx, display, rumble))?;
        Ok(Self {
            tx: Some(tx),
            thread: Some(thread),
        })
    }

    /// One `input` or `input-motion` message.
    pub fn send(&self, data: &[u8]) {
        match InputEvent::decode(data) {
            Ok(event) => self.push(Msg::Event(event)),
            Err(e) => tracing::debug!("bad input message: {e}"),
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
    display: Option<DisplayRect>,
    rumble: impl Fn(HostControl) + Send + Sync + 'static,
) {
    let mut handler = InputHandler::new(platform::input_sink(rumble));
    if let Some(rect) = display {
        platform::set_display_rect(handler.sink_mut(), rect);
    }
    let mut errors = ErrorLog::default();
    let mut next_tick = Instant::now() + TICK;
    let mut open = true;
    while open {
        let wait = next_tick.saturating_duration_since(Instant::now());
        let mut batch = match rx.recv_timeout(wait) {
            Ok(msg) => vec![msg],
            Err(RecvTimeoutError::Timeout) => Vec::new(),
            Err(RecvTimeoutError::Disconnected) => break,
        };
        // Everything else already waiting, merged together.
        loop {
            match rx.try_recv() {
                Ok(msg) => batch.push(msg),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    open = false;
                    break;
                }
            }
        }
        let now = Instant::now();
        for msg in coalesce(batch) {
            match msg {
                Msg::Event(event) => errors.check(handler.apply(event, now)),
                Msg::Display(rect) => platform::set_display_rect(handler.sink_mut(), rect),
            }
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

/// Merges each run of motion in `batch`, keeping it in order with the
/// presses and display changes around it.
fn coalesce(batch: Vec<Msg>) -> Vec<Msg> {
    let mut out = Vec::with_capacity(batch.len());
    let mut motion = Motion::default();
    for msg in batch {
        let msg = match msg {
            Msg::Event(event) => match motion.merge(event) {
                None => continue,
                Some(other) => Msg::Event(other),
            },
            display => display,
        };
        out.extend(motion.drain().into_iter().map(Msg::Event));
        out.push(msg);
    }
    out.extend(motion.drain().into_iter().map(Msg::Event));
    out
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

    #[test]
    fn motion_stays_on_its_side_of_a_press() {
        use ws_proto::MouseButton;
        let abs = |x| Msg::Event(InputEvent::MouseAbs { x, y: 0 });
        let button = |down| {
            Msg::Event(InputEvent::MouseButton {
                button: MouseButton::Left,
                down,
            })
        };
        // Move to A, press, move to B (twice), release: the click is at A,
        // the release at B.
        let out = coalesce(vec![abs(1), button(true), abs(2), abs(3), button(false)]);
        assert_eq!(out, vec![abs(1), button(true), abs(3), button(false)]);
        assert_eq!(coalesce(vec![abs(1), abs(2)]), vec![abs(2)]);
    }
}
