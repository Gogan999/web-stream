//! The host agent as a library: the `ws-host` console program and the
//! `ws-host-gui` tray app both run [`serve::serve`].

mod input;
mod media;
pub mod options;
mod platform;
pub mod serve;
