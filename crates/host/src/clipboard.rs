//! Text clipboard sync between the host PC and a connected browser.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Most bytes of UTF-8 text synced either way (see [`ws_proto::CLIPBOARD_MAX`]).
pub const MAX_BYTES: usize = ws_proto::CLIPBOARD_MAX;

/// A change is read this long after the last notification.
pub const DEBOUNCE: Duration = Duration::from_millis(250);

/// What to do with text the host clipboard now holds.
#[derive(Debug, PartialEq, Eq)]
pub enum Local {
    /// Send it to the browser.
    Forward(String),
    /// It is what was last synced: say nothing.
    Echo,
    /// Nothing to send: empty.
    Empty,
    /// Over [`MAX_BYTES`]: not synced.
    TooLarge,
}

/// The echo and size rules; one per host.
#[derive(Debug, Default)]
pub struct SyncState {
    /// The text the two clipboards last agreed on, either direction.
    last: Option<String>,
}

impl SyncState {
    /// The host clipboard changed to `text`.
    pub fn local_changed(&mut self, text: String) -> Local {
        if text.is_empty() {
            return Local::Empty;
        }
        if text.len() > MAX_BYTES {
            return Local::TooLarge;
        }
        if self.last.as_deref() == Some(text.as_str()) {
            return Local::Echo;
        }
        self.last = Some(text.clone());
        Local::Forward(text)
    }

    /// The browser asks to put `text` on the host clipboard: `true` if it should be written.
    pub fn remote_set(&mut self, text: &str) -> bool {
        if text.len() > MAX_BYTES {
            return false;
        }
        self.last = Some(text.into());
        true
    }

    /// Forget what was synced (the browser left).
    pub fn reset(&mut self) {
        self.last = None;
    }
}

/// Whether the program that copied asked for the text to stay out of clipboard monitoring, history
/// or the cloud.
pub fn marked_private(
    monitor_excluded: bool,
    include_in_history: Option<u32>,
    upload_to_cloud: Option<u32>,
) -> bool {
    monitor_excluded || include_in_history == Some(0) || upload_to_cloud == Some(0)
}

/// An open clipboard.
pub struct Clipboard {
    state: Arc<Mutex<SyncState>>,
    active: Arc<AtomicBool>,
    imp: imp::Handle,
}

impl Clipboard {
    /// Starts the clipboard thread and checks that this process can open the Windows clipboard.
    pub fn open(on_change: impl Fn(String) + Send + 'static) -> Result<Self, String> {
        let state = Arc::new(Mutex::new(SyncState::default()));
        let active = Arc::new(AtomicBool::new(false));
        let imp = imp::start(state.clone(), active.clone(), Box::new(on_change))?;
        Ok(Self { state, active, imp })
    }

