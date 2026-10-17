use serde::{Deserialize, Serialize};

/// Stable identifier for a host display for the lifetime of the host process.
///
/// On Windows this is assigned by enumeration order of (adapter, output)
/// pairs; it is not persisted across reboots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DisplayId(pub u32);

/// A display the host can capture, as shown in the client's display picker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DisplayInfo {
    pub id: DisplayId,
    /// Human readable name, e.g. `\\.\DISPLAY1 (NVIDIA GeForce RTX 4080)`.
    pub name: String,
    /// Desktop coordinates of the top-left corner (can be negative).
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// Refresh rate in millihertz (59940 = 59.94 Hz), 0 if unknown.
    pub refresh_mhz: u32,
    pub primary: bool,
}
