//! Shared interface types for web-stream.

pub mod audio;
pub mod channels;
pub mod control;
pub mod cursor;
pub mod display;
pub mod input;
pub mod names;
pub mod orientation;
pub mod signaling;
pub mod video;

pub use audio::*;
pub use control::*;
pub use cursor::*;
pub use display::*;
pub use input::*;
pub use signaling::*;
pub use video::*;

/// Microseconds on the host's monotonic clock.
pub type MicrosMono = u64;
