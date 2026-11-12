//! Windows only: the tray icon, hiding the window to the tray, one instance
//! at a time, and starting with Windows.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::time::Duration;

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    DIINSTALLDRIVER_FLAGS, DiInstallDriverW,
};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_SUCCESS, GetLastError, HWND,
};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};
use windows::Win32::System::Threading::{
    CreateMutexW, GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_SYNCHRONIZE,
    WaitForSingleObject,
};
use windows::Win32::UI::Shell::{
    FOLDERID_ProgramFilesCommonX86, KF_FLAG_DEFAULT, SHGetKnownFolderPath, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MessageBoxW, SW_HIDE, SW_RESTORE,
    SW_SHOWNORMAL, SetForegroundWindow, ShowWindowAsync,
};
use windows::core::{BOOL, GUID, HSTRING, w};
use ws_host::serve::Command;

use crate::host::Host;
use crate::{TITLE, icon_rgba};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "web-stream host";

/// The main window, once eframe has created it.
static WINDOW: AtomicIsize = AtomicIsize::new(0);

/// Set by the tray's "Check for updates"; the window takes it on its next
/// frame (see [`take_update_request`]).
static UPDATE_REQUESTED: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// Kept on the UI thread; dropped before exiting so the icon goes away.
    static TRAY: RefCell<Option<TrayIcon>> = const { RefCell::new(None) };
    /// The tray's update item, whose text says when a newer version is known.
    static UPDATE_ITEM: RefCell<Option<MenuItem>> = const { RefCell::new(None) };
    /// The tray's Quit item, whose text depends on whether the service hosts.
    static QUIT_ITEM: RefCell<Option<MenuItem>> = const { RefCell::new(None) };
}

/// True once per click on the tray's "Check for updates".
pub fn take_update_request() -> bool {
    UPDATE_REQUESTED.swap(false, Ordering::Relaxed)
}

/// Names the newer version in the tray menu and tooltip (UI thread only);
/// `None` puts the plain texts back.
pub fn set_tray_update(version: Option<&str>) {
    UPDATE_ITEM.with(|i| {
        if let Some(item) = i.borrow().as_ref() {
            item.set_text(match version {
                Some(v) => format!("Version {v} is available: update..."),
                None => "Check for updates".to_string(),
            });
        }
    });
    TRAY.with(|t| {
        if let Some(tray) = t.borrow().as_ref() {
            let _ = tray.set_tooltip(Some(match version {
                Some(v) => format!("{TITLE}: version {v} is available"),
                None => TITLE.to_string(),
            }));
        }
    });
}

/// The single-instance mutex, held for the life of the process (except
/// while an update's setup runs: see [`release_instance`]).
static INSTANCE: AtomicIsize = AtomicIsize::new(0);

/// False if the app is already running; that copy's window is brought
/// forward instead.
pub fn single_instance() -> bool {
    if take_instance_mutex() {
        return true;
    }
    if let Ok(hwnd) = unsafe { FindWindowW(None, &HSTRING::from(TITLE)) } {
        show_hwnd(hwnd);
    }
    false
}

fn take_instance_mutex() -> bool {
    let created = unsafe { CreateMutexW(None, false, w!("Local\\web-stream-host-gui")) };
    let Ok(handle) = created else { return false };
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe {
            let _ = CloseHandle(handle);
        }
        return false;
    }
    INSTANCE.store(handle.0 as isize, Ordering::Relaxed);
    true
}

/// Lets go of the single-instance mutex, which the installer's `AppMutex`
/// checks, before it is started.
pub fn release_instance() {
    let h = INSTANCE.swap(0, Ordering::Relaxed);
    if h != 0 {
        unsafe {
            let _ = CloseHandle(windows::Win32::Foundation::HANDLE(h as *mut _));
        }
    }
}

/// Takes the mutex back after the installer could not be started.
pub fn retake_instance() {
    if INSTANCE.load(Ordering::Relaxed) == 0 {
        take_instance_mutex();
    }
}

/// Starts the downloaded setup as administrator (a UAC prompt, which it
/// asks for itself), quietly, relaunching the app when it is done; returns
/// once it started, not when it ends.
pub fn run_setup(setup: &std::path::Path) -> anyhow::Result<()> {
    let r = unsafe {
        ShellExecuteW(
            None,
            w!("runas"),
            &HSTRING::from(setup.as_os_str()),
            w!("/SILENT /SUPPRESSMSGBOXES /NORESTART /RELAUNCH"),
            None,
            SW_SHOWNORMAL,
        )
    };
    if r.0 as isize <= 32 {
        anyhow::bail!(
            "the administrator prompt was declined or the installer could not start ({})",
            r.0 as isize
        );
    }
    Ok(())
}

