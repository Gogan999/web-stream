//! `ws-host-gui`: the host as a tray app with a status and settings window.
//! It is the host's only program: the same exe also runs the Windows
//! service.
//!
//! ```text
//! ws-host-gui [--hidden] [--after PID]
//!     --hidden     start in the tray (used by "Start with Windows" and updates)
//!     --after PID  wait for process PID to exit first (the build an update replaces)
//! ws-host-gui service install|update|uninstall [--quiet]
//!     started elevated from the settings; shows the result unless --quiet
//! ws-host-gui service console [--pair]
//!     the service's host: pairing, the connected browser, files (elevated)
//! ws-host-gui service run|session-host ...
//!     the service and the host it starts (see ws_host::service)
//! ```
//!
//! Closing the window keeps the host running in the tray; Quit stops it.
//! Settings are saved in the data dir and started as the equivalent host
//! options (`ws_host::options`).

// No console window on Windows.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod host;
#[cfg(windows)]
mod service_console;
mod settings;
#[cfg(windows)]
mod win;

use std::sync::Mutex;
use std::time::Duration;

use anyhow::Context;
use eframe::egui;

pub const TITLE: &str = "web-stream host";
const LOG_FILE: &str = "ws-host-gui.log";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "service") {
        std::process::exit(service(&args[1..]));
    }
    if let Err(e) = run(&args) {
        tracing::error!("{e:#}");
        #[cfg(windows)]
        win::error_box(&format!("{e:#}"));
        #[cfg(not(windows))]
        eprintln!("{e:#}");
        std::process::exit(1);
    }
}

/// `service ...`: the exit code.
fn service(args: &[String]) -> i32 {
    if args.first().is_some_and(|a| a == "console") {
        return match run_service_console(args.iter().any(|a| a == "--pair")) {
            Ok(()) => 0,
            Err(e) => {
                #[cfg(windows)]
                win::error_box(&format!("{e:#}"));
                #[cfg(not(windows))]
                eprintln!("{e:#}");
                1
            }
        };
    }
    // An administrator asked (from the settings, or update.ps1 with
    // --quiet): say how it went. The service and its host run unseen.
    let asked = matches!(
        args.first().map(String::as_str),
        Some("install" | "update" | "uninstall") | Some("-h" | "--help") | None
    );
    let quiet = args.iter().any(|a| a == "--quiet");
    let args: Vec<String> = args.iter().filter(|a| *a != "--quiet").cloned().collect();
    ws_host::service::init_logging(&args);
    let result = ws_host::service::main(&args);
    if let Err(e) = &result {
        tracing::error!("{e:#}");
    }
    if asked && !quiet {
        let text = match &result {
            Ok(text) => text.clone(),
            Err(e) => format!("{e:#}"),
        };
        #[cfg(windows)]
        if result.is_ok() {
            win::info_box(&text);
        } else {
            win::error_box(&text);
        }
        #[cfg(not(windows))]
        eprintln!("{text}");
    }
    i32::from(result.is_err())
}

#[cfg(windows)]
fn run_service_console(pair: bool) -> anyhow::Result<()> {
    service_console::run(pair)
}

#[cfg(not(windows))]
fn run_service_console(_pair: bool) -> anyhow::Result<()> {
    anyhow::bail!("the host service is Windows only")
}

fn run(args: &[String]) -> anyhow::Result<()> {
    let hidden = args.iter().any(|a| a == "--hidden");
    let after = args
        .iter()
        .position(|a| a == "--after")
        .and_then(|i| args.get(i + 1))
        .and_then(|pid| pid.parse::<u32>().ok());
    let data_dir = ws_host::options::default_data_dir();
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("could not create {}", data_dir.display()))?;
    init_logging(&data_dir);

    // Restarted by an update: the old build still holds the ports and the
    // single-instance lock until it has stopped.
    #[cfg(windows)]
    if let Some(pid) = after {
        win::wait_for_exit(pid, Duration::from_secs(30));
    }
    #[cfg(not(windows))]
    let _ = after;
    #[cfg(windows)]
    if !win::single_instance() {
        return Ok(());
    }

    let settings = match settings::Settings::load(&data_dir) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("using default settings: {e:#}");
            settings::Settings::default()
        }
    };

    let (rgba, size) = icon_rgba();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(TITLE)
            .with_inner_size([560.0, 640.0])
            .with_min_inner_size([420.0, 360.0])
            .with_icon(egui::IconData {
                rgba,
                width: size,
                height: size,
            }),
        ..Default::default()
    };
    eframe::run_native(
        TITLE,
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, settings, data_dir, hidden)))),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

/// Logs to a file in the data dir (there is no console), started afresh on
/// each launch; `RUST_LOG` filters it.
fn init_logging(data_dir: &std::path::Path) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "info,str0m=warn".into());
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match std::fs::File::create(data_dir.join(LOG_FILE)) {
        Ok(file) => builder
            .with_ansi(false)
            .with_writer(Mutex::new(file))
            .init(),
        Err(_) => builder.init(),
    }
}

/// After a browser-started update has built a new version (called on the
/// update's thread): starts the new build, which waits for this process to
/// exit, then stops the host (putting back a changed resolution) and exits.
/// If the new build can't be started, this one keeps running.
pub fn restart_after_update(host: &host::Host) {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => return host.log(format!("update: can't find this program: {e}")),
    };
    // While it runs, this build's exe has been renamed to .old: the new
    // build has the name.
    let new = exe.with_file_name(if cfg!(windows) {
        "ws-host-gui.exe"
    } else {
        "ws-host-gui"
    });
    let started = std::process::Command::new(&new)
        .args(["--hidden", "--after", &std::process::id().to_string()])
        .current_dir(new.parent().unwrap_or(&new))
        .spawn();
    if let Err(e) = started {
        return host.log(format!(
            "update: couldn't start the new build {}: {e}; restart the host yourself",
            new.display()
        ));
    }
    tracing::info!(new = %new.display(), "restarting on the new build");
    host.stop_and_wait(Duration::from_secs(15));
    std::process::exit(0)
}

/// The app and tray icon: a play triangle on a blue rounded square.
pub fn icon_rgba() -> (Vec<u8>, u32) {
    const N: u32 = 32;
    let mut rgba = Vec::with_capacity((N * N * 4) as usize);
    for y in 0..N {
        for x in 0..N {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            // Rounded square, corner radius 6.
            let r = 6.0;
            let cx = fx.clamp(r, N as f32 - r);
            let cy = fy.clamp(r, N as f32 - r);
            let inside = (fx - cx).powi(2) + (fy - cy).powi(2) <= r * r;
            // Triangle pointing right: x from 11 to 24, narrowing to the tip.
            let half = (24.0 - fx) * 0.6;
            let in_triangle = (11.0..=24.0).contains(&fx) && (fy - 16.0).abs() <= half;
            let px = match (inside, in_triangle) {
                (true, true) => [255, 255, 255, 255],
                (true, false) => [32, 96, 200, 255],
                (false, _) => [0, 0, 0, 0],
            };
            rgba.extend_from_slice(&px);
        }
    }
    (rgba, N)
}
