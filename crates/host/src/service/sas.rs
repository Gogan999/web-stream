//! Ctrl+Alt+Del for the browser. Windows sends the secure attention
//! sequence only for a service (`SendSAS`), and only when the
//! `SoftwareSASGeneration` policy lets services do it. The session host
//! isn't a service, so it sets an event the service created and handed it;
//! the service's thread waiting on that event calls `SendSAS`.

use std::time::{Duration, Instant};

use anyhow::Context;
use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::System::Threading::{CreateEventW, INFINITE, SetEvent, WaitForSingleObject};
use windows::core::{s, w};

/// Requests closer together than this are one request: a double click
/// shouldn't open and close the Ctrl+Alt+Del screen.
const MIN_INTERVAL: Duration = Duration::from_secs(2);

/// An auto-reset event the session host can inherit. Lives as long as the
/// service, so it is never closed.
pub fn new_event() -> anyhow::Result<usize> {
    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: true.into(),
    };
    // SAFETY: `sa` is valid for the call; an unnamed event.
    let event = unsafe { CreateEventW(Some(&sa), false, false, None) }.context("CreateEvent")?;
    Ok(event.0 as usize)
}

/// In the session host: asks the service for Ctrl+Alt+Del.
pub fn request(event: usize) -> Result<(), String> {
    // SAFETY: an event handle the service handed down.
    unsafe { SetEvent(HANDLE(event as *mut _)) }
        .map_err(|e| format!("could not ask the service for Ctrl+Alt+Del: {e}"))
}

/// In the service: sends Ctrl+Alt+Del each time the host sets `event`.
pub fn spawn_sender(event: usize) {
    let spawned = std::thread::Builder::new()
        .name("ws-sas".into())
        .spawn(move || {
            let mut last: Option<Instant> = None;
            loop {
                // SAFETY: the event lives as long as the process.
                if unsafe { WaitForSingleObject(HANDLE(event as *mut _), INFINITE) }
                    != WAIT_OBJECT_0
                {
                    return tracing::error!("Ctrl+Alt+Del: waiting on the event failed");
                }
                if last.is_some_and(|t| t.elapsed() < MIN_INTERVAL) {
                    continue;
                }
                last = Some(Instant::now());
                match send() {
                    Ok(()) => tracing::info!("sent Ctrl+Alt+Del for the browser"),
                    Err(e) => tracing::warn!("Ctrl+Alt+Del: {e:#}"),
                }
            }
        });
    if let Err(e) = spawned {
        tracing::error!("Ctrl+Alt+Del thread: {e}");
    }
}

fn send() -> anyhow::Result<()> {
    if !policy_allows_services() {
        tracing::warn!(
            "Ctrl+Alt+Del: the SoftwareSASGeneration policy doesn't let services send it, \
             so Windows will likely ignore it (see docs/host-setup.md)"
        );
    }
    // SAFETY: sas.dll from System32 only; SendSAS has the signature
    // `void SendSAS(BOOL AsUser)`.
    unsafe {
        let lib = LoadLibraryExW(w!("sas.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32)
            .context("load sas.dll")?;
        let f = GetProcAddress(lib, s!("SendSAS")).context("SendSAS not in sas.dll")?;
        let send_sas: extern "system" fn(i32) = std::mem::transmute(f);
        // FALSE: the caller is a service.
        send_sas(0);
    }
    Ok(())
}

/// Whether `SoftwareSASGeneration` is 1 (services) or 3 (services and
/// Ease of Access apps).
fn policy_allows_services() -> bool {
    let mut value = 0u32;
    let mut size = size_of::<u32>() as u32;
    // SAFETY: `value` is a writable DWORD of `size` bytes.
    let r = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System"),
            w!("SoftwareSASGeneration"),
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut value).cast()),
            Some(&mut size),
        )
    };
    r.is_ok() && matches!(value, 1 | 3)
}