pub fn error_box(text: &str) {
    unsafe {
        MessageBoxW(
            None,
            &HSTRING::from(text),
            &HSTRING::from(TITLE),
            MB_OK | MB_ICONERROR,
        );
    }
}

pub fn info_box(text: &str) {
    unsafe {
        MessageBoxW(
            None,
            &HSTRING::from(text),
            &HSTRING::from(TITLE),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}

/// Waits up to `timeout` for process `pid` to exit (at once if it is gone).
pub fn wait_for_exit(pid: u32, timeout: Duration) {
    let Ok(process) = (unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }) else {
        return;
    };
    unsafe {
        WaitForSingleObject(process, timeout.as_millis().min(u32::MAX as u128) as u32);
        let _ = CloseHandle(process);
    }
}

/// Whether this process runs as an elevated administrator, which the
/// service's control pipe takes.
pub fn is_elevated() -> bool {
    let mut token = Default::default();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.is_err() {
        return false;
    }
    let mut elevation = TOKEN_ELEVATION::default();
    let mut len = 0u32;
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            Some((&raw mut elevation).cast()),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
    }
    .is_ok();
    unsafe {
        let _ = CloseHandle(token);
    }
    ok && elevation.TokenIsElevated != 0
}

/// Starts this exe with `args` as administrator (a UAC prompt); returns
/// once it started, not when it ends.
pub fn run_elevated(args: &[String]) -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let params: Vec<String> = args
        .iter()
        .map(|a| ws_host::service::quote_arg(a))
        .collect();
    let r = unsafe {
        ShellExecuteW(
            None,
            w!("runas"),
            &HSTRING::from(exe.as_os_str()),
            &HSTRING::from(params.join(" ")),
            None,
            SW_SHOWNORMAL,
        )
    };
    // Above 32 is success; ERROR_CANCELLED (1223) when the prompt is declined.
    if r.0 as isize <= 32 {
        anyhow::bail!(
            "the administrator prompt was declined or failed ({})",
            r.0 as isize
        );
    }
    Ok(())
}

/// Remembers eframe's window so the tray can show and hide it.
pub fn set_window(handle: &impl HasWindowHandle) {
    if let Ok(h) = handle.window_handle() {
        if let RawWindowHandle::Win32(h) = h.as_raw() {
            WINDOW.store(h.hwnd.get(), Ordering::Relaxed);
        }
    }
}

fn window() -> Option<HWND> {
    match WINDOW.load(Ordering::Relaxed) {
        0 => None,
        h => Some(HWND(h as *mut _)),
    }
}

// Async: the host thread calls this too, and must never wait on the UI
// thread (which may be waiting for the host to stop).
fn show_hwnd(hwnd: HWND) {
    unsafe {
        let _ = ShowWindowAsync(hwnd, SW_RESTORE);
        let _ = SetForegroundWindow(hwnd);
    }
}

/// Shows the window and brings it forward. Any thread.
pub fn show_window() {
    if let Some(hwnd) = window() {
        show_hwnd(hwnd);
    }
}

/// Hides the window; the tray icon brings it back.
pub fn hide_window() {
    if let Some(hwnd) = window() {
        unsafe {
            let _ = ShowWindowAsync(hwnd, SW_HIDE);
        }
    }
}

/// Puts the icon in the tray. Its menu acts on `host` directly, so it works
/// while the window is hidden (and eframe isn't running frames).
pub fn create_tray(host: Host, ctx: eframe::egui::Context) -> anyhow::Result<()> {
    let open = MenuItem::with_id("open", "Open web-stream host", true, None);
    let pair = MenuItem::with_id("pair", "Pair a device", true, None);
    let drop = MenuItem::with_id("drop", "Disconnect the browser", true, None);
    let update = MenuItem::with_id("update", "Check for updates", true, None);
    let quit_item = MenuItem::with_id("quit", "Quit", true, None);
    let menu = Menu::new();
    menu.append_items(&[
        &open,
        &PredefinedMenuItem::separator(),
        &pair,
        &drop,
        &PredefinedMenuItem::separator(),
        &update,
        &quit_item,
    ])?;
    let (rgba, size) = icon_rgba();
    let tray = TrayIconBuilder::new()
        .with_tooltip(TITLE)
        .with_icon(tray_icon::Icon::from_rgba(rgba, size, size)?)
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .build()?;
    TRAY.with(|t| *t.borrow_mut() = Some(tray));
    UPDATE_ITEM.with(|i| *i.borrow_mut() = Some(update));
    QUIT_ITEM.with(|q| *q.borrow_mut() = Some(quit_item));

    let show = {
        let ctx = ctx.clone();
        move || {
            show_window();
            ctx.request_repaint();
        }
    };
    let tray_show = show.clone();
    TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
        let left_click = matches!(
            e,
            TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: tray_icon::MouseButtonState::Up,
                ..
            } | TrayIconEvent::DoubleClick {
                button: MouseButton::Left,
                ..
            }
        );
        if left_click {
            tray_show();
        }
    }));
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| match e.id.0.as_str() {
        "open" => show(),
        "pair" => {
            host.command(Command::OpenPairing);
            // The code shows up in the window.
            show();
        }
        "drop" => host.command(Command::DropSession),
        "update" => {
            UPDATE_REQUESTED.store(true, Ordering::Relaxed);
            // The window shows the result and the Update button.
            show();
        }
        "quit" => quit(&host),
        _ => {}
    }));
    Ok(())
}