    /// Whether a browser is connected: clipboard contents are read and forwarded only while it is.
    pub fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::Relaxed);
        if !active {
            self.state.lock().expect("clipboard state lock").reset();
        }
    }

    /// Writes the browser's text to the clipboard, then calls `done` with whether it worked.
    pub fn set(&self, text: String, done: impl FnOnce(bool) + Send + 'static) {
        let allowed = self
            .state
            .lock()
            .expect("clipboard state lock")
            .remote_set(&text);
        if !allowed {
            return done(false);
        }
        self.imp.set(text, Box::new(done));
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::sync::mpsc;
    use windows::Win32::Foundation::{
        GlobalFree, HANDLE, HGLOBAL, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM,
    };
    use windows::Win32::System::DataExchange::{
        AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData,
        IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW,
        RemoveClipboardFormatListener, SetClipboardData,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Memory::{
        GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
        HWND_MESSAGE, KillTimer, MSG, PostMessageW, PostQuitMessage, RegisterClassW, SetTimer,
        TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_CLIPBOARDUPDATE, WM_DESTROY,
        WM_TIMER, WNDCLASSW,
    };
    use windows::core::w;

    /// `CF_UNICODETEXT`.
    const CF_UNICODETEXT: u32 = 13;
    const WM_WORK: u32 = WM_APP + 1;
    const DEBOUNCE_TIMER: usize = 1;
    /// The clipboard is open in another program for a moment after a copy.
    const OPEN_TRIES: u32 = 20;
    const OPEN_WAIT: Duration = Duration::from_millis(10);

    type Done = Box<dyn FnOnce(bool) + Send>;

    enum Job {
        Set(String, Done),
        Stop,
    }

    pub struct Handle {
        hwnd: isize,
        jobs: Arc<Mutex<VecDeque<Job>>>,
    }

    impl Handle {
        pub fn set(&self, text: String, done: Done) {
            self.push(Job::Set(text, done));
        }

        fn push(&self, job: Job) {
            self.jobs
                .lock()
                .expect("clipboard jobs lock")
                .push_back(job);
            // SAFETY: posting to a window that may be gone just fails.
            let _ =
                unsafe { PostMessageW(Some(HWND(self.hwnd as _)), WM_WORK, WPARAM(0), LPARAM(0)) };
        }
    }

    impl Drop for Handle {
        fn drop(&mut self) {
            self.push(Job::Stop);
        }
    }

    struct Worker {
        state: Arc<Mutex<SyncState>>,
        active: Arc<AtomicBool>,
        on_change: Box<dyn Fn(String) + Send>,
        jobs: Arc<Mutex<VecDeque<Job>>>,
    }

    thread_local! {
        static WORKER: RefCell<Option<Worker>> = const { RefCell::new(None) };
    }

    pub fn start(
        state: Arc<Mutex<SyncState>>,
        active: Arc<AtomicBool>,
        on_change: Box<dyn Fn(String) + Send>,
    ) -> Result<Handle, String> {
        let jobs = Arc::new(Mutex::new(VecDeque::new()));
        let (ready_tx, ready_rx) = mpsc::channel::<Result<isize, String>>();
        let worker = Worker {
            state,
            active,
            on_change,
            jobs: jobs.clone(),
        };
        std::thread::Builder::new()
            .name("ws-clipboard".into())
            .spawn(move || run(worker, ready_tx))
            .map_err(|e| format!("clipboard thread: {e}"))?;
        let hwnd = ready_rx
            .recv()
            .map_err(|_| "clipboard thread ended".to_string())??;
        Ok(Handle { hwnd, jobs })
    }

    fn run(worker: Worker, ready: mpsc::Sender<Result<isize, String>>) {
        // SAFETY: plain Win32 calls on this thread; the window and its listener are removed before
        // the thread ends.
        unsafe {
            let hwnd = match create_window() {
                Ok(h) => h,
                Err(e) => return drop(ready.send(Err(e))),
            };
            if let Err(e) = AddClipboardFormatListener(hwnd) {
                let _ = DestroyWindow(hwnd);
                return drop(ready.send(Err(format!("AddClipboardFormatListener: {e}"))));
            }
            if let Err(e) = with_clipboard(hwnd, || ()) {
                let _ = RemoveClipboardFormatListener(hwnd);
                let _ = DestroyWindow(hwnd);
                return drop(ready.send(Err(e)));
            }
            WORKER.with(|w| *w.borrow_mut() = Some(worker));
            let _ = ready.send(Ok(hwnd.0 as isize));
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            let _ = RemoveClipboardFormatListener(hwnd);
            WORKER.with(|w| *w.borrow_mut() = None);
        }
    }

    unsafe fn create_window() -> Result<HWND, String> {
        // SAFETY: the class and window belong to this thread.
        unsafe {
            let instance = GetModuleHandleW(None)
                .map(|m| HINSTANCE(m.0))
                .map_err(|e| format!("GetModuleHandle: {e}"))?;
            let class = WNDCLASSW {
                lpfnWndProc: Some(wnd_proc),
                hInstance: instance,
                lpszClassName: w!("WebStreamClipboard"),
                ..Default::default()
            };
            RegisterClassW(&class);
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("WebStreamClipboard"),
                w!("web-stream clipboard"),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                Some(instance),
                None,
            )
            .map_err(|e| format!("CreateWindow: {e}"))
        }
    }

    unsafe extern "system" fn wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        // SAFETY: called by Windows on the window's thread.
        unsafe {
            match msg {
                WM_CLIPBOARDUPDATE => {
                    SetTimer(
                        Some(hwnd),
                        DEBOUNCE_TIMER,
                        DEBOUNCE.as_millis() as u32,
                        None,
                    );
                    LRESULT(0)
                }
                WM_TIMER if wparam.0 == DEBOUNCE_TIMER => {
                    let _ = KillTimer(Some(hwnd), DEBOUNCE_TIMER);
                    changed(hwnd);
                    LRESULT(0)
                }
                WM_WORK => {
                    work(hwnd);
                    LRESULT(0)
                }
                WM_DESTROY => {
                    PostQuitMessage(0);
                    LRESULT(0)
                }
                _ => DefWindowProcW(hwnd, msg, wparam, lparam),
            }
        }
    }

    unsafe fn changed(hwnd: HWND) {
        let (state, active) = WORKER
            .with(|w| {
                w.borrow()
                    .as_ref()
                    .map(|w| (w.state.clone(), w.active.load(Ordering::Relaxed)))
            })
            .unzip();
        if active != Some(true) {
            return;
        }
        let Some(state) = state else { return };
        // SAFETY: on the window's thread.
        let change = match unsafe { read(hwnd) } {
            Ok(Read::Text(text)) => state
                .lock()
                .expect("clipboard state lock")
                .local_changed(text),
            Ok(Read::TooLarge) => Local::TooLarge,
            Ok(Read::NoText) => return,
            Err(e) => return tracing::debug!("clipboard read: {e}"),
        };
        match change {
            Local::Forward(text) => WORKER.with(|w| {
                if let Some(w) = w.borrow().as_ref() {
                    (w.on_change)(text);
                }
            }),
            Local::TooLarge => tracing::info!("clipboard text over the size limit, not synced"),
            Local::Echo | Local::Empty => {}
        }
    }

    unsafe fn work(hwnd: HWND) {
        loop {
            let job = WORKER.with(|w| {
                w.borrow()
                    .as_ref()
                    .and_then(|w| w.jobs.lock().expect("clipboard jobs lock").pop_front())
            });
            match job {
                Some(Job::Set(text, done)) => {
                    // SAFETY: on the window's thread.
                    let ok = match unsafe { write(hwnd, &text) } {
                        Ok(()) => true,
                        Err(e) => {
                            tracing::warn!("clipboard write: {e}");
                            false
                        }
                    };
                    done(ok);
                }
                Some(Job::Stop) => {
                    // SAFETY: our own window.
                    let _ = unsafe { DestroyWindow(hwnd) };
                }
                None => break,
            }
        }
    }

    /// Runs `f` with the clipboard open, retrying while another program holds it.
    unsafe fn with_clipboard<T>(hwnd: HWND, f: impl FnOnce() -> T) -> Result<T, String> {
        let mut last = String::new();
        for _ in 0..OPEN_TRIES {
            // SAFETY: closed again below on every path that opened it.
            match unsafe { OpenClipboard(Some(hwnd)) } {
                Ok(()) => {
                    let out = f();
                    let _ = unsafe { CloseClipboard() };
                    return Ok(out);
                }
                Err(e) => {
                    last = e.to_string();
                    std::thread::sleep(OPEN_WAIT);
                }
            }
        }
        Err(format!("OpenClipboard: {last}"))
    }

    enum Read {
        /// No text, or the program that copied it asked not to be monitored (password managers do).
        NoText,
        Text(String),
        /// Over the cap.
        TooLarge,
    }

    unsafe fn read(hwnd: HWND) -> Result<Read, String> {
        // SAFETY: the clipboard APIs are used on this thread, with the clipboard open while the
        // global memory is locked.
        unsafe {
            if IsClipboardFormatAvailable(CF_UNICODETEXT).is_err() {
                return Ok(Read::NoText);
            }
            let excluded =
                RegisterClipboardFormatW(w!("ExcludeClipboardContentFromMonitorProcessing"));
            let history = RegisterClipboardFormatW(w!("CanIncludeInClipboardHistory"));
            let cloud = RegisterClipboardFormatW(w!("CanUploadToCloudClipboard"));
            with_clipboard(hwnd, || {
                let monitor_excluded =
                    excluded != 0 && IsClipboardFormatAvailable(excluded).is_ok();
                if marked_private(monitor_excluded, dword(history), dword(cloud)) {
                    return Ok(Read::NoText);
                }
                let handle = GetClipboardData(CF_UNICODETEXT).map_err(|e| e.to_string())?;
                let mem = HGLOBAL(handle.0);
                let units = GlobalSize(mem) / 2;
                if units > MAX_BYTES + 1 {
                    return Ok(Read::TooLarge);
                }
                let ptr = GlobalLock(mem) as *const u16;
                if ptr.is_null() {
                    return Err("GlobalLock failed".to_string());
                }
                let mut len = 0;
                while len < units && *ptr.add(len) != 0 {
                    len += 1;
                }
                let text = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
                let _ = GlobalUnlock(mem);
                Ok(Read::Text(text))
            })?
        }
    }

    /// The DWORD stored under a registered clipboard format, if the clipboard (open) has it.
    unsafe fn dword(format: u32) -> Option<u32> {
        // SAFETY: the clipboard is open; the memory is locked while read.
        unsafe {
            if format == 0 || IsClipboardFormatAvailable(format).is_err() {
                return None;
            }
            let mem = HGLOBAL(GetClipboardData(format).ok()?.0);
            if GlobalSize(mem) < 4 {
                return None;
            }
            let ptr = GlobalLock(mem) as *const u32;
            if ptr.is_null() {
                return None;
            }
            let v = ptr.read_unaligned();
            let _ = GlobalUnlock(mem);
            Some(v)
        }
    }

    unsafe fn write(hwnd: HWND, text: &str) -> Result<(), String> {
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let bytes = wide.len() * 2;
        // SAFETY: the allocation is filled before the clipboard takes it, and freed here only if
        // the clipboard did not.
        unsafe {
            with_clipboard(hwnd, || {
                let mem = GlobalAlloc(GMEM_MOVEABLE, bytes).map_err(|e| e.to_string())?;
                let ptr = GlobalLock(mem) as *mut u16;
                if ptr.is_null() {
                    let _ = GlobalFree(Some(mem));
                    return Err("GlobalLock failed".to_string());
                }
                std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
                let _ = GlobalUnlock(mem);
                EmptyClipboard().map_err(|e| e.to_string())?;
                if let Err(e) = SetClipboardData(CF_UNICODETEXT, Some(HANDLE(mem.0))) {
                    let _ = GlobalFree(Some(mem));
                    return Err(e.to_string());
                }
                Ok(())
            })?
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;

    type Done = Box<dyn FnOnce(bool) + Send>;

    /// Never built: [`start`] always fails off Windows.
    pub struct Handle;

    impl Handle {
        pub fn set(&self, _: String, done: Done) {
            done(false);
        }
    }

    pub fn start(
        _: Arc<Mutex<SyncState>>,
        _: Arc<AtomicBool>,
        _: Box<dyn Fn(String) + Send>,
    ) -> Result<Handle, String> {
        Err("the clipboard is only shared on Windows".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_text_is_forwarded_once() {
        let mut s = SyncState::default();
        assert_eq!(s.local_changed("a".into()), Local::Forward("a".into()));
        assert_eq!(s.local_changed("a".into()), Local::Echo);
        assert_eq!(s.local_changed("b".into()), Local::Forward("b".into()));
        assert_eq!(s.local_changed("a".into()), Local::Forward("a".into()));
    }

    #[test]
    fn the_browsers_own_write_is_not_sent_back() {
        let mut s = SyncState::default();
        assert!(s.remote_set("from phone"));
        assert_eq!(s.local_changed("from phone".into()), Local::Echo);
        assert_eq!(
            s.local_changed("from pc".into()),
            Local::Forward("from pc".into())
        );
        assert!(s.remote_set("from pc"));
    }

    #[test]
    fn empty_and_oversize_text_never_cross() {
        let mut s = SyncState::default();
        assert_eq!(s.local_changed(String::new()), Local::Empty);
        let big = "x".repeat(MAX_BYTES + 1);
        assert_eq!(s.local_changed(big.clone()), Local::TooLarge);
        assert!(!s.remote_set(&big));
        // Exactly the cap is fine, counted in UTF-8 bytes.
        let edge = "x".repeat(MAX_BYTES);
        assert!(s.remote_set(&edge));
        let multibyte = "\u{e9}".repeat(MAX_BYTES / 2 + 1);
        assert_eq!(s.local_changed(multibyte), Local::TooLarge);
        // A refused write leaves what was synced alone.
        let mut s = SyncState::default();
        assert!(s.remote_set("kept"));
        assert!(!s.remote_set(&big));
        assert_eq!(s.local_changed("kept".into()), Local::Echo);
    }

    #[test]
    fn private_marks_keep_text_out() {
        assert!(!marked_private(false, None, None));
        assert!(!marked_private(false, Some(1), Some(1)));
        assert!(marked_private(true, None, None));
        assert!(marked_private(false, Some(0), None));
        assert!(marked_private(false, Some(1), Some(0)));
    }

    #[test]
    fn reset_forgets_the_last_text() {
        let mut s = SyncState::default();
        s.local_changed("a".into());
        s.reset();
        assert_eq!(s.local_changed("a".into()), Local::Forward("a".into()));
    }

    #[test]
    fn it_is_unavailable_off_windows() {
        if cfg!(not(windows)) {
            assert!(Clipboard::open(|_| ()).is_err());
        }
    }
}
