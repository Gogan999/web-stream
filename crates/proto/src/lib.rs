//! Shared interface types for web-stream.
//!
//! Everything that crosses a crate boundary (host pipeline stages) or the
//! network (pairing, signaling, WebRTC data channels) is defined here so that
//! every component agrees on one shape. Types that travel to the browser are
//! JSON objects tagged by `type` (input events by `t`). Signaling and pairing
//! follow `docs/signaling.md` (snake_case); data channel messages are
//! camelCase.

pub mod audio;
pub mod channels;
pub mod control;
pub mod cursor;
pub mod display;
pub mod input;
pub mod names;
pub mod signaling;
pub mod video;

pub use audio::*;
pub use control::*;
pub use cursor::*;
pub use display::*;
pub use input::*;
pub use signaling::*;
pub use video::*;

/// Microseconds on the host's monotonic clock. All pipeline timestamps use
/// this so capture-to-send latency can be measured at any stage.
pub type MicrosMono = u64;
