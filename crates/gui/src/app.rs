//! The window: status, pairing, the connected browser and settings.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText};
#[cfg(windows)]
use ws_host::serve::Notice;
use ws_host::serve::{Command, Event};

use crate::host::{FileStatus, Host, Status};
use crate::settings::{CODECS, QUALITIES, Settings};

pub struct App {
    host: Host,
    /// What the host was last started with (and what is saved).
    settings: Settings,
    /// What the settings form shows.
    draft: Settings,
    data_dir: PathBuf,
    displays: Vec<(u32, String)>,
    /// Restart the host once the running one has stopped.
    restart: bool,
    confirm_unpair: bool,
    settings_error: Option<String>,
    #[cfg(windows)]
    autostart: bool,
    /// Frames until the window hides itself, for `--hidden`. eframe shows
    /// the window after the first frame whatever it was built with.
    #[cfg_attr(not(windows), allow(dead_code))]
    hide_in_frames: Option<u8>,
}

impl App {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        settings: Settings,
        data_dir: PathBuf,
        hidden: bool,
    ) -> Self {
        let ctx = cc.egui_ctx.clone();
        let host = Host::new(Arc::new(move |e: Option<&Event>| {
            #[cfg(windows)]
            if let Some(Event::Notice(Notice::PairingCode { .. })) = e {
                // Pairing was asked for here; the code has to be seen.
                crate::win::show_window();
            }
            #[cfg(not(windows))]
            let _ = e;
            ctx.request_repaint();
        }));

        #[cfg(windows)]
        {
            crate::win::set_window(cc);
            if let Err(e) = crate::win::create_tray(host.clone(), cc.egui_ctx.clone()) {
                host.log(format!("could not add the tray icon: {e:#}"));
            }
        }

        let displays = match ws_capture::list_displays() {
            Ok(list) => list
                .into_iter()
                .map(|d| {
                    let primary = if d.primary { ", primary" } else { "" };
                    (
                        d.id.0,
                        format!("{}: {}x{}{primary} {}", d.id.0, d.width, d.height, d.name),
                    )
                })
                .collect(),
            Err(e) => {
                tracing::warn!("could not list displays: {e}");
                Vec::new()
            }
        };

        let mut app = Self {
            host,
            draft: settings.clone(),
            settings,
            data_dir,
            displays,
            restart: false,
            confirm_unpair: false,
            settings_error: None,
            #[cfg(windows)]
            autostart: crate::win::autostart_enabled(),
            hide_in_frames: hidden.then_some(2),
        };
        if app.settings.start_hosting {
            app.start();
        }
        app
    }

    fn start(&mut self) {
        match self.settings.serve_options() {
            Ok(opts) => {
                self.settings_error = None;
                self.host.start(opts);
            }
            Err(e) => {
                let e = format!("{e:#}");
                self.host.log(format!("cannot start: {e}"));
                self.settings_error = Some(e);
            }
        }
    }

    /// Checks and saves the form; true if it was saved.
    fn save_settings(&mut self) -> bool {
        if let Err(e) = self.draft.serve_options() {
            self.settings_error = Some(format!("{e:#}"));
            return false;
        }
        if let Err(e) = self.draft.save(&self.data_dir) {
            self.settings_error = Some(format!("could not save the settings: {e:#}"));
            return false;
        }
        self.settings_error = None;
        self.settings = self.draft.clone();
        true
    }

    fn quit(&mut self, ctx: &egui::Context) {
        #[cfg(windows)]
        {
            let _ = ctx;
            crate::win::quit(&self.host);
        }
        #[cfg(not(windows))]
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn status_ui(&mut self, ui: &mut egui::Ui) {
        let active = self.host.is_active();
        if !active && self.restart {
            self.restart = false;
            self.start();
        }
        let status = self.host.state().status.clone();
        ui.horizontal(|ui| {
            let (text, color) = match &status {
                Status::Stopped => ("Stopped".to_string(), Color32::GRAY),
                Status::Starting => ("Starting...".to_string(), Color32::YELLOW),
                Status::Stopping => ("Stopping...".to_string(), Color32::YELLOW),
                Status::Running { port, .. } => (format!("Hosting on port {port}"), Color32::GREEN),
                Status::Failed(_) => ("Stopped with an error".to_string(), Color32::RED),
            };
            ui.label(RichText::new(text).strong().color(color));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if active {
                    if ui
                        .add_enabled(
                            !matches!(status, Status::Stopping),
                            egui::Button::new("Stop"),
                        )
                        .clicked()
                    {
                        self.host.stop();
                    }
                } else if ui.button("Start").clicked() {
                    self.start();
                }
            });
        });
        match &status {
            Status::Failed(e) => {
                ui.colored_label(Color32::RED, e);
            }
            Status::Running {
                port,
                ips,
                fingerprint,
                web_root,
            } => {
                egui::Grid::new("running").num_columns(2).show(ui, |ui| {
                    ui.label("Open in the browser");
                    ui.vertical(|ui| {
                        if ips.is_empty() {
                            ui.monospace(format!("https://<this PC>:{port}/"));
                        }
                        for ip in ips {
                            let url = format!("https://{ip}:{port}/");
                            if ui
                                .link(RichText::new(&url).monospace())
                                .on_hover_text("Click to copy")
                                .clicked()
                            {
                                ui.ctx().copy_text(url);
                            }
                        }
                    });
                    ui.end_row();
                    ui.label("Certificate SHA-256");
                    ui.add(egui::Label::new(RichText::new(fingerprint).monospace().small()).wrap());
                    ui.end_row();
                    if let Some(dir) = web_root {
                        ui.label("Web client");
                        ui.label(format!("from {}", dir.display()));
                        ui.end_row();
                    }
                });
            }
            _ => {}
        }
    }

    fn session_ui(&mut self, ui: &mut egui::Ui) {
        let running = matches!(self.host.state().status, Status::Running { .. });
        let now = Instant::now();
        let (connected, pairing_until, code, locked) = {
            let mut s = self.host.state();
            s.expire(now);
            (
                s.connected.clone(),
                s.pairing_until,
                s.code.clone(),
                s.pairing_locked,
            )
        };

        ui.horizontal(|ui| {
            ui.label("Connected:");
            match &connected {
                Some(name) => {
                    ui.strong(name);
                    if ui.button("Disconnect").clicked() {
                        self.host.command(Command::DropSession);
                    }
                }
                None => {
                    ui.weak("no browser");
                }
            }
        });

        if connected.is_some() {
            self.files_ui(ui);
        }

        ui.horizontal(|ui| {
            if ui
                .add_enabled(running, egui::Button::new("Pair a browser"))
                .on_hover_text("Lets a browser ask for a pairing code for two minutes")
                .clicked()
            {
                self.host.command(Command::OpenPairing);
            }
            if let Some(until) = pairing_until {
                let left = until.saturating_duration_since(now).as_secs();
                ui.label(format!(
                    "Pairing open for {}:{:02}: request a code in the browser",
                    left / 60,
                    left % 60
                ));
            } else if locked {
                ui.colored_label(
                    Color32::RED,
                    "Locked after wrong codes; pair again to unlock",
                );
            }
        });
        if let Some(code) = &code {
            ui.add_space(4.0);
            ui.label(format!("Type this code in {:?}:", code.device_name));
            ui.label(RichText::new(&code.code).monospace().size(36.0).strong());
            let left = code.expires.saturating_duration_since(now).as_secs();
            ui.weak(format!("expires in {left}s"));
        }
        if pairing_until.is_some() || code.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(500));
        }

        ui.horizontal(|ui| {
            if self.confirm_unpair {
                ui.label("Forget every paired browser?");
                if ui.button("Yes, forget them").clicked() {
                    self.host.command(Command::UnpairAll);
                    self.confirm_unpair = false;
                }
                if ui.button("No").clicked() {
                    self.confirm_unpair = false;
                }
            } else if ui
                .add_enabled(running, egui::Button::new("Forget paired browsers..."))
                .clicked()
            {
                self.confirm_unpair = true;
            }
        });
    }

    /// Files shared with the connected browser: pick more, see how they
    /// are doing, stop sharing one.
    fn files_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            #[cfg(windows)]
            if ui
                .button("Send file...")
                .on_hover_text(
                    "Offer files to the browser; it saves them when you click Save there",
                )
                .clicked()
            {
                pick_files(self.host.clone());
            }
            ui.weak("or drop files on this window");
        });
        if self.settings.allow_upload {
            ui.horizontal(|ui| {
                ui.label("Files from the browser go to");
                let dir = match self.settings.upload_dir.trim() {
                    "" => ws_host::options::default_upload_dir(),
                    d => PathBuf::from(d),
                };
                if ui.link(dir.display().to_string()).clicked() {
                    open_folder(&dir);
                }
            });
        }
        let shared = self.host.state().shared.clone();
        if shared.is_empty() {
            return;
        }
        egui::Grid::new("shared")
            .num_columns(3)
            .striped(true)
            .show(ui, |ui| {
                for f in &shared {
                    ui.add(egui::Label::new(&f.name).truncate())
                        .on_hover_text(&f.name);
                    let status = match &f.status {
                        FileStatus::Offered => format!("{}, waiting for Save", size(f.size)),
                        FileStatus::Sending(n) => {
                            let pct = (n * 100).checked_div(f.size).unwrap_or(100);
                            format!("{pct}% of {}", size(f.size))
                        }
                        FileStatus::Sent => format!("{}, sent", size(f.size)),
                        FileStatus::Cancelled => "stopped by the browser".into(),
                        FileStatus::Failed(e) => format!("failed: {e}"),
                    };
                    ui.label(status);
                    if ui.small_button("Stop sharing").clicked() {
                        self.host.command(Command::StopSharing(f.id));
                    }
                    ui.end_row();
                }
            });
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        let d = &mut self.draft;
        egui::Grid::new("settings")
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                ui.label("Port (TCP and UDP)");
                ui.add(egui::DragValue::new(&mut d.port).range(1..=65534));
                ui.end_row();
                ui.label("ICE-TCP fallback");
                ui.checkbox(
                    &mut d.ice_tcp,
                    format!("on TCP {}", d.port.saturating_add(1)),
                );
                ui.end_row();
                ui.label("Public addresses");
                ui.add(
                    egui::TextEdit::singleline(&mut d.public_ips)
                        .hint_text("WAN IPs that forward here (optional)"),
                );
                ui.end_row();
                ui.label("Web client folder");
                ui.add(
                    egui::TextEdit::singleline(&mut d.web_root)
                        .hint_text("empty: the one built into the host"),
                );
                ui.end_row();

                ui.label("Display");
                let selected = match d.display {
                    None => "Primary".to_string(),
                    Some(id) => self
                        .displays
                        .iter()
                        .find(|(i, _)| *i == id)
                        .map(|(_, name)| name.clone())
                        .unwrap_or_else(|| format!("Display {id}")),
                };
                egui::ComboBox::from_id_salt("display")
                    .selected_text(selected)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut d.display, None, "Primary");
                        for (id, name) in &self.displays {
                            ui.selectable_value(&mut d.display, Some(*id), name);
                        }
                    });
                ui.end_row();

                ui.label("Codec");
                combo(ui, "codec", &mut d.codec, CODECS);
                ui.end_row();
                ui.label("Size");
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut d.width).range(320..=7680));
                    ui.label("x");
                    ui.add(egui::DragValue::new(&mut d.height).range(180..=4320));
                });
                ui.end_row();
                ui.label("Frame rate");
                ui.add(
                    egui::DragValue::new(&mut d.fps)
                        .range(1..=240)
                        .suffix(" fps"),
                );
                ui.end_row();
                ui.label("Bitrate cap");
                ui.add(
                    egui::DragValue::new(&mut d.kbps)
                        .range(500..=200_000)
                        .speed(100)
                        .suffix(" kbps"),
                );
                ui.end_row();
                ui.label("Quality");
                combo(ui, "quality", &mut d.quality, QUALITIES);
                ui.end_row();

                ui.label("Game audio");
                ui.checkbox(&mut d.game_audio, "send to the browser");
                ui.end_row();
                ui.label("Audio quality");
                ui.add_enabled_ui(d.game_audio, |ui| {
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::DragValue::new(&mut d.audio_kbps)
                                .range(32..=510)
                                .suffix(" kbps"),
                        )
                        .on_hover_text("192 kbps and up is hard to tell from the original");
                        egui::ComboBox::from_id_salt("audio-frame")
                            .selected_text(format!("{} ms frames", d.audio_frame_ms))
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut d.audio_frame_ms,
                                    10,
                                    "10 ms (lowest delay)",
                                );
                                ui.selectable_value(
                                    &mut d.audio_frame_ms,
                                    20,
                                    "20 ms (better sound)",
                                );
                            });
                    });
                });
                ui.end_row();
                ui.label("Browser mic");
                ui.horizontal(|ui| {
                    ui.checkbox(&mut d.mic, "play into");
                    ui.add_enabled(
                        d.mic,
                        egui::TextEdit::singleline(&mut d.mic_device).desired_width(140.0),
                    );
                });
                ui.end_row();
                ui.label("Resolution");
                ui.checkbox(
                    &mut d.allow_resolution_change,
                    "let the browser change this PC's resolution",
                );
                ui.end_row();
                ui.label("Files");
                ui.vertical(|ui| {
                    ui.checkbox(&mut d.allow_upload, "let the browser send files to this PC")
                        .on_hover_text(
                            "They are saved in this folder, marked as downloaded from the \
                             internet, and never opened by the host",
                        );
                    ui.add_enabled(
                        d.allow_upload,
                        egui::TextEdit::singleline(&mut d.upload_dir)
                            .hint_text("Downloads\\web-stream"),
                    );
                });
                ui.end_row();
                ui.label("Updates");
                ui.checkbox(
                    &mut d.allow_remote_update,
                    "let the browser update and restart this host",
                )
                .on_hover_text(
                    "Adds \"Update host\" to the browser's settings: it runs \
                     scripts\\update.ps1 here (pull main, build, restart). Only paired \
                     browsers can use it",
                );
                ui.end_row();
                ui.label("More options");
                ui.add(
                    egui::TextEdit::singleline(&mut d.extra_args)
                        .hint_text("ws-host flags, e.g. --cert a.pem --key b.pem"),
                );
                ui.end_row();
                ui.label("On launch");
                ui.checkbox(&mut d.start_hosting, "start hosting when the app opens");
                ui.end_row();
            });

        #[cfg(windows)]
        {
            let mut autostart = self.autostart;
            if ui
                .checkbox(&mut autostart, "Start with Windows (in the tray)")
                .changed()
            {
                match crate::win::set_autostart(autostart) {
                    Ok(()) => self.autostart = autostart,
                    Err(e) => self.host.log(format!("could not change autostart: {e:#}")),
                }
            }
        }

        if let Some(e) = &self.settings_error {
            ui.colored_label(Color32::RED, e);
        }
        let changed = self.draft != self.settings;
        ui.horizontal(|ui| {
            if ui
                .add_enabled(changed, egui::Button::new("Save and restart"))
                .on_hover_text("Settings take effect when the host starts")
                .clicked()
                && self.save_settings()
            {
                if self.host.is_active() {
                    self.host.stop();
                    self.restart = true;
                } else {
                    self.start();
                }
            }
            if ui.add_enabled(changed, egui::Button::new("Undo")).clicked() {
                self.draft = self.settings.clone();
                self.settings_error = None;
            }
            if ui
                .button("Copy command line")
                .on_hover_text("The same host as a ws-host command")
                .clicked()
            {
                match self.draft.command_line() {
                    Ok(cmd) => ui.ctx().copy_text(cmd),
                    Err(e) => self.settings_error = Some(format!("{e:#}")),
                }
            }
        });
    }

    fn log_ui(&self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .max_height(140.0)
            .stick_to_bottom(true)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for line in &self.host.state().log {
                    ui.monospace(line);
                }
            });
    }
}

