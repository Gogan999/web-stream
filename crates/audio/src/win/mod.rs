//! WASAPI plumbing shared by loopback capture and mic playback.

mod loopback;
mod render;

pub use loopback::LoopbackCapture;
pub use render::MicPlayback;

use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::{
    DEVICE_STATE_ACTIVE, IAudioClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0, eConsole, eRender,
};
use windows::Win32::Media::KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE};
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize, STGM_READ,
};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW,
};
use windows::core::{PWSTR, w};

use crate::convert::{CHANNELS, InputFormat, SAMPLE_RATE, SampleFormat};
use crate::{AudioError, Result};

impl From<windows::core::Error> for AudioError {
    fn from(e: windows::core::Error) -> Self {
        AudioError::Platform(e.to_string())
    }
}

/// Per-thread setup for an audio thread: COM, 1 ms timer resolution so short
/// waits are accurate, and the MMCSS "Pro Audio" scheduling class.
pub(crate) struct AudioThread {
    mmcss: Option<HANDLE>,
}

impl AudioThread {
    pub(crate) fn enter() -> Result<Self> {
        // SAFETY: plain Win32 calls with no pointer arguments, undone in Drop.
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
            timeBeginPeriod(1);
            let mut task = 0;
            let mmcss = AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task).ok();
            Ok(Self { mmcss })
        }
    }
}

impl Drop for AudioThread {
    fn drop(&mut self) {
        // SAFETY: undoes exactly what `enter` did on this same thread.
        unsafe {
            if let Some(h) = self.mmcss {
                let _ = AvRevertMmThreadCharacteristics(h);
            }
            timeEndPeriod(1);
            CoUninitialize();
        }
    }
}

/// An auto-reset event for WASAPI's event-driven mode.
pub(crate) struct Event(pub(crate) HANDLE);

