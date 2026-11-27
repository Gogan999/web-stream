//! The parts of libpulse (PulseAudio or pipewire-pulse) the host uses, loaded at run time.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::convert::{CHANNELS, SAMPLE_RATE};
use crate::{AudioError, Result};

#[repr(C)]
struct Opaque {
    _private: [u8; 0],
}

type Mainloop = Opaque;
type Api = Opaque;
type Context = Opaque;
type Stream = Opaque;
type Operation = Opaque;

#[repr(C)]
pub struct SampleSpec {
    format: c_int,
    rate: u32,
    channels: u8,
}

#[repr(C)]
pub struct BufferAttr {
    maxlength: u32,
    tlength: u32,
    prebuf: u32,
    minreq: u32,
    fragsize: u32,
}

#[repr(C)]
struct ServerInfo {
    user_name: *const c_char,
    host_name: *const c_char,
    server_version: *const c_char,
    server_name: *const c_char,
    sample_spec: SampleSpec,
    default_sink_name: *const c_char,
    default_source_name: *const c_char,
}

#[repr(C)]
struct ModuleInfo {
    index: u32,
    name: *const c_char,
    argument: *const c_char,
}

#[repr(C)]
struct SinkInfo {
    name: *const c_char,
    index: u32,
    description: *const c_char,
}

type IndexCb = extern "C" fn(*mut Context, u32, *mut c_void);
type SuccessCb = extern "C" fn(*mut Context, c_int, *mut c_void);
type ServerInfoCb = extern "C" fn(*mut Context, *const ServerInfo, *mut c_void);
type ModuleInfoCb = extern "C" fn(*mut Context, *const ModuleInfo, c_int, *mut c_void);
type SinkInfoCb = extern "C" fn(*mut Context, *const SinkInfo, c_int, *mut c_void);

const SAMPLE_FLOAT32LE: c_int = 5;
const CONTEXT_NOAUTOSPAWN: c_int = 1;
const CONTEXT_READY: c_int = 4;
const CONTEXT_FAILED: c_int = 5;
const CONTEXT_TERMINATED: c_int = 6;
const STREAM_READY: c_int = 2;
const STREAM_FAILED: c_int = 3;
const STREAM_TERMINATED: c_int = 4;
const OPERATION_RUNNING: c_int = 0;
const STREAM_ADJUST_LATENCY: c_int = 0x2000;
const STREAM_DONT_MOVE: c_int = 0x0200;
const SEEK_RELATIVE: c_int = 0;
const INVALID_INDEX: u32 = u32::MAX;

/// How long a server request may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

macro_rules! api {
    ($($name:ident: fn($($arg:ty),*) $(-> $ret:ty)?;)*) => {
        #[allow(non_snake_case)]
        struct Lib {
            _lib: libloading::Library,
            $($name: unsafe extern "C" fn($($arg),*) $(-> $ret)?,)*
        }

        impl Lib {
            fn load() -> std::result::Result<Self, String> {
                // SAFETY: libpulse runs no code on load that needs anything from us.
                let lib = unsafe { libloading::Library::new("libpulse.so.0") }
                    .map_err(|e| format!("libpulse is not installed: {e}"))?;
                // SAFETY: each symbol is looked up with the C signature libpulse declares.
                unsafe {
                    Ok(Self {
                        $($name: *lib
                            .get(concat!(stringify!($name), "\0").as_bytes())
                            .map_err(|e| format!("libpulse: {e}"))?,)*
                        _lib: lib,
                    })
                }
            }
        }
    };
}

