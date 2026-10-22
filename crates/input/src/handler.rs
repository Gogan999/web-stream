//! Turns decoded input events into injection calls, tracking what is held
//! down so a dropped connection never leaves a key, button or stick stuck.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use thiserror::Error;
use tracing::{debug, warn};
use ws_proto::{GamepadState, InputDecodeError, InputEvent, MAX_GAMEPADS, MouseButton, seq_newer};

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

/// After a disconnect, gamepad state still in flight on the unordered channel
/// is ignored for this long so it cannot plug the pad back in. A failed
/// plug-in waits this long before the next attempt.
pub const GAMEPAD_HOLDOFF: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy)]
struct PadSlot {
    last_seen: Instant,
    neutral: bool,
    /// What the virtual pad was last set to.
    state: GamepadState,
    /// Sequence number of the newest snapshot applied, if numbered.
    seq: Option<u16>,
}

pub struct InputHandler<S: InputSink> {
    sink: S,
    gamepad_kind: GamepadKind,
    keys: HashSet<KeyTarget>,
    buttons: HashSet<MouseButton>,
    pads: [Option<PadSlot>; MAX_GAMEPADS as usize],
    /// Per index: ignore gamepad state until this instant.
    holdoff: [Option<Instant>; MAX_GAMEPADS as usize],
    /// Sequence number of the newest numbered absolute move applied.
    abs_seq: Option<u16>,
}

impl<S: InputSink> InputHandler<S> {
    pub fn new(sink: S) -> Self {
        Self {
            sink,
            gamepad_kind: GamepadKind::default(),
            keys: HashSet::new(),
            buttons: HashSet::new(),
            pads: [None; MAX_GAMEPADS as usize],
            holdoff: [None; MAX_GAMEPADS as usize],
            abs_seq: None,
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
            InputEvent::MouseAbs { x, y, seq } => {
                // One overtaken by a newer position is stale. The browser
                // numbers its reliable positions (ahead of a touch click)
                // too, so a lossy one still in flight can't follow them.
                // Unnumbered ones (from older pages) always apply.
                if let Some(seq) = seq {
                    if self.abs_seq.is_some_and(|last| !seq_newer(seq, last)) {
                        return Ok(());
                    }
                    self.abs_seq = Some(seq);
                }
                self.sink.mouse_move_abs(x, y)?
            }
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
            InputEvent::Gamepad { index, state, seq } => {
                let last = self.pads[index as usize].and_then(|s| s.seq);
                if let (Some(seq), Some(last)) = (seq, last) {
                    // Overtaken on the unordered channel by a newer snapshot.
                    if !seq_newer(seq, last) {
                        return Ok(());
                    }
                }
                self.apply_pad(index, state, seq.or(last), now)?;
            }
            InputEvent::GamepadButtons { index, state, seq } => {
                // In order on the reliable channel, so its buttons always
                // apply: a press or release is never lost. Its sticks only
                // if no newer snapshot has already moved them.
                let (state, seq) = match self.pads[index as usize] {
                    Some(slot) if slot.seq.is_some_and(|last| !seq_newer(seq, last)) => (
                        GamepadState {
                            buttons: state.buttons,
                            ..slot.state
                        },
                        slot.seq,
                    ),
                    _ => (state, Some(seq)),
                };
                self.apply_pad(index, state, seq, now)?;
            }
            InputEvent::GamepadDisconnect { index } => {
                self.holdoff[index as usize] = Some(now + GAMEPAD_HOLDOFF);
                if self.pads[index as usize].take().is_some() {
                    self.sink.gamepad_disconnect(index)?;
                }
            }
            InputEvent::ReleaseAll => self.release_all(),
        }
        Ok(())
    }