/// Quit only closes this app while the Windows service hosts, so the menu
/// says so.
/// False if there is no tray menu yet (try again later).
pub fn set_service_mode(service: bool) -> bool {
    QUIT_ITEM.with(|q| {
        q.borrow().as_ref().is_some_and(|item| {
            item.set_text(if service {
                "Close tray (service keeps running)"
            } else {
                "Quit"
            });
            true
        })
    })
}

/// Stops the host (putting back a changed resolution) and exits. Runs on
/// the UI thread, from the tray menu or when the window closes.
pub fn quit(host: &Host) -> ! {
    hide_window();
    host.stop_and_wait(Duration::from_secs(15));
    crate::upnp::shutdown();
    TRAY.with(|t| t.borrow_mut().take());
    std::process::exit(0)
}

/// Exits for an elevated copy of this app (started with `--after` this
/// process) to take over: like [`quit`], but the router keeps forwarding
/// the ports, which the new copy goes on renewing.
pub fn hand_over(host: &Host) -> ! {
    hide_window();
    host.stop_and_wait(Duration::from_secs(15));
    TRAY.with(|t| t.borrow_mut().take());
    std::process::exit(0)
}

pub fn autostart_enabled() -> bool {
    let mut size = 0u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            &HSTRING::from(RUN_KEY),
            &HSTRING::from(RUN_VALUE),
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut size),
        )
    };
    r == ERROR_SUCCESS
}

/// Starts the app hidden in the tray when this user signs in.
pub fn set_autostart(enabled: bool) -> anyhow::Result<()> {
    let key = HSTRING::from(RUN_KEY);
    let name = HSTRING::from(RUN_VALUE);
    if enabled {
        let exe = std::env::current_exe()?;
        let command = HSTRING::from(format!("\"{}\" --hidden", exe.display()));
        // Bytes, including the terminating NUL.
        let bytes = (command.len() + 1) * 2;
        unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                &key,
                &name,
                REG_SZ.0,
                Some(command.as_ptr().cast()),
                bytes as u32,
            )
        }
        .ok()?;
    } else if autostart_enabled() {
        unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, &key, &name) }.ok()?;
    }
    Ok(())
}

fn known_folder(id: &GUID) -> anyhow::Result<std::path::PathBuf> {
    // SAFETY: the returned string is freed below.
    let p = unsafe { SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None) }?;
    // SAFETY: a valid NUL-terminated string from the shell.
    let path = unsafe { p.to_string() }.map(std::path::PathBuf::from);
    // SAFETY: allocated by the shell with CoTaskMemAlloc.
    unsafe { CoTaskMemFree(Some(p.0.cast())) };
    Ok(path?)
}

/// Steam's signed virtual microphone driver, in Steam's own folder (under
/// Program Files, so only administrators can change it).
pub fn steam_mic_inf() -> anyhow::Result<std::path::PathBuf> {
    Ok(known_folder(&FOLDERID_ProgramFilesCommonX86)?
        .join(r"Steam\drivers\Windows10\x64\SteamStreamingMicrophone.inf"))
}

/// Installs the driver package `inf`, as Sunshine installs Steam's virtual
/// speakers; true if Windows needs a restart. Needs administrator rights.
pub fn install_driver(inf: &std::path::Path) -> anyhow::Result<bool> {
    let mut reboot = BOOL(0);
    unsafe {
        DiInstallDriverW(
            None,
            &HSTRING::from(inf.as_os_str()),
            DIINSTALLDRIVER_FLAGS(0),
            Some(&mut reboot),
        )
    }?;
    Ok(reboot.as_bool())
}