api! {
    pa_mainloop_new: fn() -> *mut Mainloop;
    pa_mainloop_free: fn(*mut Mainloop);
    pa_mainloop_get_api: fn(*mut Mainloop) -> *mut Api;
    pa_mainloop_prepare: fn(*mut Mainloop, c_int) -> c_int;
    pa_mainloop_poll: fn(*mut Mainloop) -> c_int;
    pa_mainloop_dispatch: fn(*mut Mainloop) -> c_int;
    pa_context_new: fn(*mut Api, *const c_char) -> *mut Context;
    pa_context_connect: fn(*mut Context, *const c_char, c_int, *const c_void) -> c_int;
    pa_context_disconnect: fn(*mut Context);
    pa_context_unref: fn(*mut Context);
    pa_context_get_state: fn(*mut Context) -> c_int;
    pa_context_errno: fn(*mut Context) -> c_int;
    pa_context_load_module: fn(*mut Context, *const c_char, *const c_char, IndexCb, *mut c_void) -> *mut Operation;
    pa_context_unload_module: fn(*mut Context, u32, SuccessCb, *mut c_void) -> *mut Operation;
    pa_context_get_server_info: fn(*mut Context, ServerInfoCb, *mut c_void) -> *mut Operation;
    pa_context_get_module_info_list: fn(*mut Context, ModuleInfoCb, *mut c_void) -> *mut Operation;
    pa_context_get_sink_info_list: fn(*mut Context, SinkInfoCb, *mut c_void) -> *mut Operation;
    pa_operation_get_state: fn(*mut Operation) -> c_int;
    pa_operation_unref: fn(*mut Operation);
    pa_strerror: fn(c_int) -> *const c_char;
    pa_stream_new: fn(*mut Context, *const c_char, *const SampleSpec, *const c_void) -> *mut Stream;
    pa_stream_connect_record: fn(*mut Stream, *const c_char, *const BufferAttr, c_int) -> c_int;
    pa_stream_connect_playback: fn(*mut Stream, *const c_char, *const BufferAttr, c_int, *const c_void, *mut Stream) -> c_int;
    pa_stream_get_state: fn(*mut Stream) -> c_int;
    pa_stream_peek: fn(*mut Stream, *mut *const c_void, *mut usize) -> c_int;
    pa_stream_drop: fn(*mut Stream) -> c_int;
    pa_stream_writable_size: fn(*mut Stream) -> usize;
    pa_stream_write: fn(*mut Stream, *const c_void, usize, *const c_void, i64, c_int) -> c_int;
    pa_stream_disconnect: fn(*mut Stream) -> c_int;
    pa_stream_unref: fn(*mut Stream);
}

fn lib() -> Result<&'static Lib> {
    static LIB: OnceLock<std::result::Result<Lib, String>> = OnceLock::new();
    LIB.get_or_init(Lib::load)
        .as_ref()
        .map_err(|e| AudioError::Platform(e.clone()))
}

/// Whether libpulse can be loaded.
pub fn available() -> bool {
    lib().is_ok()
}

fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: libpulse hands out NUL-terminated strings valid for the callback.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

/// 48 kHz stereo float, the format the Opus path uses.
pub fn native_spec() -> SampleSpec {
    SampleSpec {
        format: SAMPLE_FLOAT32LE,
        rate: SAMPLE_RATE,
        channels: CHANNELS as u8,
    }
}

/// Bytes in `ms` milliseconds of native audio.
pub const fn bytes_for_ms(ms: u32) -> u32 {
    SAMPLE_RATE / 1000 * ms * (CHANNELS as u32) * 4
}

/// A mainloop and a context connected to one server; everything runs on the owning thread.
pub struct Conn {
    lib: &'static Lib,
    mainloop: *mut Mainloop,
    context: *mut Context,
}

