//! `ws-host-gui`: the host as a tray app with a status and settings window.
//!
//! ```text
//! ws-host-gui [--hidden]   --hidden starts in the tray (used by "Start with Windows")
//! ```
//!
//! Closing the window keeps the host running in the tray; Quit stops it.
//! Settings are saved in the data dir and started as the equivalent
//! `ws-host` command line, which still works on its own.

// No console window on Windows.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod host;
mod settings;
#[cfg(windows)]
mod win;

use std::sync::Mutex;

use anyhow::Context;
use eframe::egui;

pub const TITLE: &str = "web-stream host";
const LOG_FILE: &str = "ws-host-gui.log";

fn main() {
    if let Err(e) = run() {
        tracing::error!("{e:#}");
        #[cfg(windows)]
        win::error_box(&format!("{e:#}"));
        #[cfg(not(windows))]
        eprintln!("{e:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let hidden = std::env::args().skip(1).any(|a| a == "--hidden");
    let data_dir = ws_host::options::default_data_dir();
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("could not create {}", data_dir.display()))?;
    init_logging(&data_dir);

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
/// each launch; `RUST_LOG` filters it as for `ws-host`.
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