    /// Sets pad `index` to `state`, plugging it in first if needed.
    fn apply_pad(
        &mut self,
        index: u8,
        state: GamepadState,
        seq: Option<u16>,
        now: Instant,
    ) -> Result<(), HandleError> {
        let i = index as usize;
        if self.holdoff[i].is_some_and(|until| now < until) {
            return Ok(());
        }
        let slot = &mut self.pads[i];
        if slot.is_none() {
            if let Err(e) = self.sink.gamepad_connect(index, self.gamepad_kind) {
                self.holdoff[i] = Some(now + GAMEPAD_HOLDOFF);
                return Err(e.into());
            }
        }
        let neutral = state == GamepadState::default();
        // Keepalives repeat the same state; only changes reach the driver,
        // except right after plug-in so the pad starts in the right state.
        let changed = slot.is_none_or(|s| !(s.neutral && neutral));
        *slot = Some(PadSlot {
            last_seen: now,
            neutral,
            state,
            seq,
        });
        if changed {
            if let Err(e) = self.sink.gamepad_update(index, &state) {
                // Plug-in failed or the pad vanished: forget it and
                // try plugging in again after the hold-off.
                self.pads[i] = None;
                self.holdoff[i] = Some(now + GAMEPAD_HOLDOFF);
                return Err(e.into());
            }
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
                slot.state = GamepadState::default();
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
                slot.state = GamepadState::default();
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
        send(
            &mut h,
            InputEvent::MouseAbs {
                x: 10,
                y: 20,
                seq: None,
            },
        );
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
                seq: None,
            },
        );
        send(
            &mut h,
            InputEvent::Gamepad {
                index: 2,
                state: GamepadState::default(),
                seq: None,
            },
        );
        send(
            &mut h,
            InputEvent::Gamepad {
                index: 2,
                state: pad(0x1000),
                seq: None,
            },
        );
        send(
            &mut h,
            InputEvent::Gamepad {
                index: 2,
                state: pad(0x1000),
                seq: None,
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

    #[derive(Default)]
    struct NoVigem(u32);

    impl InputSink for NoVigem {
        fn key(&mut self, _: KeyTarget, _: bool) -> Result<(), SinkError> {
            Ok(())
        }
        fn mouse_move(&mut self, _: i32, _: i32) -> Result<(), SinkError> {
            Ok(())
        }
        fn mouse_move_abs(&mut self, _: u16, _: u16) -> Result<(), SinkError> {
            Ok(())
        }
        fn mouse_button(&mut self, _: MouseButton, _: bool) -> Result<(), SinkError> {
            Ok(())
        }
        fn mouse_wheel(&mut self, _: i32, _: i32) -> Result<(), SinkError> {
            Ok(())
        }
        fn gamepad_connect(&mut self, _: u8, _: GamepadKind) -> Result<(), SinkError> {
            self.0 += 1;
            Err(SinkError("no driver".into()))
        }
        fn gamepad_update(&mut self, _: u8, _: &GamepadState) -> Result<(), SinkError> {
            Ok(())
        }
        fn gamepad_disconnect(&mut self, _: u8) -> Result<(), SinkError> {
            Ok(())
        }
    }

    #[test]
    fn late_state_after_disconnect_does_not_replug() {
        let mut h = InputHandler::new(Recorder::default());
        let t0 = Instant::now();
        let state = InputEvent::Gamepad {
            index: 0,
            state: pad(0x1000),
            seq: None,
        };
        h.apply(state.clone(), t0).unwrap();
        h.apply(InputEvent::GamepadDisconnect { index: 0 }, t0)
            .unwrap();
        take(&mut h);
        // Reordered on the unordered channel: arrives after the disconnect.
        h.apply(state.clone(), t0 + Duration::from_millis(5))
            .unwrap();
        assert!(take(&mut h).is_empty());
        // A pad that really reconnects later is plugged back in.
        h.apply(state, t0 + GAMEPAD_HOLDOFF).unwrap();
        assert_eq!(
            take(&mut h),
            [
                Call::Connect(0, GamepadKind::Xbox360),
                Call::Update(0, pad(0x1000))
            ]
        );
    }

    #[test]
    fn failed_plug_in_is_not_retried_on_every_keepalive() {
        let mut h = InputHandler::new(NoVigem::default());
        let t0 = Instant::now();
        let state = InputEvent::Gamepad {
            index: 1,
            state: pad(0),
            seq: None,
        };
        assert!(h.apply(state.clone(), t0).is_err());
        for ms in [100, 200, 900] {
            h.apply(state.clone(), t0 + Duration::from_millis(ms))
                .unwrap();
        }
        assert_eq!(h.sink().0, 1);
        assert!(h.apply(state, t0 + GAMEPAD_HOLDOFF).is_err());
        assert_eq!(h.sink().0, 2);
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
                seq: None,
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
                seq: None,
            },
            t0,
        )
        .unwrap();
        h.apply(
            InputEvent::Gamepad {
                index: 3,
                state: GamepadState::default(),
                seq: None,
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
                seq: None,
            },
        );
        take(&mut h);
        h.shutdown();
        assert_eq!(take(&mut h), [Call::Key(W, false), Call::Disconnect(3)]);
    }

    #[test]
    fn overtaken_motion_is_ignored() {
        let mut h = InputHandler::new(Recorder::default());
        let abs = |x, seq| InputEvent::MouseAbs { x, y: 0, seq };
        send(&mut h, abs(1, Some(10)));
        send(&mut h, abs(2, Some(9)));
        send(&mut h, abs(3, Some(11)));
        // Unnumbered (ahead of a touch click) always applies.
        send(&mut h, abs(4, None));
        assert_eq!(
            take(&mut h),
            [Call::Abs(1, 0), Call::Abs(3, 0), Call::Abs(4, 0)]
        );

        let state = |buttons, seq| InputEvent::Gamepad {
            index: 0,
            state: pad(buttons),
            seq: Some(seq),
        };
        send(&mut h, state(0x1000, 5));
        send(&mut h, state(0x2000, 4));
        assert_eq!(
            take(&mut h),
            [
                Call::Connect(0, GamepadKind::default()),
                Call::Update(0, pad(0x1000))
            ]
        );
    }

    #[test]
    fn reliable_button_changes_are_never_lost() {
        let mut h = InputHandler::new(Recorder::default());
        let moved = GamepadState {
            lx: 1000,
            ..Default::default()
        };
        send(
            &mut h,
            InputEvent::Gamepad {
                index: 0,
                state: moved,
                seq: Some(12),
            },
        );
        take(&mut h);
        // A tap (press 10, release 11) arrives after snapshot 12 on the
        // unordered channel: both edges still apply, keeping 12's stick.
        for (buttons, seq) in [(0x1000, 10), (0, 11)] {
            send(
                &mut h,
                InputEvent::GamepadButtons {
                    index: 0,
                    state: pad(buttons),
                    seq,
                },
            );
        }
        let pressed = GamepadState {
            buttons: 0x1000,
            ..moved
        };
        assert_eq!(
            take(&mut h),
            [Call::Update(0, pressed), Call::Update(0, moved)]
        );
        // A newer one applies whole.
        send(
            &mut h,
            InputEvent::GamepadButtons {
                index: 0,
                state: pad(0x2000),
                seq: 13,
            },
        );
        assert_eq!(take(&mut h), [Call::Update(0, pad(0x2000))]);
    }
}
