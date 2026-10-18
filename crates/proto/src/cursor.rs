//! The host pointer's shape, for the browser to show (DXGI leaves the
//! pointer out of the video).

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Largest custom pointer image the host sends, per side. Bigger ones (very
/// large accessibility sizes) fall back to [`CursorKind::Default`].
pub const MAX_CURSOR_SIDE: u32 = 256;

/// One [`crate::HostControl::CursorShape`] message. Standard shapes carry the
/// host's image too when it could be read, for a browser that draws the
/// pointer itself (pointer lock hides its own).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CursorShape {
    pub kind: CursorKind,
    /// Names the host's image of this pointer for the rest of the session.
    /// Always set for [`CursorKind::Custom`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u32>,
    /// The image, sent only the first time its `id` is used in a session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<CursorImage>,
}

/// The standard Windows pointers, named as their CSS `cursor` keyword so the
/// browser shows its own native version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CursorKind {
    Default,
    Pointer,
    Text,
    Wait,
    Progress,
    Crosshair,
    Move,
    EwResize,
    NsResize,
    NwseResize,
    NeswResize,
    NotAllowed,
    Help,
    /// The pointer is hidden (games, video players, typing).
    #[serde(rename = "none")]
    Hidden,
    /// Anything else: see [`CursorShape::id`] and [`CursorShape::image`].
    Custom,
}

/// A pointer image at the host's size, in desktop pixels.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CursorImage {
    pub width: u32,
    pub height: u32,
    /// The click point, from the top-left corner.
    pub hot_x: u32,
    pub hot_y: u32,
    /// RGBA, 8 bits each, straight alpha, rows top to bottom; base64 on the
    /// wire.
    #[serde(serialize_with = "to_base64", deserialize_with = "from_base64")]
    pub rgba: Vec<u8>,
}

fn to_base64<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&STANDARD.encode(bytes))
}

fn from_base64<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
    let text = String::deserialize(d)?;
    STANDARD.decode(text).map_err(serde::de::Error::custom)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HostControl;

    #[test]
    fn standard_shape_is_its_css_name() {
        let msg = HostControl::CursorShape(CursorShape {
            kind: CursorKind::NwseResize,
            id: None,
            image: None,
        });
        assert_eq!(
            serde_json::to_string(&msg).unwrap(),
            r#"{"type":"cursorShape","kind":"nwse-resize"}"#
        );
        let hidden = HostControl::CursorShape(CursorShape {
            kind: CursorKind::Hidden,
            id: None,
            image: None,
        });
        assert_eq!(
            serde_json::to_string(&hidden).unwrap(),
            r#"{"type":"cursorShape","kind":"none"}"#
        );
    }

    #[test]
    fn custom_image_round_trips_as_base64() {
        let msg = HostControl::CursorShape(CursorShape {
            kind: CursorKind::Custom,
            id: Some(3),
            image: Some(CursorImage {
                width: 1,
                height: 1,
                hot_x: 0,
                hot_y: 0,
                rgba: vec![255, 0, 0, 128],
            }),
        });
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            json,
            r#"{"type":"cursorShape","kind":"custom","id":3,"image":{"width":1,"height":1,"hotX":0,"hotY":0,"rgba":"/wAAgA=="}}"#
        );
        assert_eq!(serde_json::from_str::<HostControl>(&json).unwrap(), msg);
    }

    #[test]
    fn position_shape() {
        let msg = HostControl::CursorPosition { x: -3, y: 40 };
        assert_eq!(
            serde_json::to_string(&msg).unwrap(),
            r#"{"type":"cursorPosition","x":-3,"y":40}"#
        );
    }
}
