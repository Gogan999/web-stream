//! Input from the browser, injected on the host by `ws-input`.
//!
//! Binary on the wire: one data channel message per event, little-endian,
//! first byte is the type. Motion and gamepad state ride the unordered,
//! unreliable [`crate::channels::INPUT_MOTION`] channel; everything else rides
//! the reliable [`crate::channels::INPUT`] one. The byte layouts are the table
//! in `docs/dev/protocol.md`.

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
    /// Absolute position, 0..=65535 across the active display. `seq` (on
    /// `input-motion`) counts up per position sent, so the host can ignore
    /// one that arrives after a newer one; `None` (on `input`, ahead of a
    /// touch click) is always applied.
    MouseAbs {
        x: u16,
        y: u16,
        seq: Option<u16>,
        /// Which video stream's display `x` and `y` are across: 0 is the
        /// main one, 1 the second monitor.
        lane: u8,
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
    /// Full state on every change and every 100 ms, on `input-motion`; the
    /// first state for an index plugs in a virtual pad. `seq` counts up per
    /// pad across this and [`Self::GamepadButtons`], so the host ignores a
    /// snapshot older than one it already applied. `None` from clients
    /// that don't number them: always applied.
    Gamepad {
        index: u8,
        state: GamepadState,
        seq: Option<u16>,
    },
    /// Full state sent on the reliable `input` channel whenever the buttons
    /// change, so a press or release is never lost. Its buttons always
    /// apply (they arrive in order); its sticks and triggers only if `seq`
    /// is newer than the last state applied.
    GamepadButtons {
        index: u8,
        state: GamepadState,
        seq: u16,
    },
    GamepadDisconnect {
        index: u8,
    },
    /// Release every held key and button (focus lost, tab hidden).
    ReleaseAll,
    /// Type this text as Unicode characters, whatever the host's keyboard
    /// layout, as `SendInput` with `KEYEVENTF_UNICODE`. For text a phone
    /// keyboard or a paste produced that has no key on a US layout. On the
    /// reliable `input` channel, so it stays in order with key presses.
    /// At most [`TEXT_MAX`] bytes of UTF-8.
    Text {
        text: String,
    },
}

/// Most bytes of UTF-8 in one [`InputEvent::Text`]; the browser splits
/// longer text into several events.
pub const TEXT_MAX: usize = 4096;

/// How many video streams (and so displays input can address) there are.
pub const MAX_LANES: u8 = 2;