fn combo(ui: &mut egui::Ui, id: &str, value: &mut String, choices: &[&str]) {
    egui::ComboBox::from_id_salt(id)
        .selected_text(value.as_str())
        .show_ui(ui, |ui| {
            for c in choices {
                ui.selectable_value(value, c.to_string(), *c);
            }
        });
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        #[cfg(windows)]
        {
            if let Some(n) = self.hide_in_frames {
                if n == 0 {
                    crate::win::hide_window();
                    self.hide_in_frames = None;
                } else {
                    self.hide_in_frames = Some(n - 1);
                    ctx.request_repaint();
                }
            }
            if ctx.input(|i| i.viewport().close_requested()) {
                // Closing the window leaves the host running in the tray.
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                crate::win::hide_window();
            }
        }

        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if !dropped.is_empty() {
            if self.host.state().connected.is_some() {
                self.host.command(Command::ShareFiles(dropped));
            } else {
                self.host
                    .log("no browser is connected: connect one, then share the file");
            }
        }

        egui::TopBottomPanel::bottom("footer").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Open data folder").clicked() {
                    open_folder(&self.data_dir);
                }
                ui.weak("certificate, paired browsers, settings and log");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Quit").clicked() {
                        self.quit(ctx);
                    }
                });
            });
        });
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                self.status_ui(ui);
                ui.separator();
                self.session_ui(ui);
                ui.separator();
                egui::CollapsingHeader::new("Settings").show(ui, |ui| self.settings_ui(ui));
                egui::CollapsingHeader::new("Activity")
                    .default_open(true)
                    .show(ui, |ui| self.log_ui(ui));
            });
        });

        // A pending restart waits for the old host to stop.
        if self.restart {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.host.stop_and_wait(Duration::from_secs(15));
    }
}

/// Opens the file dialog on a thread of its own, so the window keeps
/// drawing, and shares what is picked.
#[cfg(windows)]
fn pick_files(host: Host) {
    let spawned = std::thread::Builder::new()
        .name("ws-pick-files".into())
        .spawn(move || {
            if let Some(paths) = rfd::FileDialog::new()
                .set_title("Send to the browser")
                .pick_files()
            {
                host.command(Command::ShareFiles(paths));
            }
        });
    if let Err(e) = spawned {
        tracing::warn!("file dialog: {e}");
    }
}

fn size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64;
    let mut unit = 0;
    v /= 1024.0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if v < 10.0 {
        format!("{v:.1} {}", UNITS[unit])
    } else {
        format!("{v:.0} {}", UNITS[unit])
    }
}

fn open_folder(dir: &std::path::Path) {
    let _ = std::fs::create_dir_all(dir);
    #[cfg(windows)]
    let opener = "explorer";
    #[cfg(not(windows))]
    let opener = "xdg-open";
    if let Err(e) = std::process::Command::new(opener).arg(dir).spawn() {
        tracing::warn!("could not open {}: {e}", dir.display());
    }
}