impl Conn {
    /// Connects to `server` (e.g. `unix:/run/user/1000/pulse/native`).
    pub fn connect(server: &str, name: &str) -> Result<Self> {
        let lib = lib()?;
        let server = CString::new(server).map_err(|e| AudioError::Platform(e.to_string()))?;
        let name = CString::new(name).map_err(|e| AudioError::Platform(e.to_string()))?;
        // SAFETY: plain libpulse calls; the objects are freed in Drop.
        unsafe {
            let mainloop = (lib.pa_mainloop_new)();
            if mainloop.is_null() {
                return Err(AudioError::Platform("pa_mainloop_new failed".into()));
            }
            let context = (lib.pa_context_new)((lib.pa_mainloop_get_api)(mainloop), name.as_ptr());
            let conn = Self {
                lib,
                mainloop,
                context,
            };
            if context.is_null() {
                return Err(AudioError::Platform("pa_context_new failed".into()));
            }
            if (lib.pa_context_connect)(
                context,
                server.as_ptr(),
                CONTEXT_NOAUTOSPAWN,
                std::ptr::null(),
            ) < 0
            {
                return Err(conn.error("connect"));
            }
            let deadline = Instant::now() + REQUEST_TIMEOUT;
            loop {
                match (lib.pa_context_get_state)(context) {
                    CONTEXT_READY => return Ok(conn),
                    CONTEXT_FAILED | CONTEXT_TERMINATED => return Err(conn.error("connect")),
                    _ if Instant::now() >= deadline => {
                        return Err(AudioError::Platform("audio server did not answer".into()));
                    }
                    _ => conn.iterate(Duration::from_millis(50))?,
                }
            }
        }
    }

    fn error(&self, what: &str) -> AudioError {
        // SAFETY: valid context; pa_strerror returns a static string.
        let msg = unsafe {
            cstr((self.lib.pa_strerror)((self.lib.pa_context_errno)(
                self.context,
            )))
        };
        AudioError::Platform(format!("audio server {what}: {msg}"))
    }

    /// Runs the mainloop until something happens or `timeout` passes.
    pub fn iterate(&self, timeout: Duration) -> Result<()> {
        // SAFETY: valid mainloop, used from this thread only.
        unsafe {
            let us = timeout.as_micros().min(i32::MAX as u128) as c_int;
            if (self.lib.pa_mainloop_prepare)(self.mainloop, us) < 0
                || (self.lib.pa_mainloop_poll)(self.mainloop) < 0
                || (self.lib.pa_mainloop_dispatch)(self.mainloop) < 0
            {
                return Err(AudioError::Platform("audio mainloop stopped".into()));
            }
            if (self.lib.pa_context_get_state)(self.context) != CONTEXT_READY {
                return Err(self.error("connection"));
            }
        }
        Ok(())
    }

    /// Runs the mainloop until `op` completes.
    fn wait(&self, op: *mut Operation) -> Result<()> {
        if op.is_null() {
            return Err(self.error("request"));
        }
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        let result = loop {
            // SAFETY: `op` is a live operation until unref below.
            if unsafe { (self.lib.pa_operation_get_state)(op) } != OPERATION_RUNNING {
                break Ok(());
            }
            if Instant::now() >= deadline {
                break Err(AudioError::Platform(
                    "audio server request timed out".into(),
                ));
            }
            if let Err(e) = self.iterate(Duration::from_millis(50)) {
                break Err(e);
            }
        };
        // SAFETY: drops our reference to `op`.
        unsafe { (self.lib.pa_operation_unref)(op) };
        result
    }

    /// The name of the default output device.
    pub fn default_sink(&self) -> Result<String> {
        extern "C" fn cb(_: *mut Context, info: *const ServerInfo, out: *mut c_void) {
            // SAFETY: `out` is the String passed below; `info` is valid for the call.
            unsafe {
                if let Some(info) = info.as_ref() {
                    *(out as *mut String) = cstr(info.default_sink_name);
                }
            }
        }
        let mut name = String::new();
        // SAFETY: `name` outlives the operation, which `wait` runs to completion.
        let op = unsafe {
            (self.lib.pa_context_get_server_info)(
                self.context,
                cb,
                &mut name as *mut String as *mut c_void,
            )
        };
        self.wait(op)?;
        Ok(name)
    }

    /// Output devices as (name, description).
    pub fn sinks(&self) -> Result<Vec<(String, String)>> {
        extern "C" fn cb(_: *mut Context, info: *const SinkInfo, eol: c_int, out: *mut c_void) {
            // SAFETY: `out` is the Vec passed below; `info` is valid for the call.
            unsafe {
                if eol == 0 {
                    if let Some(info) = info.as_ref() {
                        (*(out as *mut Vec<(String, String)>))
                            .push((cstr(info.name), cstr(info.description)));
                    }
                }
            }
        }
        let mut out: Vec<(String, String)> = Vec::new();
        // SAFETY: `out` outlives the operation.
        let op = unsafe {
            (self.lib.pa_context_get_sink_info_list)(
                self.context,
                cb,
                &mut out as *mut Vec<(String, String)> as *mut c_void,
            )
        };
        self.wait(op)?;
        Ok(out)
    }

