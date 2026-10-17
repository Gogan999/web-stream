//! WASAPI loopback capture of an output device, encoded to Opus.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, mpsc as std_mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioCaptureClient,
    IAudioClient, IMMDevice, WAVEFORMATEX,
};
use windows::Win32::System::Threading::WaitForSingleObject;
use ws_proto::AudioPacket;

use super::{
    AudioThread, Event, activate, device_id, find_render_device, mix_format, native_format,
};
use crate::convert::{Converter, InputFormat, SAMPLE_RATE, SampleFormat};
use crate::encode::{FRAME_DURATION, FRAME_SAMPLES, OpusFrameEncoder};
use crate::{AudioError, LoopbackConfig, Result};

/// 20 ms of engine buffer: enough headroom for a 5 ms polling loop.
const BUFFER_HNS: i64 = 200_000;
/// With nothing playing, loopback delivers no packets at all. After this long
/// without data the encoder is fed silence so packets keep flowing at 10 ms.
const SILENCE_AFTER: Duration = Duration::from_millis(30);
/// At most 100 ms of silence per fill.
const MAX_SILENCE_FRAMES: usize = 10;
const DEFAULT_DEVICE_CHECK: Duration = Duration::from_secs(1);

/// Captures what the host is playing and sends it as 10 ms Opus packets.
/// Dropping it stops the capture thread.
pub struct LoopbackCapture {
    stop: Arc<AtomicBool>,
    bitrate: Arc<AtomicU32>,
    thread: Option<JoinHandle<()>>,
}

impl LoopbackCapture {
    /// Opens the device and starts capturing. Packets are sent with
    /// `try_send`; a full channel drops the packet. If the device goes away
    /// later the thread keeps retrying, following the default device when
    /// `config.device` is `None`.
    pub fn start(config: LoopbackConfig, packets: mpsc::Sender<AudioPacket>) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let bitrate = Arc::new(AtomicU32::new(config.bitrate));
        let (ready_tx, ready_rx) = std_mpsc::sync_channel(1);
        let thread = {
            let (stop, bitrate) = (stop.clone(), bitrate.clone());
            std::thread::Builder::new()
                .name("ws-audio-loopback".into())
                .spawn(move || run(config, packets, stop, bitrate, ready_tx))
                .map_err(|e| AudioError::Platform(e.to_string()))?
        };
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                stop,
                bitrate,
                thread: Some(thread),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => Err(AudioError::Platform(
                "loopback thread exited during start".into(),
            )),
        }
    }

    /// Changes the Opus bitrate from the next packet on.
    pub fn set_bitrate(&self, bits_per_second: u32) {
        self.bitrate.store(bits_per_second, Ordering::Relaxed);
    }
}

impl Drop for LoopbackCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Stream {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: Event,
    converter: Converter,
    device_id: String,
}

impl Stream {
    fn open(device: &IMMDevice) -> Result<Self> {
        let event = Event::new()?;
        let (client, input) = init_client(device)?;
        // SAFETY: COM calls on an initialised client.
        let capture = unsafe {
            // Loopback ignores the event on some Windows builds; the 5 ms
            // wait timeout in the loop covers that case.
            let _ = client.SetEventHandle(event.0);
            let capture: IAudioCaptureClient = client.GetService()?;
            client.Start()?;
            capture
        };
        info!(?input, "loopback capture started");
        Ok(Self {
            client,
            capture,
            event,
            converter: Converter::new(input),
            device_id: device_id(device)?,
        })
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: COM call on a valid client.
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

/// Asks the engine to convert to 48 kHz stereo float; falls back to the mix
/// format, which [`Converter`] then handles.
fn init_client(device: &IMMDevice) -> Result<(IAudioClient, InputFormat)> {
    let flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
    let native = native_format();
    let client = activate(device)?;
    // SAFETY: the format pointer is valid for the call.
    let converted = unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            flags | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            BUFFER_HNS,
            0,
            &native.Format,
            None,
        )
    };
    if converted.is_ok() {
        let f = InputFormat {
            format: SampleFormat::F32,
            channels: 2,
            rate: SAMPLE_RATE,
        };
        return Ok((client, f));
    }
    debug!("engine conversion unavailable, capturing the mix format");
    // A client cannot be initialised twice, so start over.
    let client = activate(device)?;
    let (input, raw) = mix_format(&client)?;
    // SAFETY: `raw` holds the device's own mix format.
    unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            flags,
            BUFFER_HNS,
            0,
            raw.as_ptr() as *const WAVEFORMATEX,
            None,
        )?;
    }
    Ok((client, input))
}

enum End {
    Stopped,
    /// The device was removed, disabled, or is no longer the default.
    Reopen,
}

