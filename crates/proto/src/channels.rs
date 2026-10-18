//! WebRTC data channel labels. The host opens all of them (see
//! `docs/signaling.md`); the browser picks them up in `ondatachannel`.

/// Ordered, reliable: [`crate::ClientControl`] / [`crate::HostControl`].
pub const CONTROL: &str = "control";
/// Ordered, reliable: key and button presses.
pub const INPUT: &str = "input";
/// Unordered, `maxRetransmits: 0`: mouse motion, gamepad axes.
pub const INPUT_MOTION: &str = "input-motion";
/// Ordered, reliable: file transfer, both ways.
pub const FILE: &str = "file";
