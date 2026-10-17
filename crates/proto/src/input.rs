//! Input from the browser, injected on the host by `ws-input`.
//!
//! Binary on the wire: one data channel message per event, little-endian,
//! first byte is the type. Motion and gamepad state ride the unordered,
//! unreliable [`crate::channels::INPUT_MOTION`] channel; everything else rides
//! the reliable [`crate::channels::INPUT`] one. The byte layouts are the table
//! in `docs/protocol.md`.

use std::fmt;

/// Mouse buttons, numbered like `MouseEvent.button` in the browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    Back,
    Forward,
}

impl MouseButton {
    fn from_wire(b: u8) -> Option<Self> {
        Some(match b {
            0 => Self::Left,
            1 => Self::Middle,
            2 => Self::Right,
            3 => Self::Back,
            4 => Self::Forward,
            _ => return None,
        })
    }

    fn to_wire(self) -> u8 {
        match self {
            Self::Left => 0,
            Self::Middle => 1,
            Self::Right => 2,
            Self::Back => 3,
            Self::Forward => 4,
        }
    }
}

/// Gamepad slots, matching XInput's four users.
pub const MAX_GAMEPADS: u8 = 4;

/// Full controller snapshot in XInput layout.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GamepadState {
    /// `XINPUT_GAMEPAD_*` flags, plus 0x0400 for Guide.
    pub buttons: u16,
    /// Sticks use XInput's sign: +y is up.
    pub lx: i16,
    pub ly: i16,
    pub rx: i16,
    pub ry: i16,
    pub lt: u8,
    pub rt: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    /// Relative motion (pointer lock).
    MouseMove {
        dx: i16,
        dy: i16,
    },
    /// Absolute position, 0..=65535 across the active display.
    MouseAbs {
        x: u16,
        y: u16,
    },
    MouseButton {
        button: MouseButton,
        down: bool,
    },
    /// WHEEL_DELTA units (120 per notch); +dy away from the user, +dx right.
    Wheel {
        dx: i16,
        dy: i16,
    },
    /// USB HID usage on the Keyboard page (0x07), i.e. the physical key, so
    /// mapping is layout independent.
    Key {
        usage: u16,
        down: bool,
    },
    /// Full state on every change and every 100 ms; the first state for an
    /// index plugs in a virtual pad.
    Gamepad {
        index: u8,
        state: GamepadState,
    },
    GamepadDisconnect {
        index: u8,
    },
    /// Release every held key and button (focus lost, tab hidden).
    ReleaseAll,
}

pub mod wire {
    pub const MOUSE_MOVE: u8 = 0x01;
    pub const MOUSE_ABS: u8 = 0x02;
    pub const MOUSE_BUTTON: u8 = 0x03;
    pub const WHEEL: u8 = 0x04;
    pub const KEY: u8 = 0x05;
    pub const GAMEPAD: u8 = 0x06;
    pub const GAMEPAD_DISCONNECT: u8 = 0x07;
    pub const RELEASE_ALL: u8 = 0x08;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputDecodeError {
    Empty,
    UnknownType(u8),
    Length {
        kind: u8,
        expected: usize,
        got: usize,
    },
    Invalid(&'static str),
}

impl fmt::Display for InputDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "empty input message"),
            Self::UnknownType(t) => write!(f, "unknown input message type 0x{t:02x}"),
            Self::Length {
                kind,
                expected,
                got,
            } => {
                write!(
                    f,
                    "input message 0x{kind:02x} must be {expected} bytes, got {got}"
                )
            }
            Self::Invalid(what) => write!(f, "invalid input message: {what}"),
        }
    }
}

impl std::error::Error for InputDecodeError {}

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}

fn i16_at(b: &[u8], i: usize) -> i16 {
    u16_at(b, i) as i16
}

fn bool_at(b: &[u8], i: usize) -> Result<bool, InputDecodeError> {
    match b[i] {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(InputDecodeError::Invalid("down flag must be 0 or 1")),
    }
}

fn pad_at(b: &[u8], i: usize) -> Result<u8, InputDecodeError> {
    match b[i] {
        n if n < MAX_GAMEPADS => Ok(n),
        _ => Err(InputDecodeError::Invalid("gamepad index out of range")),
    }
}