    /// Loaded modules as (index, name, argument).
    pub fn modules(&self) -> Result<Vec<(u32, String, String)>> {
        extern "C" fn cb(_: *mut Context, info: *const ModuleInfo, eol: c_int, out: *mut c_void) {
            // SAFETY: `out` is the Vec passed below; `info` is valid for the call.
            unsafe {
                if eol == 0 {
                    if let Some(info) = info.as_ref() {
                        (*(out as *mut Vec<(u32, String, String)>)).push((
                            info.index,
                            cstr(info.name),
                            cstr(info.argument),
                        ));
                    }
                }
            }
        }
        let mut out: Vec<(u32, String, String)> = Vec::new();
        // SAFETY: `out` outlives the operation.
        let op = unsafe {
            (self.lib.pa_context_get_module_info_list)(
                self.context,
                cb,
                &mut out as *mut Vec<(u32, String, String)> as *mut c_void,
            )
        };
        self.wait(op)?;
        Ok(out)
    }

    pub fn load_module(&self, name: &str, args: &str) -> Result<u32> {
        extern "C" fn cb(_: *mut Context, index: u32, out: *mut c_void) {
            // SAFETY: `out` is the u32 passed below.
            unsafe { *(out as *mut u32) = index };
        }
        let c_name = CString::new(name).map_err(|e| AudioError::Platform(e.to_string()))?;
        let c_args = CString::new(args).map_err(|e| AudioError::Platform(e.to_string()))?;
        let mut index = INVALID_INDEX;
        // SAFETY: the strings and `index` outlive the operation.
        let op = unsafe {
            (self.lib.pa_context_load_module)(
                self.context,
                c_name.as_ptr(),
                c_args.as_ptr(),
                cb,
                &mut index as *mut u32 as *mut c_void,
            )
        };
        self.wait(op)?;
        if index == INVALID_INDEX {
            return Err(self.error(&format!("load {name}")));
        }
        Ok(index)
    }

    pub fn unload_module(&self, index: u32) -> Result<()> {
        extern "C" fn cb(_: *mut Context, _: c_int, _: *mut c_void) {}
        // SAFETY: no user data.
        let op = unsafe {
            (self.lib.pa_context_unload_module)(self.context, index, cb, std::ptr::null_mut())
        };
        self.wait(op)
    }

    /// Opens a 48 kHz stereo float stream: recording from `device`, or playing into it.
    pub fn stream(&self, name: &str, device: &str, record: bool, ms: u32) -> Result<PaStream<'_>> {
        let c_name = CString::new(name).map_err(|e| AudioError::Platform(e.to_string()))?;
        let c_dev = CString::new(device).map_err(|e| AudioError::Platform(e.to_string()))?;
        let spec = native_spec();
        let chunk = bytes_for_ms(ms);
        let attr = BufferAttr {
            maxlength: u32::MAX,
            tlength: if record { u32::MAX } else { chunk * 2 },
            prebuf: if record { u32::MAX } else { chunk },
            minreq: if record { u32::MAX } else { chunk / 2 },
            fragsize: if record { chunk } else { u32::MAX },
        };
        let flags = STREAM_ADJUST_LATENCY | STREAM_DONT_MOVE;
        // SAFETY: valid context; the stream is released by PaStream's Drop.
        unsafe {
            let raw =
                (self.lib.pa_stream_new)(self.context, c_name.as_ptr(), &spec, std::ptr::null());
            if raw.is_null() {
                return Err(self.error("stream"));
            }
            let stream = PaStream { conn: self, raw };
            let rc = if record {
                (self.lib.pa_stream_connect_record)(raw, c_dev.as_ptr(), &attr, flags)
            } else {
                (self.lib.pa_stream_connect_playback)(
                    raw,
                    c_dev.as_ptr(),
                    &attr,
                    flags,
                    std::ptr::null(),
                    std::ptr::null_mut(),
                )
            };
            if rc < 0 {
                return Err(self.error("stream connect"));
            }
            let deadline = Instant::now() + REQUEST_TIMEOUT;
            loop {
                match (self.lib.pa_stream_get_state)(raw) {
                    STREAM_READY => return Ok(stream),
                    STREAM_FAILED | STREAM_TERMINATED => return Err(self.error("stream")),
                    _ if Instant::now() >= deadline => {
                        return Err(AudioError::Platform("audio stream did not start".into()));
                    }
                    _ => self.iterate(Duration::from_millis(50))?,
                }
            }
        }
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        // SAFETY: frees what `connect` created, on the owning thread.
        unsafe {
            if !self.context.is_null() {
                (self.lib.pa_context_disconnect)(self.context);
                (self.lib.pa_context_unref)(self.context);
            }
            (self.lib.pa_mainloop_free)(self.mainloop);
        }
    }
}

