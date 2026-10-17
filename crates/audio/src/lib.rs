//! Host audio: WASAPI loopback capture and Opus encode for playback in the
//! browser, and decode of the browser microphone into a virtual capture
//! device. Packets cross crate boundaries as [`ws_proto::AudioPacket`].
