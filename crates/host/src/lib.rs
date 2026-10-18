//! The host agent as a library: the `ws-host` console program and the
//! `ws-host-gui` tray app both run [`serve::serve`].

mod cursor;
mod input;
mod media;
pub mod options;
mod platform;
pub mod serve;

pub use platform::audio_devices;

/// The web client this host was built with, as (path, contents).
pub mod web {
    include!(concat!(env!("OUT_DIR"), "/web_files.rs"));
}
