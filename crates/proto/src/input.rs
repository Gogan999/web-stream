use serde::{Deserialize, Serialize};

/// Mouse buttons, numbered like `MouseEvent.button` in the browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    Back,
    Forward,
}

/// Input from the browser, injected on the host by the input crate.
///
/// Motion goes on the unordered [`crate::channels::INPUT_MOTION`] channel,
/// buttons and keys on the reliable [`crate::channels::INPUT`] one (the input
/// work owns the final wire encoding).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum InputEvent {
    /// Relative motion (pointer lock).
    MouseMove {
        dx: i32,
        dy: i32,
    },
    /// Absolute position normalized to the captured display, 0.0..=1.0.
    MouseAbs {
        x: f32,
        y: f32,
    },
    MouseButton {
        button: MouseButton,
        down: bool,
    },
    /// Wheel deltas in pixels (`WheelEvent.deltaMode == 0`).
    Wheel {
        dx: f32,
        dy: f32,
    },
    /// `KeyboardEvent.code`, e.g. `"KeyW"`, so mapping is layout independent.
    Key {
        code: String,
        down: bool,
    },
    /// Release every held key and button (focus lost, tab hidden).
    ReleaseAll,
}
