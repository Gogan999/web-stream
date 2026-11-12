//! The host agent as a library. The `ws-host-gui` tray app runs
//! [`serve::serve`], and its exe also runs the Windows service
//! ([`service`]).

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

/// For test hosts: game audio becomes a beep every second and the browser
/// mic plays into a level meter (`ws_audio::synthetic`), where there are no
/// Windows audio devices to use.
#[cfg(not(windows))]
pub fn use_synthetic_audio() {
    platform::use_synthetic_audio();
}

pub use platform::audio_devices;

/// The web client this host was built with, as (path, contents).
pub mod web {
    include!(concat!(env!("OUT_DIR"), "/web_files.rs"));
}