impl Event {
    pub(crate) fn new() -> Result<Self> {
        // SAFETY: no pointer arguments; the handle is closed in Drop.
        Ok(Self(unsafe {
            windows::Win32::System::Threading::CreateEventW(None, false, false, None)?
        }))
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: we own the handle.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn enumerator() -> Result<IMMDeviceEnumerator> {
    // SAFETY: COM is initialised on this thread by AudioThread.
    Ok(unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? })
}

/// Takes ownership of a CoTaskMem-allocated wide string.
fn take_pwstr(p: PWSTR) -> String {
    // SAFETY: `p` came from a COM call that returns a CoTaskMem string.
    unsafe {
        let s = p.to_string().unwrap_or_default();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

pub(crate) fn device_id(device: &IMMDevice) -> Result<String> {
    // SAFETY: valid device; the returned string is freed by take_pwstr.
    Ok(take_pwstr(unsafe { device.GetId()? }))
}

fn friendly_name(device: &IMMDevice) -> Result<String> {
    // SAFETY: valid device; the PROPVARIANT string copy is freed by take_pwstr.
    unsafe {
        let store = device.OpenPropertyStore(STGM_READ)?;
        let value = store.GetValue(&PKEY_Device_FriendlyName)?;
        Ok(take_pwstr(PropVariantToStringAlloc(&value)?))
    }
}

fn render_devices() -> Result<Vec<(IMMDevice, String)>> {
    let mut out = vec![];
    // SAFETY: COM calls on valid interfaces.
    unsafe {
        let all = enumerator()?.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
        for i in 0..all.GetCount()? {
            let device = all.Item(i)?;
            let name = friendly_name(&device).unwrap_or_default();
            out.push((device, name));
        }
    }
    Ok(out)
}

/// Names of the active output devices, for settings and error messages.
pub fn list_render_devices() -> Result<Vec<String>> {
    let _thread = AudioThread::enter()?;
    Ok(render_devices()?.into_iter().map(|(_, n)| n).collect())
}

/// The default output device, or the first active output whose name contains
/// `name` (case-insensitive).
pub(crate) fn find_render_device(name: Option<&str>) -> Result<IMMDevice> {
    let Some(name) = name else {
        // SAFETY: COM call on a valid enumerator.
        return Ok(unsafe { enumerator()?.GetDefaultAudioEndpoint(eRender, eConsole)? });
    };
    let devices = render_devices()?;
    let wanted = name.to_lowercase();
    if let Some(i) = devices
        .iter()
        .position(|(_, n)| n.to_lowercase().contains(&wanted))
    {
        return Ok(devices.into_iter().nth(i).unwrap().0);
    }
    let names: Vec<_> = devices.into_iter().map(|(_, n)| n).collect();
    Err(AudioError::DeviceNotFound(format!(
        "no output device matching {name:?}; available: {}",
        names.join(", ")
    )))
}

pub(crate) fn activate(device: &IMMDevice) -> Result<IAudioClient> {
    // SAFETY: COM call on a valid device.
    Ok(unsafe { device.Activate(CLSCTX_ALL, None)? })
}

/// 48 kHz stereo float, the format the Opus path uses.
pub(crate) fn native_format() -> WAVEFORMATEXTENSIBLE {
    let block = (CHANNELS * 4) as u16;
    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_EXTENSIBLE as u16,
            nChannels: CHANNELS as u16,
            nSamplesPerSec: SAMPLE_RATE,
            nAvgBytesPerSec: SAMPLE_RATE * block as u32,
            nBlockAlign: block,
            wBitsPerSample: 32,
            cbSize: (size_of::<WAVEFORMATEXTENSIBLE>() - size_of::<WAVEFORMATEX>()) as u16,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 {
            wValidBitsPerSample: 32,
        },
        // SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT
        dwChannelMask: 0x3,
        SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
    }
}

/// Reads the device's mix format and frees it.
pub(crate) fn mix_format(client: &IAudioClient) -> Result<(InputFormat, Vec<u8>)> {
    // SAFETY: GetMixFormat returns a CoTaskMem WAVEFORMATEX(+extension) that we
    // copy byte for byte and then free.
    unsafe {
        let p = client.GetMixFormat()?;
        let len = size_of::<WAVEFORMATEX>() + (*p).cbSize as usize;
        let raw = std::slice::from_raw_parts(p as *const u8, len).to_vec();
        CoTaskMemFree(Some(p as *const _));
        Ok((parse_format(&raw)?, raw))
    }
}

fn parse_format(raw: &[u8]) -> Result<InputFormat> {
    // SAFETY: `raw` holds at least a WAVEFORMATEX, and a full
    // WAVEFORMATEXTENSIBLE when the tag says so; reads are unaligned-safe.
    let base: WAVEFORMATEX = unsafe { std::ptr::read_unaligned(raw.as_ptr() as *const _) };
    let (tag, bits) = (base.wFormatTag as u32, base.wBitsPerSample);
    let float = if tag == WAVE_FORMAT_EXTENSIBLE && raw.len() >= size_of::<WAVEFORMATEXTENSIBLE>() {
        let ext: WAVEFORMATEXTENSIBLE =
            unsafe { std::ptr::read_unaligned(raw.as_ptr() as *const _) };
        let sub = ext.SubFormat;
        if sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
            true
        } else if sub == KSDATAFORMAT_SUBTYPE_PCM {
            false
        } else {
            return Err(AudioError::UnsupportedFormat(format!("subformat {sub:?}")));
        }
    } else {
        tag == WAVE_FORMAT_IEEE_FLOAT
    };
    let format = match (float, bits) {
        (true, 32) => SampleFormat::F32,
        (false, 16) => SampleFormat::I16,
        (false, 24) => SampleFormat::I24,
        (false, 32) => SampleFormat::I32,
        _ => {
            return Err(AudioError::UnsupportedFormat(format!(
                "{bits}-bit, float={float}"
            )));
        }
    };
    let (channels, block_align, rate) = (
        base.nChannels as usize,
        base.nBlockAlign as usize,
        base.nSamplesPerSec,
    );
    if channels == 0 || block_align != channels * format.bytes() {
        return Err(AudioError::UnsupportedFormat(format!(
            "{channels} channels with block align {block_align}"
        )));
    }
    Ok(InputFormat {
        format,
        channels,
        rate,
    })
}
