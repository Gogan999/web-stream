//! Conversion from the XInput-layout wire state to a DualShock 4 report.

use ws_proto::GamepadState;

/// `XINPUT_GAMEPAD_*` button bits, as sent on the wire.
pub mod xbutton {
    pub const DPAD_UP: u16 = 0x0001;
    pub const DPAD_DOWN: u16 = 0x0002;
    pub const DPAD_LEFT: u16 = 0x0004;
    pub const DPAD_RIGHT: u16 = 0x0008;
    pub const START: u16 = 0x0010;
    pub const BACK: u16 = 0x0020;
    pub const LEFT_THUMB: u16 = 0x0040;
    pub const RIGHT_THUMB: u16 = 0x0080;
    pub const LEFT_SHOULDER: u16 = 0x0100;
    pub const RIGHT_SHOULDER: u16 = 0x0200;
    pub const GUIDE: u16 = 0x0400;
    pub const A: u16 = 0x1000;
    pub const B: u16 = 0x2000;
    pub const X: u16 = 0x4000;
    pub const Y: u16 = 0x8000;
}

/// ViGEm `DS4_BUTTONS` bits; the low nibble holds the d-pad hat.
mod ds4button {
    pub const SQUARE: u16 = 1 << 4;
    pub const CROSS: u16 = 1 << 5;
    pub const CIRCLE: u16 = 1 << 6;
    pub const TRIANGLE: u16 = 1 << 7;
    pub const SHOULDER_LEFT: u16 = 1 << 8;
    pub const SHOULDER_RIGHT: u16 = 1 << 9;
    pub const TRIGGER_LEFT: u16 = 1 << 10;
    pub const TRIGGER_RIGHT: u16 = 1 << 11;
    pub const SHARE: u16 = 1 << 12;
    pub const OPTIONS: u16 = 1 << 13;
    pub const THUMB_LEFT: u16 = 1 << 14;
    pub const THUMB_RIGHT: u16 = 1 << 15;
    pub const SPECIAL_PS: u8 = 1 << 0;
}

/// Field-for-field the same as `vigem_client::DS4Report`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ds4Report {
    pub thumb_lx: u8,
    pub thumb_ly: u8,
    pub thumb_rx: u8,
    pub thumb_ry: u8,
    pub buttons: u16,
    pub special: u8,
    pub trigger_l: u8,
    pub trigger_r: u8,
}

fn axis(v: i16) -> u8 {
    ((v as i32 + 32768) >> 8) as u8
}

fn hat(buttons: u16) -> u16 {
    use xbutton::*;
    let up = buttons & DPAD_UP != 0;
    let down = buttons & DPAD_DOWN != 0;
    let left = buttons & DPAD_LEFT != 0;
    let right = buttons & DPAD_RIGHT != 0;
    // Opposite directions cancel out.
    let (v, h) = (up as i8 - down as i8, right as i8 - left as i8);
    match (v, h) {
        (1, 0) => 0,
        (1, 1) => 1,
        (0, 1) => 2,
        (-1, 1) => 3,
        (-1, 0) => 4,
        (-1, -1) => 5,
        (0, -1) => 6,
        (1, -1) => 7,
        _ => 8,
    }
}

impl From<&GamepadState> for Ds4Report {
    fn from(s: &GamepadState) -> Self {
        let pairs = [
            (xbutton::X, ds4button::SQUARE),
            (xbutton::A, ds4button::CROSS),
            (xbutton::B, ds4button::CIRCLE),
            (xbutton::Y, ds4button::TRIANGLE),
            (xbutton::LEFT_SHOULDER, ds4button::SHOULDER_LEFT),
            (xbutton::RIGHT_SHOULDER, ds4button::SHOULDER_RIGHT),
            (xbutton::BACK, ds4button::SHARE),
            (xbutton::START, ds4button::OPTIONS),
            (xbutton::LEFT_THUMB, ds4button::THUMB_LEFT),
            (xbutton::RIGHT_THUMB, ds4button::THUMB_RIGHT),
        ];
        let mut buttons = hat(s.buttons);
        for (x, d) in pairs {
            if s.buttons & x != 0 {
                buttons |= d;
            }
        }
        if s.lt > 0 {
            buttons |= ds4button::TRIGGER_LEFT;
        }
        if s.rt > 0 {
            buttons |= ds4button::TRIGGER_RIGHT;
        }
        Ds4Report {
            thumb_lx: axis(s.lx),
            // XInput Y is up-positive, DS4 Y is down-positive.
            thumb_ly: 255 - axis(s.ly),
            thumb_rx: axis(s.rx),
            thumb_ry: 255 - axis(s.ry),
            buttons,
            special: if s.buttons & xbutton::GUIDE != 0 {
                ds4button::SPECIAL_PS
            } else {
                0
            },
            trigger_l: s.lt,
            trigger_r: s.rt,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neutral_state_is_centred_with_no_hat() {
        let r = Ds4Report::from(&GamepadState::default());
        assert_eq!(
            (r.thumb_lx, r.thumb_ly, r.thumb_rx, r.thumb_ry),
            (128, 127, 128, 127)
        );
        assert_eq!(r.buttons, 8);
        assert_eq!(r.special, 0);
    }

    #[test]
    fn axes_cover_full_range_and_invert_y() {
        let s = GamepadState {
            lx: i16::MIN,
            ly: i16::MAX,
            ..Default::default()
        };
        let r = Ds4Report::from(&s);
        assert_eq!(r.thumb_lx, 0);
        assert_eq!(r.thumb_ly, 0, "stick pushed up reads as DS4 top");
    }

    #[test]
    fn dpad_maps_to_hat_directions() {
        use xbutton::*;
        assert_eq!(hat(DPAD_UP), 0);
        assert_eq!(hat(DPAD_UP | DPAD_RIGHT), 1);
        assert_eq!(hat(DPAD_RIGHT), 2);
        assert_eq!(hat(DPAD_DOWN | DPAD_RIGHT), 3);
        assert_eq!(hat(DPAD_DOWN), 4);
        assert_eq!(hat(DPAD_DOWN | DPAD_LEFT), 5);
        assert_eq!(hat(DPAD_LEFT), 6);
        assert_eq!(hat(DPAD_UP | DPAD_LEFT), 7);
        assert_eq!(hat(DPAD_UP | DPAD_DOWN), 8);
        assert_eq!(hat(DPAD_UP | DPAD_DOWN | DPAD_LEFT), 6);
    }

    #[test]
    fn face_buttons_triggers_and_guide() {
        let s = GamepadState {
            buttons: xbutton::A | xbutton::Y | xbutton::GUIDE | xbutton::START,
            lt: 200,
            ..Default::default()
        };
        let r = Ds4Report::from(&s);
        assert_eq!(
            r.buttons,
            8 | ds4button::CROSS
                | ds4button::TRIANGLE
                | ds4button::OPTIONS
                | ds4button::TRIGGER_LEFT
        );
        assert_eq!(r.special, ds4button::SPECIAL_PS);
        assert_eq!(r.trigger_l, 200);
    }
}