fn run(
    config: LoopbackConfig,
    packets: mpsc::Sender<AudioPacket>,
    stop: Arc<AtomicBool>,
    bitrate: Arc<AtomicU32>,
    ready: std_mpsc::SyncSender<Result<()>>,
) {
    let setup = AudioThread::enter()
        .and_then(|t| Ok((t, OpusFrameEncoder::new(bitrate.load(Ordering::Relaxed))?)));
    let (_thread, mut encoder) = match setup {
        Ok(v) => v,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let mut ready = Some(ready);
    let mut dropped = 0u64;
    let mut send = |p: AudioPacket| {
        if packets.try_send(p).is_err() {
            dropped += 1;
            if dropped.is_power_of_two() {
                debug!(dropped, "audio packet dropped, consumer is behind or gone");
            }
        }
    };
    while !stop.load(Ordering::Relaxed) {
        let stream = find_render_device(config.device.as_deref()).and_then(|d| Stream::open(&d));
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                if let Some(r) = ready.take() {
                    let _ = r.send(Err(e));
                    return;
                }
                warn!("loopback device unavailable, retrying: {e}");
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        if let Some(r) = ready.take() {
            let _ = r.send(Ok(()));
        }
        match capture(stream, &config, &mut encoder, &stop, &bitrate, &mut send) {
            Ok(End::Stopped) => break,
            Ok(End::Reopen) => info!("audio output changed, reopening loopback"),
            Err(e) => {
                warn!("loopback capture failed, reopening: {e}");
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

fn capture(
    mut stream: Stream,
    config: &LoopbackConfig,
    encoder: &mut OpusFrameEncoder,
    stop: &AtomicBool,
    bitrate: &AtomicU32,
    send: &mut impl FnMut(AudioPacket),
) -> Result<End> {
    let frame_bytes = stream.converter.input().frame_bytes();
    let rate = stream.converter.input().rate as f64;
    let mut samples = Vec::with_capacity(SAMPLE_RATE as usize / 50 * 2);
    let mut current_bitrate = bitrate.load(Ordering::Relaxed);
    // End of the audio handed to the encoder so far, on the wall clock.
    let mut audio_end = Instant::now();
    let mut last_default_check = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        // SAFETY: valid event handle owned by `stream`.
        unsafe { WaitForSingleObject(stream.event.0, 5) };

        let wanted = bitrate.load(Ordering::Relaxed);
        if wanted != current_bitrate {
            encoder.set_bitrate(wanted)?;
            current_bitrate = wanted;
        }

        let mut got_data = false;
        loop {
            // SAFETY: COM calls on a started capture client; the buffer is
            // valid for `frames * frame_bytes` bytes until ReleaseBuffer.
            let (frames, flags, buffer) = unsafe {
                let next = match stream.capture.GetNextPacketSize() {
                    Ok(n) => n,
                    Err(e) if e.code() == AUDCLNT_E_DEVICE_INVALIDATED => return Ok(End::Reopen),
                    Err(e) => return Err(e.into()),
                };
                if next == 0 {
                    break;
                }
                let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
                stream
                    .capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                // A null buffer with frames > 0 is treated as silence.
                let bytes: &[u8] = if data.is_null() {
                    flags |= AUDCLNT_BUFFERFLAGS_SILENT.0 as u32;
                    &[]
                } else {
                    std::slice::from_raw_parts(data, frames as usize * frame_bytes)
                };
                (frames, flags, bytes)
            };
            samples.clear();
            if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                stream.converter.push_silence(frames as usize, &mut samples);
            } else {
                stream.converter.push(buffer, &mut samples);
            }
            // SAFETY: releases exactly what GetBuffer returned.
            unsafe { stream.capture.ReleaseBuffer(frames)? };
            let now = Instant::now();
            let captured_at = now
                .checked_sub(Duration::from_secs_f64(frames as f64 / rate))
                .unwrap_or(now);
            encoder.push(&samples, captured_at, &mut *send)?;
            audio_end = now;
            got_data = true;
        }

        let now = Instant::now();
        if !got_data && now.duration_since(audio_end) >= SILENCE_AFTER {
            // Fill the gap up to one frame behind now, in whole frames. After a
            // stall or sleep, send at most MAX_SILENCE and resync to now
            // rather than replaying minutes of silence.
            let gap = now.duration_since(audio_end) - FRAME_DURATION;
            let frames = (gap.as_micros() / FRAME_DURATION.as_micros()) as usize;
            let capped = frames.min(MAX_SILENCE_FRAMES);
            if capped > 0 {
                samples.clear();
                samples.resize(capped * FRAME_SAMPLES * 2, 0.0);
                encoder.push(&samples, audio_end, &mut *send)?;
                audio_end = if capped < frames {
                    now
                } else {
                    audio_end + FRAME_DURATION * capped as u32
                };
            }
        }

        if config.device.is_none() && now.duration_since(last_default_check) >= DEFAULT_DEVICE_CHECK
        {
            last_default_check = now;
            if let Ok(id) = find_render_device(None).and_then(|d| device_id(&d)) {
                if id != stream.device_id {
                    return Ok(End::Reopen);
                }
            }
        }
    }
    Ok(End::Stopped)
}
