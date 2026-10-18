//! The host agent as a library: the `ws-host` console program and the
//! `ws-host-gui` tray app both run [`serve::serve`].

mod cursor;
mod files;
mod input;
mod media;
mod names;
pub mod options;
mod platform;
pub mod serve;
mod uploads;

pub use platform::audio_devices;
