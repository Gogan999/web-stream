//! Following the input desktop.
//!
//! Windows shows the lock screen, the sign-in screen and UAC prompts on a
//! desktop of their own ("Winlogon"), separate from the one apps run on. A
//! thread captures the screen, reads the pointer and injects input only on
//! the desktop it is attached to, so the threads that do call
//! [`follow_input_desktop`] when that stops working. Only a process running
//! as SYSTEM may open the Winlogon desktop (the service starts the host that
//! way, see `ws-host service`); for anyone else the call changes nothing.

#[cfg(windows)]
mod imp {
    use std::cell::RefCell;

    use windows::Win32::Foundation::{CloseHandle, GENERIC_ALL, HANDLE};
    use windows::Win32::Security::{
        GetTokenInformation, IsWellKnownSid, TOKEN_QUERY, TOKEN_USER, TokenUser, WinLocalSystemSid,
    };
    use windows::Win32::System::StationsAndDesktops::{
        CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, GetThreadDesktop,
        GetUserObjectInformationW, HDESK, OpenInputDesktop, SetThreadDesktop, UOI_NAME,
    };
    use windows::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentThreadId, OpenProcessToken,
    };

    /// The desktop this thread was moved to, and the one it started on.
    struct Attached {
        opened: HDESK,
        original: HDESK,
    }

    impl Drop for Attached {
        fn drop(&mut self) {
            // A desktop can't be closed while a thread is on it: move back
            // first. Neither call matters if the thread is exiting anyway.
            // SAFETY: both handles are valid; `opened` is ours to close.
            unsafe {
                let _ = SetThreadDesktop(self.original);
                let _ = CloseDesktop(self.opened);
            }
        }
    }

    thread_local! {
        static ATTACHED: RefCell<Option<Attached>> = const { RefCell::new(None) };
    }

    fn name(desk: HDESK) -> Option<String> {
        let mut buf = [0u16; 256];
        let mut needed = 0u32;
        // SAFETY: `buf` is writable for its length in bytes.
        unsafe {
            GetUserObjectInformationW(
                HANDLE(desk.0),
                UOI_NAME,
                Some(buf.as_mut_ptr().cast()),
                size_of_val(&buf) as u32,
                Some(&mut needed),
            )
        }
        .ok()?;
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..len]))
    }

    /// Attaches the calling thread to the desktop that receives input now,
    /// if it is on another one. True if the thread moved, so whatever just
    /// failed is worth retrying. The thread must not own windows or hooks.
    pub fn follow_input_desktop() -> bool {
        // SAFETY: plain calls; the returned handle is closed or kept below.
        let input = match unsafe {
            OpenInputDesktop(
                DESKTOP_CONTROL_FLAGS(0),
                false,
                DESKTOP_ACCESS_FLAGS(GENERIC_ALL.0),
            )
        } {
            Ok(d) => d,
            // Usually access denied: the secure desktop, and we aren't SYSTEM.
            Err(_) => return false,
        };
        // SAFETY: the current thread's desktop handle needs no closing.
        let current = unsafe { GetThreadDesktop(GetCurrentThreadId()) }.ok();
        let input_name = name(input);
        if input_name.is_none() || current.and_then(name) == input_name {
            // SAFETY: we opened `input` and nothing uses it.
            let _ = unsafe { CloseDesktop(input) };
            return false;
        }
        // SAFETY: `input` is a valid desktop handle.
        if let Err(e) = unsafe { SetThreadDesktop(input) } {
            tracing::debug!("cannot switch to the input desktop: {e}");
            let _ = unsafe { CloseDesktop(input) };
            return false;
        }
        tracing::info!(
            desktop = input_name.as_deref(),
            "following the input desktop"
        );
        ATTACHED.with(|a| {
            let mut a = a.borrow_mut();
            match a.as_mut() {
                // Off the previous one now, so it can be closed.
                Some(prev) => {
                    // SAFETY: we opened it, and this thread (the only one
                    // with this handle) is off it now.
                    let _ = unsafe { CloseDesktop(std::mem::replace(&mut prev.opened, input)) };
                }
                None => {
                    *a = current.map(|original| Attached {
                        opened: input,
                        original,
                    })
                }
            }
        });
        true
    }

    /// True if this process runs as LocalSystem.
    pub fn running_as_system() -> bool {
        let mut token = HANDLE::default();
        // SAFETY: the pseudo handle needs no closing; `token` is closed below.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.is_err() {
            return false;
        }
        let token = Owned(token);
        let mut buf = vec![0u8; 256];
        let mut len = 0u32;
        // SAFETY: `buf` is writable for its length.
        let ok = unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                Some(buf.as_mut_ptr().cast()),
                buf.len() as u32,
                &mut len,
            )
        }
        .is_ok();
        if !ok {
            return false;
        }
        // SAFETY: GetTokenInformation filled a TOKEN_USER at the start of
        // `buf`; read unaligned since a Vec<u8> promises no alignment.
        let user: TOKEN_USER = unsafe { std::ptr::read_unaligned(buf.as_ptr().cast()) };
        // SAFETY: the SID points into `buf`, which is still alive.
        unsafe { IsWellKnownSid(user.User.Sid, WinLocalSystemSid) }.as_bool()
    }

    struct Owned(HANDLE);

    impl Drop for Owned {
        fn drop(&mut self) {
            // SAFETY: we own the handle.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

#[cfg(not(windows))]
mod imp {
    /// There is one desktop off Windows.
    pub fn follow_input_desktop() -> bool {
        false
    }

    pub fn running_as_system() -> bool {
        false
    }
}

pub use imp::*;
