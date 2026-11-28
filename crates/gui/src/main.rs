//! `ws-host-gui`: the host as a tray app with a status and settings window.

#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod firstrun;
mod host;
mod portcheck;
mod qr;
#[cfg(windows)]
mod service_watch;
mod settings;
mod setup;
mod update_check;
mod upnp;
#[cfg(windows)]
mod win;

use std::sync::Mutex;
use std::time::Duration;

use anyhow::Context;
use eframe::egui;

pub const TITLE: &str = "web-stream host";
const LOG_FILE: &str = "ws-host-gui.log";
const SETUP_LOG_FILE: &str = "setup.log";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    #[cfg(target_os = "linux")]
    if args
        .first()
        .is_some_and(|a| a == ws_host::clipboard::HELPER_ARG)
    {
        std::process::exit(ws_host::clipboard::helper_main());
    }
    #[cfg(target_os = "linux")]
    if args
        .first()
        .is_some_and(|a| a == ws_host::display_mode::HELPER_ARG)
    {
        std::process::exit(ws_host::display_mode::helper_main());
    }
    if args.first().is_some_and(|a| a == "service") {
        std::process::exit(service(&args[1..]));
    }
    if args.first().is_some_and(|a| a == "setup") {
        let data_dir = ws_host::options::default_data_dir();
        let _ = std::fs::create_dir_all(&data_dir);
        init_logging(&data_dir, SETUP_LOG_FILE);
        std::process::exit(setup::main(&args[1..]));
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

/// `service...`: the exit code.
fn service(args: &[String]) -> i32 {
    let asked = matches!(
        args.first().map(String::as_str),
        Some("install" | "update" | "start" | "stop" | "uninstall") | Some("-h" | "--help") | None
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

fn run(args: &[String]) -> anyhow::Result<()> {
    let hidden = args.iter().any(|a| a == "--hidden");
    let pair = args.iter().any(|a| a == "--pair");
    let after = args
        .iter()
        .position(|a| a == "--after")
        .and_then(|i| args.get(i + 1))
        .and_then(|pid| pid.parse::<u32>().ok());
    let data_dir = ws_host::options::default_data_dir();
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("could not create {}", data_dir.display()))?;
    init_logging(&data_dir, LOG_FILE);

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
        Box::new(move |cc| {
            Ok(Box::new(app::App::new(
                cc, settings, data_dir, hidden, pair,
            )))
        }),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

/// Logs to a file in the data dir, started afresh on each launch; `RUST_LOG` filters it.
fn init_logging(data_dir: &std::path::Path, file: &str) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "info,str0m=warn".into());
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match std::fs::File::create(data_dir.join(file)) {
        Ok(file) => builder
            .with_ansi(false)
            .with_writer(Mutex::new(file))
            .init(),
        Err(_) => builder.init(),
    }
}

/// After a browser-started update has built a new version: starts the new build.
pub fn restart_after_update(host: &host::Host) {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => return host.log(format!("update: can't find this program: {e}")),
    };
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