/// A record or playback stream on a [`Conn`].
pub struct PaStream<'a> {
    conn: &'a Conn,
    raw: *mut Stream,
}

impl PaStream<'_> {
    fn check(&self) -> Result<()> {
        // SAFETY: valid stream.
        match unsafe { (self.conn.lib.pa_stream_get_state)(self.raw) } {
            STREAM_READY => Ok(()),
            _ => Err(self.conn.error("stream")),
        }
    }

    /// Hands every recorded fragment to `f`; `None` stands for a hole of that many bytes.
    pub fn read(&self, mut f: impl FnMut(Option<&[u8]>, usize)) -> Result<()> {
        self.check()?;
        loop {
            let (mut data, mut len) = (std::ptr::null(), 0usize);
            // SAFETY: valid record stream; the fragment stays valid until pa_stream_drop.
            unsafe {
                if (self.conn.lib.pa_stream_peek)(self.raw, &mut data, &mut len) < 0 {
                    return Err(self.conn.error("read"));
                }
                if len == 0 {
                    return Ok(());
                }
                if data.is_null() {
                    f(None, len);
                } else {
                    f(Some(std::slice::from_raw_parts(data.cast(), len)), len);
                }
                (self.conn.lib.pa_stream_drop)(self.raw);
            }
        }
    }

    /// Bytes the server will take now.
    pub fn writable(&self) -> Result<usize> {
        self.check()?;
        // SAFETY: valid playback stream.
        let n = unsafe { (self.conn.lib.pa_stream_writable_size)(self.raw) };
        Ok(if n == usize::MAX { 0 } else { n })
    }

    pub fn write(&self, samples: &[f32]) -> Result<()> {
        // SAFETY: libpulse copies the data before returning (no free callback).
        let rc = unsafe {
            (self.conn.lib.pa_stream_write)(
                self.raw,
                samples.as_ptr().cast(),
                size_of_val(samples),
                std::ptr::null(),
                0,
                SEEK_RELATIVE,
            )
        };
        if rc < 0 {
            return Err(self.conn.error("write"));
        }
        Ok(())
    }
}

impl Drop for PaStream<'_> {
    fn drop(&mut self) {
        // SAFETY: releases the stream on the owning thread.
        unsafe {
            (self.conn.lib.pa_stream_disconnect)(self.raw);
            (self.conn.lib.pa_stream_unref)(self.raw);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_sizes() {
        assert_eq!(bytes_for_ms(10), 480 * 2 * 4);
        assert_eq!(bytes_for_ms(20), 960 * 2 * 4);
    }

    #[test]
    fn struct_layouts_match_libpulse() {
        assert_eq!(size_of::<SampleSpec>(), 12);
        assert_eq!(size_of::<BufferAttr>(), 20);
        #[cfg(target_pointer_width = "64")]
        assert_eq!(std::mem::offset_of!(ServerInfo, default_sink_name), 48);
        #[cfg(target_pointer_width = "64")]
        assert_eq!(std::mem::offset_of!(SinkInfo, description), 16);
    }
}
