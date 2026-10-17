//! WebRTC transport between the host and the browser.
//!
//! Owns the peer connection, the video/audio tracks fed from
//! [`ws_proto::EncodedFrame`] and [`ws_proto::AudioPacket`], the data
//! channels in [`ws_proto::channels`], and signaling with the browser
//! ([`ws_proto::HostSignal`] / [`ws_proto::ClientSignal`]).
//!
//! Late frames are dropped, never queued: if the send side is behind it
//! skips to the newest frame and asks the encoder for a keyframe.
