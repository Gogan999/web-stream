//! The host agent as a library.

mod bottleneck;
pub mod clipboard;
mod codec;
pub mod console;
mod cursor;
mod files;
mod input;
mod media;
pub mod options;
mod platform;
pub mod sas;
mod scale;
pub mod serve;
pub mod service;
pub mod split;
mod tunnel;
pub mod update;
mod uploads;

pub use platform::audio_devices;

/// The web client this host was built with, as (path, contents).
pub mod web {
    include!(concat!(env!("OUT_DIR"), "/web_files.rs"));
}
