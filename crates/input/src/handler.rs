//! Turns decoded input events into injection calls, tracking what is held
//! down so a dropped connection never leaves a key, button or stick stuck.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use thiserror::Error;
use tracing::{debug, warn};
use ws_proto::{GamepadState, InputDecodeError, InputEvent, MAX_GAMEPADS, MouseButton};

use crate::keymap::{self, KeyTarget};

#[derive(Debug, Error)]
#[error("{0}")]
pub struct SinkError(pub String);

/// Which virtual controller the host plugs in for a browser gamepad.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum GamepadKind {
    #[default]
    Xbox360,
    DualShock4,
}

/// Where input ends up. The Windows implementation is
/// [`crate::WindowsInput`]; tests use a recorder.
pub trait InputSink {
    fn key(&mut self, key: KeyTarget, down: bool) -> Result<(), SinkError>;
    fn mouse_move(&mut self, dx: i32, dy: i32) -> Result<(), SinkError>;
    /// `x` and `y` are 0..=65535 across the streamed display.
    fn mouse_move_abs(&mut self, x: u16, y: u16) -> Result<(), SinkError>;
    fn mouse_button(&mut self, button: MouseButton, down: bool) -> Result<(), SinkError>;
    /// WHEEL_DELTA units; positive `dy` scrolls away from the user.
    fn mouse_wheel(&mut self, dx: i32, dy: i32) -> Result<(), SinkError>;
    fn gamepad_connect(&mut self, index: u8, kind: GamepadKind) -> Result<(), SinkError>;
    fn gamepad_update(&mut self, index: u8, state: &GamepadState) -> Result<(), SinkError>;
    fn gamepad_disconnect(&mut self, index: u8) -> Result<(), SinkError>;
}

#[derive(Debug, Error)]
pub enum HandleError {
    #[error(transparent)]
    Decode(#[from] InputDecodeError),
    #[error(transparent)]
    Sink(#[from] SinkError),
}

/// The client resends gamepad state every 100 ms; this many missed resends
/// means the client is gone and a held stick must not keep the game moving.
pub const GAMEPAD_STALE_AFTER: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy)]
struct PadSlot {
    last_seen: Instant,
    neutral: bool,
}

pub struct InputHandler<S: InputSink> {
    sink: S,
    gamepad_kind: GamepadKind,
    keys: HashSet<KeyTarget>,
    buttons: HashSet<MouseButton>,
    pads: [Option<PadSlot>; MAX_GAMEPADS as usize],
}

impl<S: InputSink> InputHandler<S> {
    pub fn new(sink: S) -> Self {
        Self {
            sink,
            gamepad_kind: GamepadKind::default(),
            keys: HashSet::new(),
            buttons: HashSet::new(),
            pads: [None; MAX_GAMEPADS as usize],
        }
    }

    /// Kind of virtual pad plugged in for gamepads that connect from now on.
    pub fn set_gamepad_kind(&mut self, kind: GamepadKind) {
        self.gamepad_kind = kind;
    }

    pub fn sink(&self) -> &S {
        &self.sink
    }

    pub fn sink_mut(&mut self) -> &mut S {
        &mut self.sink
    }

    /// Handles one `input` or `input-motion` data channel message.
    pub fn handle(&mut self, msg: &[u8]) -> Result<(), HandleError> {
        let event = InputEvent::decode(msg)?;
        self.apply(event, Instant::now())
    }

    pub fn apply(&mut self, event: InputEvent, now: Instant) -> Result<(), HandleError> {
        match event {
            InputEvent::MouseMove { dx, dy } => self.sink.mouse_move(dx.into(), dy.into())?,
            InputEvent::MouseAbs { x, y } => self.sink.mouse_move_abs(x, y)?,
            InputEvent::MouseButton { button, down } => {
                if down {
                    self.buttons.insert(button);
                } else if !self.buttons.remove(&button) {
                    return Ok(());
                }
                self.sink.mouse_button(button, down)?;
            }
            InputEvent::Wheel { dx, dy } => self.sink.mouse_wheel(dx.into(), dy.into())?,
            InputEvent::Key { usage, down } => {
                let Some(key) = keymap::lookup(usage) else {
                    debug!(usage, "ignoring key with no Windows mapping");
                    return Ok(());
                };
                if down {
                    // Repeated downs are the browser's auto-repeat; pass them on.
                    self.keys.insert(key);
                } else if !self.keys.remove(&key) {
                    return Ok(());
                }
                self.sink.key(key, down)?;
            }
            InputEvent::Gamepad { index, state } => {
                let slot = &mut self.pads[index as usize];
                if slot.is_none() {
                    self.sink.gamepad_connect(index, self.gamepad_kind)?;
                }
                let neutral = state == GamepadState::default();
                // Keepalives repeat the same state; only changes reach the driver,
                // except right after plug-in so the pad starts in the right state.
                let changed = slot.is_none_or(|s| !(s.neutral && neutral));
                *slot = Some(PadSlot {
                    last_seen: now,
                    neutral,
                });
                if changed {
                    self.sink.gamepad_update(index, &state)?;
                }
            }
            InputEvent::GamepadDisconnect { index } => {
                if self.pads[index as usize].take().is_some() {
                    self.sink.gamepad_disconnect(index)?;
                }
            }
            InputEvent::ReleaseAll => self.release_all(),
        }
        Ok(())
    }

