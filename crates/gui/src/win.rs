//! Windows only: the tray icon, hiding the window to the tray, one instance
//! at a time, and starting with Windows.

use std::cell::RefCell;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::Duration;

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, ERROR_SUCCESS, GetLastError, HWND};
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, MB_ICONERROR, MB_OK, MessageBoxW, SW_HIDE, SW_RESTORE, SetForegroundWindow,
    ShowWindowAsync,
};
use windows::core::{HSTRING, w};
use ws_host::serve::Command;

use crate::host::Host;
use crate::{TITLE, icon_rgba};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "web-stream host";

/// The main window, once eframe has created it.
static WINDOW: AtomicIsize = AtomicIsize::new(0);

thread_local! {
    /// Kept on the UI thread; dropped before exiting so the icon goes away.
    static TRAY: RefCell<Option<TrayIcon>> = const { RefCell::new(None) };
}

/// False if the app is already running; that copy's window is brought
/// forward instead.
pub fn single_instance() -> bool {
    // Never closed: the mutex lives as long as the process.
    let created = unsafe { CreateMutexW(None, false, w!("Local\\web-stream-host-gui")) };
    if created.is_ok() && unsafe { GetLastError() } != ERROR_ALREADY_EXISTS {
        return true;
    }
    if let Ok(hwnd) = unsafe { FindWindowW(None, &HSTRING::from(TITLE)) } {
        show_hwnd(hwnd);
    }
    false
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
    let pair = MenuItem::with_id("pair", "Pair a browser", true, None);
    let drop = MenuItem::with_id("drop", "Disconnect the browser", true, None);
    let quit_item = MenuItem::with_id("quit", "Quit", true, None);
    let menu = Menu::new();
    menu.append_items(&[
        &open,
        &PredefinedMenuItem::separator(),
        &pair,
        &drop,
        &PredefinedMenuItem::separator(),
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
        "quit" => quit(&host),
        _ => {}
    }));
    Ok(())
}

/// Stops the host (putting back a changed resolution) and exits. Runs on
/// the UI thread, from the tray menu or when the window closes.
pub fn quit(host: &Host) -> ! {
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
