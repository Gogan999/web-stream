//! Injects [`ws_proto::InputEvent`]s from the browser into the host OS.

pub mod cursor;
pub mod gamepad;
pub mod handler;
pub mod keymap;
pub mod mouse;
pub mod text;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod win;
#[cfg(windows)]
mod win_cursor;

pub use handler::{
    GAMEPAD_HOLDOFF, GAMEPAD_STALE_AFTER, GamepadKind, HandleError, InputHandler, InputSink,
    SinkError,
};

#[cfg(target_os = "linux")]
pub use crate::linux::{LinuxInput, linux_key, send_ctrl_alt_del, us_key};
#[cfg(windows)]
pub use crate::win::WindowsInput;
pub use mouse::DisplayRect;