    /// Centres any gamepad whose state has not been refreshed for
    /// [`GAMEPAD_STALE_AFTER`]. Call it from a timer (every 100 ms or so) while
    /// a session is live, since a dead client sends nothing to trigger it.
    pub fn expire_stale_gamepads(&mut self, now: Instant) {
        for (index, slot) in self.pads.iter_mut().enumerate() {
            let Some(slot) = slot else { continue };
            if !slot.neutral && now.duration_since(slot.last_seen) >= GAMEPAD_STALE_AFTER {
                warn!(index, "gamepad state went stale, centring it");
                if let Err(e) = self
                    .sink
                    .gamepad_update(index as u8, &GamepadState::default())
                {
                    warn!(index, "gamepad reset failed: {e}");
                }
                slot.neutral = true;
            }
        }
    }

    /// Releases every held key and button and centres every gamepad. Call it
    /// when the client loses focus or the session ends; errors are logged
    /// because there is nobody left to report them to.
    pub fn release_all(&mut self) {
        for key in std::mem::take(&mut self.keys) {
            if let Err(e) = self.sink.key(key, false) {
                warn!(?key, "release failed: {e}");
            }
        }
        for button in std::mem::take(&mut self.buttons) {
            if let Err(e) = self.sink.mouse_button(button, false) {
                warn!(?button, "release failed: {e}");
            }
        }
        for (index, slot) in self.pads.iter_mut().enumerate() {
            let Some(slot) = slot else { continue };
            if !slot.neutral {
                if let Err(e) = self
                    .sink
                    .gamepad_update(index as u8, &GamepadState::default())
                {
                    warn!(index, "gamepad reset failed: {e}");
                }
                slot.neutral = true;
            }
        }
    }

    /// Releases everything and unplugs all virtual gamepads.
    pub fn shutdown(&mut self) {
        self.release_all();
        for index in 0..MAX_GAMEPADS {
            if self.pads[index as usize].take().is_some() {
                if let Err(e) = self.sink.gamepad_disconnect(index) {
                    warn!(index, "gamepad unplug failed: {e}");
                }
            }
        }
    }
}

