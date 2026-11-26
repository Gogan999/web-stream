//! Scheduling for the transport's own thread on Windows: fine timer
//! resolution and the MMCSS "Games" class while the run loop runs.

/// Asks Windows for 1 ms timer resolution until dropped. At the default
/// ~15.6 ms the run loop's sleeps (the pacer's next send) wake late, and the
/// pacer then sends well below its rate: the queue grows without bound and
/// every frame is dropped as late. Does nothing on other systems.
pub(crate) struct FineTimer(());

impl FineTimer {
    pub(crate) fn new() -> Self {
        #[cfg(windows)]
        {
            unthrottle_process();
            // SAFETY: no pointer arguments; undone in Drop.
            unsafe {
                windows::Win32::Media::timeBeginPeriod(1);
            }
        }
        Self(())
    }
}

/// Opts the process out of EcoQoS and timer-resolution throttling.
#[cfg(windows)]
pub fn unthrottle_process() {
    use windows::Win32::System::Threading::{
        GetCurrentProcess, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
        PROCESS_POWER_THROTTLING_STATE, ProcessPowerThrottling, SetProcessInformation,
    };
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED
                | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
            StateMask: 0,
        };
        // SAFETY: `state` outlives the call and its size is passed with it.
        let result = unsafe {
            SetProcessInformation(
                GetCurrentProcess(),
                ProcessPowerThrottling,
                std::ptr::from_ref(&state).cast(),
                std::mem::size_of_val(&state) as u32,
            )
        };
        if let Err(e) = result {
            tracing::debug!("power throttling opt-out refused: {e}");
        }
    });
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

/// Puts the calling thread in the MMCSS "Games" class until dropped, so the
/// pacer's wakeups aren't delayed behind ordinary threads (a game using
/// every core, say). Must be dropped on the thread that made it. Does
/// nothing on other systems, or if MMCSS refuses.
pub(crate) struct Mmcss {
    #[cfg(windows)]
    task: Option<windows::Win32::Foundation::HANDLE>,
}

impl Mmcss {
    pub(crate) fn enter() -> Self {
        #[cfg(windows)]
        {
            let mut index = 0;
            // SAFETY: `index` outlives the call; undone in Drop on this thread.
            let task = unsafe {
                windows::Win32::System::Threading::AvSetMmThreadCharacteristicsW(
                    windows::core::w!("Games"),
                    &mut index,
                )
            }
            .inspect_err(|e| tracing::debug!("MMCSS Games class refused: {e}"))
            .ok();
            Self { task }
        }
        #[cfg(not(windows))]
        Self {}
    }
}

impl Drop for Mmcss {
    fn drop(&mut self) {
        #[cfg(windows)]
        if let Some(task) = self.task {
            // SAFETY: the handle came from AvSetMmThreadCharacteristicsW on
            // this thread.
            let _ =
                unsafe { windows::Win32::System::Threading::AvRevertMmThreadCharacteristics(task) };
        }
    }
}
