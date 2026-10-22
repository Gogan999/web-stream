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

/// Every channel, for turning a label back into its constant.
pub const ALL: [&str; 4] = [CONTROL, INPUT, INPUT_MOTION, FILE];

/// The constant for `label`, if it names a channel.
pub fn known(label: &str) -> Option<&'static str> {
    ALL.into_iter().find(|c| *c == label)
}
