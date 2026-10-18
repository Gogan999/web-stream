//! Fine timer resolution on Windows while the transport runs.

/// Asks Windows for 1 ms timer resolution until dropped. At the default
/// ~15.6 ms the run loop's sleeps (the pacer's next send) wake late, and the
/// pacer then sends well below its rate: the queue grows without bound and
/// every frame is dropped as late. Does nothing on other systems.
pub(crate) struct FineTimer(());

impl FineTimer {
    pub(crate) fn new() -> Self {
        #[cfg(windows)]
        // SAFETY: no pointer arguments; undone in Drop.
        unsafe {
            windows::Win32::Media::timeBeginPeriod(1);
        }
        Self(())
    }
}

impl Drop for FineTimer {
    fn drop(&mut self) {
        #[cfg(windows)]
        // SAFETY: undoes exactly the call in `new`.
        unsafe {
            windows::Win32::Media::timeEndPeriod(1);
        }
    }
}
