//! The OS-specific pieces: input injection, game audio capture and the
//! virtual mic. Off Windows they are stand-ins so the rest of the host (and
//! its signaling and transport) still runs and can be tested; after
//! [`use_synthetic_audio`] the audio ones are a beep and a mic level meter
//! (`ws_audio::synthetic`), for tests of the media path.

use tokio::sync::mpsc;
use ws_audio::{LoopbackConfig, MicConfig, MicReceiver};
use ws_input::DisplayRect;
use ws_input::cursor::CursorState;
use ws_proto::{AudioPacket, HostControl};

#[cfg(windows)]
mod imp {
    use super::*;

    pub type Sink = ws_input::WindowsInput;
    pub type Loopback = ws_audio::LoopbackCapture;
    pub type Mic = ws_audio::MicPlayback;

    pub fn input_sink(rumble: impl Fn(HostControl) + Send + Sync + 'static) -> Sink {
        ws_input::WindowsInput::new(rumble)
    }

    pub fn set_display_rect(sink: &mut Sink, rect: DisplayRect) {
        sink.set_display_rect(rect);
    }

    pub fn start_loopback(
        config: LoopbackConfig,
        packets: mpsc::Sender<AudioPacket>,
    ) -> ws_audio::Result<Loopback> {
        ws_audio::LoopbackCapture::start(config, packets)
    }

    pub fn start_mic(config: MicConfig) -> ws_audio::Result<(Mic, MicReceiver)> {
        ws_audio::MicPlayback::start(config)
    }

    /// Reads the host pointer; called from the cursor thread only.
    pub fn cursor_reader() -> Option<impl FnMut() -> Option<CursorState>> {
        let mut reader = ws_input::cursor::CursorReader::new();
        Some(move || reader.read())
    }

    pub fn audio_devices() -> ws_audio::Result<Vec<String>> {
        ws_audio::list_render_devices()
    }

    pub fn set_audio(capture: &Loopback, bitrate: u32, frame_ms: u32) {
        capture.set_bitrate(bitrate);
        capture.set_frame_ms(frame_ms);
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;
    use ws_input::{GamepadKind, InputSink, SinkError, keymap::KeyTarget};
    use ws_proto::{GamepadState, MouseButton};

    /// Logs what would have been injected.
    pub struct Sink;

    impl InputSink for Sink {
        fn key(&mut self, key: KeyTarget, down: bool) -> Result<(), SinkError> {
            tracing::debug!(?key, down, "input: key");
            Ok(())
        }
        fn mouse_move(&mut self, dx: i32, dy: i32) -> Result<(), SinkError> {
            tracing::trace!(dx, dy, "input: mouse move");
            Ok(())
        }
        fn mouse_move_abs(&mut self, x: u16, y: u16) -> Result<(), SinkError> {
            tracing::trace!(x, y, "input: mouse abs");
            Ok(())
        }
        fn mouse_button(&mut self, button: MouseButton, down: bool) -> Result<(), SinkError> {
            tracing::debug!(?button, down, "input: mouse button");
            Ok(())
        }
        fn mouse_wheel(&mut self, dx: i32, dy: i32) -> Result<(), SinkError> {
            tracing::debug!(dx, dy, "input: wheel");
            Ok(())
        }
        fn gamepad_connect(&mut self, index: u8, kind: GamepadKind) -> Result<(), SinkError> {
            tracing::debug!(index, ?kind, "input: gamepad connect");
            Ok(())
        }
        fn gamepad_update(&mut self, index: u8, _: &GamepadState) -> Result<(), SinkError> {
            tracing::trace!(index, "input: gamepad state");
            Ok(())
        }
        fn gamepad_disconnect(&mut self, index: u8) -> Result<(), SinkError> {
            tracing::debug!(index, "input: gamepad disconnect");
            Ok(())
        }
    }

    use std::sync::atomic::{AtomicBool, Ordering};
    use ws_audio::synthetic::{BeepSource, MicMeter};

    static SYNTHETIC: AtomicBool = AtomicBool::new(false);

    /// From now on game audio is a beep every second and the browser's mic
    /// plays into a level meter (see `ws_audio::synthetic`).
    pub fn use_synthetic_audio() {
        SYNTHETIC.store(true, Ordering::Relaxed);
    }

    fn synthetic() -> bool {
        SYNTHETIC.load(Ordering::Relaxed)
    }

    pub struct Loopback(BeepSource);
    pub struct Mic(#[allow(dead_code)] MicMeter);

    pub fn input_sink(_rumble: impl Fn(HostControl) + Send + Sync + 'static) -> Sink {
        Sink
    }

    pub fn set_display_rect(_: &mut Sink, _: DisplayRect) {}

    pub fn start_loopback(
        config: LoopbackConfig,
        packets: mpsc::Sender<AudioPacket>,
    ) -> ws_audio::Result<Loopback> {
        if !synthetic() {
            return Err(unsupported());
        }
        Ok(Loopback(BeepSource::start(config, packets)?))
    }

    pub fn start_mic(config: MicConfig) -> ws_audio::Result<(Mic, MicReceiver)> {
        if !synthetic() {
            return Err(unsupported());
        }
        let (meter, receiver) = MicMeter::start(config)?;
        Ok((Mic(meter), receiver))
    }

    pub fn cursor_reader() -> Option<impl FnMut() -> Option<CursorState>> {
        None::<fn() -> Option<CursorState>>
    }

    pub fn audio_devices() -> ws_audio::Result<Vec<String>> {
        if synthetic() {
            return Ok(vec!["Synthetic beep".into()]);
        }
        Err(unsupported())
    }

    pub fn set_audio(capture: &Loopback, bitrate: u32, frame_ms: u32) {
        capture.0.set_bitrate(bitrate);
        capture.0.set_frame_ms(frame_ms);
    }

    fn unsupported() -> ws_audio::AudioError {
        ws_audio::AudioError::Platform("audio is only supported on Windows".into())
    }
}

pub use imp::*;
