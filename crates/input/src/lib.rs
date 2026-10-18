//! Injects [`ws_proto::InputEvent`]s from the browser into the host OS.
//!
//! The transport hands every `input` and `input-motion` data channel message to
//! [`InputHandler::handle`]; the handler decodes it, tracks held keys,
//! buttons and gamepads, and drives an [`InputSink`]. On Windows the sink is
//! [`WindowsInput`]: `SendInput` with scan codes for keyboard and mouse, and
//! the ViGEmBus driver for virtual Xbox 360 or DualShock 4 controllers.
//!
//! Wiring, per session:
//!
//! ```text
//! let mut input = InputHandler::new(WindowsInput::new(move |rumble| control_tx.send(rumble)));
//! // on each input / input-motion message:   input.handle(&bytes)?;
//! // a ~100 ms timer, required:         input.expire_stale_gamepads(Instant::now());
//! // on display switch:                  input.sink_mut().set_display_rect(rect);
//! // dropping `input` releases everything and unplugs the virtual pads.
//! ```
//!
//! The `expire_stale_gamepads` timer is required: a client that vanishes
//! sends nothing, so without it a held stick keeps the game moving until the
//! connection times out.

pub mod cursor;
pub mod gamepad;
pub mod handler;
pub mod keymap;
pub mod mouse;

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
