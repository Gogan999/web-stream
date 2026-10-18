//! Plays the browser microphone into a render device, normally a virtual
//! cable whose other end games use as a microphone.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc as std_mpsc};
use std::thread::JoinHandle;

use tracing::{info, warn};
use windows::Win32::Media::Audio::{
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioClient,
    IAudioRenderClient,
};
use windows::Win32::System::Threading::WaitForSingleObject;

use super::{AudioThread, Event, activate, find_mic_device};
use crate::convert::CHANNELS;
use crate::mic::{MicSource, mic_channel};
use crate::{AudioError, MicConfig, MicReceiver, Result};

/// 10 ms requested; the engine rounds up to what the device needs.
const BUFFER_HNS: i64 = 100_000;

/// Plays mic audio from a [`MicReceiver`] into the configured device until
/// dropped.
pub struct MicPlayback {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl MicPlayback {
    /// Opens the device (see [`MicConfig::device`]) and returns the handle to
    /// feed with the browser's mic packets.
    pub fn start(config: MicConfig) -> Result<(Self, MicReceiver)> {
        let (receiver, source) = mic_channel(config.jitter)?;
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = std_mpsc::sync_channel(1);
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("ws-audio-mic".into())
                .spawn(move || run(config, source, stop, ready_tx))
                .map_err(|e| AudioError::Platform(e.to_string()))?
        };
        match ready_rx.recv() {
            Ok(Ok(())) => Ok((
                Self {
                    stop,
                    thread: Some(thread),
                },
                receiver,
            )),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => Err(AudioError::Platform(
                "mic thread exited during start".into(),
            )),
        }
    }
}

impl Drop for MicPlayback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Output {
    client: IAudioClient,
    render: IAudioRenderClient,
    event: Event,
    buffer_frames: u32,
}

impl Output {
    fn open(device_name: &str) -> Result<Self> {
        let device = find_mic_device(device_name)?;
        let client = activate(&device)?;
        let event = Event::new()?;
        let format = super::native_format();
        // SAFETY: COM calls on a fresh client; the format pointer is valid for the call.
        unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                    | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                    | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                BUFFER_HNS,
                0,
                &format.Format,
                None,
            )?;
            client.SetEventHandle(event.0)?;
            let render: IAudioRenderClient = client.GetService()?;
            let buffer_frames = client.GetBufferSize()?;
            client.Start()?;
            Ok(Self {
                client,
                render,
                event,
                buffer_frames,
            })
        }
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        // SAFETY: COM call on a valid client.
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

fn run(
    config: MicConfig,
    mut source: MicSource,
    stop: Arc<AtomicBool>,
    ready: std_mpsc::SyncSender<Result<()>>,
) {
    let opened = AudioThread::enter().and_then(|t| Ok((t, Output::open(&config.device)?)));
    let (_thread, mut output) = match opened {
        Ok(v) => {
            let _ = ready.send(Ok(()));
            v
        }
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    info!(device = config.device, "mic playback started");
    let mut scratch = vec![0f32; output.buffer_frames as usize * CHANNELS];
    while !stop.load(Ordering::Relaxed) {
        match play(&output, &mut source, &mut scratch, &stop) {
            Ok(()) => break,
            Err(e) => {
                warn!("mic playback failed, reopening: {e}");
                drop(output);
                loop {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    if let Ok(o) = Output::open(&config.device) {
                        output = o;
                        scratch.resize(output.buffer_frames as usize * CHANNELS, 0.0);
                        break;
                    }
                }
            }
        }
    }
}

fn play(
    output: &Output,
    source: &mut MicSource,
    scratch: &mut [f32],
    stop: &AtomicBool,
) -> Result<()> {
    while !stop.load(Ordering::Relaxed) {
        // SAFETY: valid event handle owned by `output`.
        unsafe { WaitForSingleObject(output.event.0, 100) };
        // SAFETY: COM calls on a started render client; the buffer returned by
        // GetBuffer holds `frames` frames of stereo f32 until ReleaseBuffer.
        unsafe {
            let padding = output.client.GetCurrentPadding()?;
            let frames = output.buffer_frames.saturating_sub(padding) as usize;
            if frames == 0 {
                continue;
            }
            let pcm = &mut scratch[..frames * CHANNELS];
            source.fill(pcm);
            let data = output.render.GetBuffer(frames as u32)? as *mut f32;
            std::ptr::copy_nonoverlapping(pcm.as_ptr(), data, pcm.len());
            output.render.ReleaseBuffer(frames as u32, 0)?;
        }
    }
    Ok(())
}