pub mod wire {
    pub const MOUSE_MOVE: u8 = 0x01;
    pub const MOUSE_ABS: u8 = 0x02;
    pub const MOUSE_BUTTON: u8 = 0x03;
    pub const WHEEL: u8 = 0x04;
    pub const KEY: u8 = 0x05;
    pub const GAMEPAD: u8 = 0x06;
    pub const GAMEPAD_DISCONNECT: u8 = 0x07;
    pub const RELEASE_ALL: u8 = 0x08;
    pub const GAMEPAD_BUTTONS: u8 = 0x09;
    /// [`MOUSE_ABS`] for a display other than the main one: a lane byte
    /// follows the type.
    pub const MOUSE_ABS_ON: u8 = 0x0a;
    /// Unicode text: the type byte, then up to [`super::TEXT_MAX`] bytes of
    /// UTF-8 (so, unlike the others, a variable length).
    pub const TEXT: u8 = 0x0b;
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

/// Bytes 2..14 of a gamepad message.
fn pad_state(b: &[u8]) -> GamepadState {
    GamepadState {
        buttons: u16_at(b, 2),
        lx: i16_at(b, 4),
        ly: i16_at(b, 6),
        rx: i16_at(b, 8),
        ry: i16_at(b, 10),
        lt: b[12],
        rt: b[13],
    }
}

fn put_pad_state(out: &mut Vec<u8>, s: &GamepadState) {
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

/// Whether sequence number `a` comes after `b`, allowing for wraparound.
pub fn seq_newer(a: u16, b: u16) -> bool {
    (a.wrapping_sub(b) as i16) > 0
}

impl InputEvent {
    /// Decodes one data channel message. Anything malformed is rejected
    /// whole; the host never injects a partial event.
    pub fn decode(msg: &[u8]) -> Result<Self, InputDecodeError> {
        let &kind = msg.first().ok_or(InputDecodeError::Empty)?;
        if kind == wire::TEXT {
            let bytes = &msg[1..];
            if bytes.len() > TEXT_MAX {
                return Err(InputDecodeError::Invalid("text too long"));
            }
            let text = std::str::from_utf8(bytes)
                .map_err(|_| InputDecodeError::Invalid("text is not UTF-8"))?;
            return Ok(Self::Text { text: text.into() });
        }
        // Absolute moves and gamepad state may carry a trailing sequence
        // number.
        let (expected, with_seq) = match kind {
            wire::MOUSE_MOVE | wire::WHEEL => (5, None),
            wire::MOUSE_ABS => (5, Some(7)),
            wire::MOUSE_ABS_ON => (6, Some(8)),
            wire::MOUSE_BUTTON => (3, None),
            wire::KEY => (4, None),
            wire::GAMEPAD => (14, Some(16)),
            wire::GAMEPAD_BUTTONS => (16, None),
            wire::GAMEPAD_DISCONNECT => (2, None),
            wire::RELEASE_ALL => (1, None),
            other => return Err(InputDecodeError::UnknownType(other)),
        };
        let seq = (with_seq == Some(msg.len())).then(|| u16_at(msg, msg.len() - 2));
        if msg.len() != expected && seq.is_none() {
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
                seq,
                lane: 0,
            },
            wire::MOUSE_ABS_ON => Self::MouseAbs {
                x: u16_at(b, 2),
                y: u16_at(b, 4),
                seq,
                lane: match b[1] {
                    n if n < MAX_LANES => n,
                    _ => return Err(InputDecodeError::Invalid("lane out of range")),
                },
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
                state: pad_state(b),
                seq,
            },
            wire::GAMEPAD_BUTTONS => Self::GamepadButtons {
                index: pad_at(b, 1)?,
                state: pad_state(b),
                seq: u16_at(b, 14),
            },
            wire::GAMEPAD_DISCONNECT => Self::GamepadDisconnect {
                index: pad_at(b, 1)?,
            },
            _ => Self::ReleaseAll,
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16);
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
            Self::MouseAbs { x, y, seq, lane } => {
                if lane == 0 {
                    pair(&mut out, wire::MOUSE_ABS, x.to_le_bytes(), y.to_le_bytes());
                } else {
                    out.extend_from_slice(&[wire::MOUSE_ABS_ON, lane]);
                    out.extend_from_slice(&x.to_le_bytes());
                    out.extend_from_slice(&y.to_le_bytes());
                }
                if let Some(seq) = seq {
                    out.extend_from_slice(&seq.to_le_bytes());
                }
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
            Self::Gamepad {
                index,
                state: s,
                seq,
            } => {
                out.extend_from_slice(&[wire::GAMEPAD, index]);
                put_pad_state(&mut out, &s);
                if let Some(seq) = seq {
                    out.extend_from_slice(&seq.to_le_bytes());
                }
            }
            Self::GamepadButtons {
                index,
                state: s,
                seq,
            } => {
                out.extend_from_slice(&[wire::GAMEPAD_BUTTONS, index]);
                put_pad_state(&mut out, &s);
                out.extend_from_slice(&seq.to_le_bytes());
            }
            Self::GamepadDisconnect { index } => {
                out.extend_from_slice(&[wire::GAMEPAD_DISCONNECT, index])
            }
            Self::ReleaseAll => out.push(wire::RELEASE_ALL),
            Self::Text { ref text } => {
                out.push(wire::TEXT);
                out.extend_from_slice(text.as_bytes());
            }
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
        let pad = GamepadState {
            buttons: 0x1001,
            lx: -1,
            ly: 2,
            rx: 0,
            ry: 0,
            lt: 255,
            rt: 7,
        };
        let cases: [(InputEvent, &[u8]); 13] = [
            (MouseMove { dx: 1, dy: -1 }, &[0x01, 1, 0, 0xff, 0xff]),
            (
                MouseAbs {
                    x: 0,
                    y: 65535,
                    seq: None,
                    lane: 0,
                },
                &[0x02, 0, 0, 0xff, 0xff],
            ),
            (
                MouseAbs {
                    x: 0,
                    y: 65535,
                    seq: Some(258),
                    lane: 0,
                },
                &[0x02, 0, 0, 0xff, 0xff, 2, 1],
            ),
            (
                MouseAbs {
                    x: 0,
                    y: 65535,
                    seq: None,
                    lane: 1,
                },
                &[0x0a, 1, 0, 0, 0xff, 0xff],
            ),
            (
                MouseAbs {
                    x: 0,
                    y: 65535,
                    seq: Some(258),
                    lane: 1,
                },
                &[0x0a, 1, 0, 0, 0xff, 0xff, 2, 1],
            ),
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
                    state: pad,
                    seq: None,
                },
                &[0x06, 1, 0x01, 0x10, 0xff, 0xff, 2, 0, 0, 0, 0, 0, 255, 7],
            ),
            (
                Gamepad {
                    index: 1,
                    state: pad,
                    seq: Some(1),
                },
                &[
                    0x06, 1, 0x01, 0x10, 0xff, 0xff, 2, 0, 0, 0, 0, 0, 255, 7, 1, 0,
                ],
            ),
            (
                GamepadButtons {
                    index: 1,
                    state: pad,
                    seq: 0xffff,
                },
                &[
                    0x09, 1, 0x01, 0x10, 0xff, 0xff, 2, 0, 0, 0, 0, 0, 255, 7, 0xff, 0xff,
                ],
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
    fn every_type_and_length_decodes_or_errors_without_panicking() {
        let sizes: [(u8, &[usize]); 10] = [
            (0x01, &[5]),
            (0x02, &[5, 7]),
            (0x03, &[3]),
            (0x04, &[5]),
            (0x05, &[4]),
            (0x06, &[14, 16]),
            (0x07, &[2]),
            (0x08, &[1]),
            (0x09, &[16]),
            (0x0a, &[6, 8]),
        ];
        // Text (0x0b) is variable length; it has its own test.
        for kind in (0..=255u8).filter(|k| *k != wire::TEXT) {
            for len in 1..=20usize {
                let mut msg = vec![0u8; len];
                msg[0] = kind;
                let result = InputEvent::decode(&msg);
                let expected = sizes.iter().find(|(k, _)| *k == kind).map(|(_, n)| *n);
                match expected {
                    Some(n) if n.contains(&len) => {
                        assert!(result.is_ok(), "0x{kind:02x} len {len}")
                    }
                    Some(_) => assert!(matches!(result, Err(InputDecodeError::Length { .. }))),
                    None => assert_eq!(result, Err(InputDecodeError::UnknownType(kind))),
                }
            }
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
        // Only the lanes there are.
        assert!(matches!(
            InputEvent::decode(&[0x0a, 2, 0, 0, 0, 0]),
            Err(Invalid(_))
        ));
    }

    #[test]
    fn text_round_trip_and_limits() {
        let ev = InputEvent::Text {
            text: "h\u{e9}llo \u{1f600} \u{4e16}".into(),
        };
        let wire = ev.encode();
        assert_eq!(wire[0], 0x0b);
        assert_eq!(InputEvent::decode(&wire), Ok(ev));
        assert_eq!(
            InputEvent::decode(&[0x0b]),
            Ok(InputEvent::Text {
                text: String::new()
            })
        );
        // Not UTF-8, or too long: rejected whole.
        assert!(matches!(
            InputEvent::decode(&[0x0b, 0xff, 0xfe]),
            Err(InputDecodeError::Invalid(_))
        ));
        let mut long = vec![0x0b];
        long.resize(1 + TEXT_MAX, b'a');
        assert!(InputEvent::decode(&long).is_ok());
        long.push(b'a');
        assert!(matches!(
            InputEvent::decode(&long),
            Err(InputDecodeError::Invalid(_))
        ));
    }

    #[test]
    fn sequence_numbers_wrap() {
        assert!(seq_newer(1, 0));
        assert!(!seq_newer(0, 1));
        assert!(!seq_newer(5, 5));
        assert!(seq_newer(2, 65530));
        assert!(!seq_newer(65530, 2));
    }
}