impl InputEvent {
    /// Decodes one data channel message. Anything malformed is rejected
    /// whole; the host never injects a partial event.
    pub fn decode(msg: &[u8]) -> Result<Self, InputDecodeError> {
        let &kind = msg.first().ok_or(InputDecodeError::Empty)?;
        let expected = match kind {
            wire::MOUSE_MOVE | wire::MOUSE_ABS | wire::WHEEL => 5,
            wire::MOUSE_BUTTON => 3,
            wire::KEY => 4,
            wire::GAMEPAD => 14,
            wire::GAMEPAD_DISCONNECT => 2,
            wire::RELEASE_ALL => 1,
            other => return Err(InputDecodeError::UnknownType(other)),
        };
        if msg.len() != expected {
            return Err(InputDecodeError::Length {
                kind,
                expected,
                got: msg.len(),
            });
        }
        let b = msg;
        Ok(match kind {
            wire::MOUSE_MOVE => Self::MouseMove {
                dx: i16_at(b, 1),
                dy: i16_at(b, 3),
            },
            wire::MOUSE_ABS => Self::MouseAbs {
                x: u16_at(b, 1),
                y: u16_at(b, 3),
            },
            wire::MOUSE_BUTTON => Self::MouseButton {
                button: MouseButton::from_wire(b[1])
                    .ok_or(InputDecodeError::Invalid("unknown mouse button"))?,
                down: bool_at(b, 2)?,
            },
            wire::WHEEL => Self::Wheel {
                dx: i16_at(b, 1),
                dy: i16_at(b, 3),
            },
            wire::KEY => Self::Key {
                usage: u16_at(b, 1),
                down: bool_at(b, 3)?,
            },
            wire::GAMEPAD => Self::Gamepad {
                index: pad_at(b, 1)?,
                state: GamepadState {
                    buttons: u16_at(b, 2),
                    lx: i16_at(b, 4),
                    ly: i16_at(b, 6),
                    rx: i16_at(b, 8),
                    ry: i16_at(b, 10),
                    lt: b[12],
                    rt: b[13],
                },
            },
            wire::GAMEPAD_DISCONNECT => Self::GamepadDisconnect {
                index: pad_at(b, 1)?,
            },
            _ => Self::ReleaseAll,
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(14);
        let pair = |out: &mut Vec<u8>, kind: u8, a: [u8; 2], b: [u8; 2]| {
            out.push(kind);
            out.extend_from_slice(&a);
            out.extend_from_slice(&b);
        };
        match *self {
            Self::MouseMove { dx, dy } => pair(
                &mut out,
                wire::MOUSE_MOVE,
                dx.to_le_bytes(),
                dy.to_le_bytes(),
            ),
            Self::MouseAbs { x, y } => {
                pair(&mut out, wire::MOUSE_ABS, x.to_le_bytes(), y.to_le_bytes())
            }
            Self::MouseButton { button, down } => {
                out.extend_from_slice(&[wire::MOUSE_BUTTON, button.to_wire(), down as u8])
            }
            Self::Wheel { dx, dy } => {
                pair(&mut out, wire::WHEEL, dx.to_le_bytes(), dy.to_le_bytes())
            }
            Self::Key { usage, down } => {
                out.push(wire::KEY);
                out.extend_from_slice(&usage.to_le_bytes());
                out.push(down as u8);
            }
            Self::Gamepad { index, state: s } => {
                out.extend_from_slice(&[wire::GAMEPAD, index]);
                for v in [
                    s.buttons,
                    s.lx as u16,
                    s.ly as u16,
                    s.rx as u16,
                    s.ry as u16,
                ] {
                    out.extend_from_slice(&v.to_le_bytes());
                }
                out.extend_from_slice(&[s.lt, s.rt]);
            }
            Self::GamepadDisconnect { index } => {
                out.extend_from_slice(&[wire::GAMEPAD_DISCONNECT, index])
            }
            Self::ReleaseAll => out.push(wire::RELEASE_ALL),
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_match_protocol_doc() {
        use InputEvent::*;
        let cases: [(InputEvent, &[u8]); 8] = [
            (MouseMove { dx: 1, dy: -1 }, &[0x01, 1, 0, 0xff, 0xff]),
            (MouseAbs { x: 0, y: 65535 }, &[0x02, 0, 0, 0xff, 0xff]),
            (
                MouseButton {
                    button: super::MouseButton::Forward,
                    down: true,
                },
                &[0x03, 4, 1],
            ),
            (Wheel { dx: 0, dy: 120 }, &[0x04, 0, 0, 120, 0]),
            (
                Key {
                    usage: 0x1a,
                    down: false,
                },
                &[0x05, 0x1a, 0, 0],
            ),
            (
                Gamepad {
                    index: 1,
                    state: GamepadState {
                        buttons: 0x1001,
                        lx: -1,
                        ly: 2,
                        rx: 0,
                        ry: 0,
                        lt: 255,
                        rt: 7,
                    },
                },
                &[0x06, 1, 0x01, 0x10, 0xff, 0xff, 2, 0, 0, 0, 0, 0, 255, 7],
            ),
            (GamepadDisconnect { index: 3 }, &[0x07, 3]),
            (ReleaseAll, &[0x08]),
        ];
        for (event, bytes) in cases {
            assert_eq!(event.encode(), bytes, "{event:?}");
            assert_eq!(InputEvent::decode(bytes).unwrap(), event);
        }
    }

    #[test]
    fn rejects_malformed_messages() {
        use InputDecodeError::*;
        assert_eq!(InputEvent::decode(&[]), Err(Empty));
        assert_eq!(InputEvent::decode(&[0x7f]), Err(UnknownType(0x7f)));
        assert!(matches!(
            InputEvent::decode(&[0x01, 1, 2, 3]),
            Err(Length { .. })
        ));
        assert!(matches!(InputEvent::decode(&[0x08, 0]), Err(Length { .. })));
        assert!(matches!(InputEvent::decode(&[0x03, 5, 1]), Err(Invalid(_))));
        assert!(matches!(InputEvent::decode(&[0x03, 0, 2]), Err(Invalid(_))));
        assert!(matches!(
            InputEvent::decode(&[0x05, 4, 0, 9]),
            Err(Invalid(_))
        ));
        assert!(matches!(InputEvent::decode(&[0x07, 4]), Err(Invalid(_))));
    }
}
