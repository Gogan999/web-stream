//! Shared interface types for web-stream.
//!
//! Everything that crosses a crate boundary (host pipeline stages) or the
//! network (signaling, WebRTC data channels) is defined here so that every
//! component agrees on one shape. Types that travel to the browser derive
//! serde with `camelCase`/tagged representations so the web client can
//! consume them as plain JSON.

pub mod audio;
pub mod control;
pub mod display;
pub mod input;
pub mod signaling;
pub mod video;

pub use audio::*;
pub use control::*;
pub use display::*;
pub use input::*;
pub use signaling::*;
pub use video::*;

/// Microseconds on the host's monotonic clock. All pipeline timestamps use
/// this so capture-to-send latency can be measured at any stage.
pub type MicrosMono = u64;

/// Converts an [`std::time::Instant`] to [`MicrosMono`]. The epoch is the
/// first call in the process, so values from every crate are comparable.
pub fn micros_mono(at: std::time::Instant) -> MicrosMono {
    static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let epoch = *EPOCH.get_or_init(|| at);
    at.saturating_duration_since(epoch).as_micros() as MicrosMono
}