impl<S: InputSink> Drop for InputHandler<S> {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq)]
    enum Call {
        Key(KeyTarget, bool),
        Move(i32, i32),
        Abs(u16, u16),
        Button(MouseButton, bool),
        Wheel(i32, i32),
        Connect(u8, GamepadKind),
        Update(u8, GamepadState),
        Disconnect(u8),
    }

    #[derive(Default)]
    struct Recorder(Vec<Call>);

    impl InputSink for Recorder {
        fn key(&mut self, key: KeyTarget, down: bool) -> Result<(), SinkError> {
            self.0.push(Call::Key(key, down));
            Ok(())
        }
        fn mouse_move(&mut self, dx: i32, dy: i32) -> Result<(), SinkError> {
            self.0.push(Call::Move(dx, dy));
            Ok(())
        }
        fn mouse_move_abs(&mut self, x: u16, y: u16) -> Result<(), SinkError> {
            self.0.push(Call::Abs(x, y));
            Ok(())
        }
        fn mouse_button(&mut self, b: MouseButton, down: bool) -> Result<(), SinkError> {
            self.0.push(Call::Button(b, down));
            Ok(())
        }
        fn mouse_wheel(&mut self, dx: i32, dy: i32) -> Result<(), SinkError> {
            self.0.push(Call::Wheel(dx, dy));
            Ok(())
        }
        fn gamepad_connect(&mut self, i: u8, k: GamepadKind) -> Result<(), SinkError> {
            self.0.push(Call::Connect(i, k));
            Ok(())
        }
        fn gamepad_update(&mut self, i: u8, s: &GamepadState) -> Result<(), SinkError> {
            self.0.push(Call::Update(i, *s));
            Ok(())
        }
        fn gamepad_disconnect(&mut self, i: u8) -> Result<(), SinkError> {
            self.0.push(Call::Disconnect(i));
            Ok(())
        }
    }

    fn send(h: &mut InputHandler<Recorder>, e: InputEvent) {
        h.handle(&e.encode()).unwrap();
    }

    fn take(h: &mut InputHandler<Recorder>) -> Vec<Call> {
        std::mem::take(&mut h.sink_mut().0)
    }

    const W_USAGE: u16 = 0x1a;
    const W: KeyTarget = KeyTarget::Scan {
        code: 0x11,
        extended: false,
    };

    fn pad(buttons: u16) -> GamepadState {
        GamepadState {
            buttons,
            ..Default::default()
        }
    }

    #[test]
    fn keys_and_mouse_pass_through() {
        let mut h = InputHandler::new(Recorder::default());
        send(
            &mut h,
            InputEvent::Key {
                usage: W_USAGE,
                down: true,
            },
        );
        send(
            &mut h,
            InputEvent::Key {
                usage: W_USAGE,
                down: false,
            },
        );
        send(
            &mut h,
            InputEvent::MouseButton {
                button: MouseButton::Left,
                down: true,
            },
        );
        send(&mut h, InputEvent::MouseMove { dx: 3, dy: -4 });
        send(&mut h, InputEvent::MouseAbs { x: 10, y: 20 });
        send(&mut h, InputEvent::Wheel { dx: 0, dy: 120 });
        assert_eq!(
            take(&mut h),
            [
                Call::Key(W, true),
                Call::Key(W, false),
                Call::Button(MouseButton::Left, true),
                Call::Move(3, -4),
                Call::Abs(10, 20),
                Call::Wheel(0, 120),
            ]
        );
    }

    #[test]
    fn unmatched_releases_and_unmapped_keys_are_dropped() {
        let mut h = InputHandler::new(Recorder::default());
        send(
            &mut h,
            InputEvent::Key {
                usage: W_USAGE,
                down: false,
            },
        );
        send(
            &mut h,
            InputEvent::MouseButton {
                button: MouseButton::Right,
                down: false,
            },
        );
        send(
            &mut h,
            InputEvent::Key {
                usage: 0x66,
                down: true,
            },
        ); // Power
        assert!(take(&mut h).is_empty());
    }

    #[test]
    fn first_state_plugs_in_a_pad_and_keepalives_are_not_resent() {
        let mut h = InputHandler::new(Recorder::default());
        h.set_gamepad_kind(GamepadKind::DualShock4);
        send(
            &mut h,
            InputEvent::Gamepad {
                index: 2,
                state: GamepadState::default(),
            },
        );
        send(
            &mut h,
            InputEvent::Gamepad {
                index: 2,
                state: GamepadState::default(),
            },
        );
        send(
            &mut h,
            InputEvent::Gamepad {
                index: 2,
                state: pad(0x1000),
            },
        );
        send(
            &mut h,
            InputEvent::Gamepad {
                index: 2,
                state: pad(0x1000),
            },
        );
        send(&mut h, InputEvent::GamepadDisconnect { index: 2 });
        send(&mut h, InputEvent::GamepadDisconnect { index: 2 });
        assert_eq!(
            take(&mut h),
            [
                Call::Connect(2, GamepadKind::DualShock4),
                Call::Update(2, GamepadState::default()),
                Call::Update(2, pad(0x1000)),
                // A held button's keepalive is forwarded; it is harmless and
                // repairs a lost earlier update.
                Call::Update(2, pad(0x1000)),
                Call::Disconnect(2),
            ]
        );
    }

    #[test]
    fn release_all_lets_go_of_everything_held() {
        let mut h = InputHandler::new(Recorder::default());
        send(
            &mut h,
            InputEvent::Key {
                usage: W_USAGE,
                down: true,
            },
        );
        send(
            &mut h,
            InputEvent::MouseButton {
                button: MouseButton::Left,
                down: true,
            },
        );
        send(
            &mut h,
            InputEvent::Gamepad {
                index: 0,
                state: pad(0x1000),
            },
        );
        take(&mut h);

        send(&mut h, InputEvent::ReleaseAll);
        let calls = take(&mut h);
        assert!(calls.contains(&Call::Key(W, false)));
        assert!(calls.contains(&Call::Button(MouseButton::Left, false)));
        assert!(calls.contains(&Call::Update(0, GamepadState::default())));
        assert_eq!(calls.len(), 3);

        send(&mut h, InputEvent::ReleaseAll);
        assert!(take(&mut h).is_empty(), "nothing is held any more");
    }

    #[test]
    fn stale_gamepads_are_centred() {
        let mut h = InputHandler::new(Recorder::default());
        let t0 = Instant::now();
        h.apply(
            InputEvent::Gamepad {
                index: 1,
                state: pad(0x2000),
            },
            t0,
        )
        .unwrap();
        h.apply(
            InputEvent::Gamepad {
                index: 3,
                state: GamepadState::default(),
            },
            t0,
        )
        .unwrap();
        take(&mut h);
        h.expire_stale_gamepads(t0 + Duration::from_millis(100));
        assert!(take(&mut h).is_empty());
        h.expire_stale_gamepads(t0 + GAMEPAD_STALE_AFTER);
        assert_eq!(take(&mut h), [Call::Update(1, GamepadState::default())]);
        h.expire_stale_gamepads(t0 + GAMEPAD_STALE_AFTER * 2);
        assert!(take(&mut h).is_empty());
    }

    #[test]
    fn drop_unplugs_pads_and_releases_keys() {
        let mut h = InputHandler::new(Recorder::default());
        send(
            &mut h,
            InputEvent::Key {
                usage: W_USAGE,
                down: true,
            },
        );
        send(
            &mut h,
            InputEvent::Gamepad {
                index: 3,
                state: GamepadState::default(),
            },
        );
        take(&mut h);
        h.shutdown();
        assert_eq!(take(&mut h), [Call::Key(W, false), Call::Disconnect(3)]);
    }
}
