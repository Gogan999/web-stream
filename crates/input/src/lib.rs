//! Injects [`ws_proto::InputEvent`]s from the browser into the host OS.

pub mod cursor;
pub mod gamepad;
pub mod handler;
pub mod keymap;
pub mod mouse;
pub mod text;

#[cfg(windows)]
mod win;
#[cfg(windows)]
mod win_cursor;

pub use handler::{
    GAMEPAD_HOLDOFF, GAMEPAD_STALE_AFTER, GamepadKind, HandleError, InputHandler, InputSink,
    SinkError,
};

#[cfg(windows)]
pub use crate::win::WindowsInput;
pub use mouse::DisplayRect;
