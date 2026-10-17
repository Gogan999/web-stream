//! WebRTC transport between the host and the browser.
//!
//! Owns the peer connection, the video/audio tracks fed from
//! [`ws_proto::EncodedFrame`] and [`ws_proto::AudioPacket`], and the `control`
//! and `input` data channels carrying [`ws_proto::ClientControl`],
//! [`ws_proto::HostControl`] and [`ws_proto::InputEvent`].
//!
//! Late frames are dropped, never queued: if the send side is behind it
//! skips to the newest frame and asks the encoder for a keyframe.
