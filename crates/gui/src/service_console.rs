//! `ws-host-gui service console`: a window on the web-stream service's
//! host, run as an elevated administrator (the tray app starts it with a
//! UAC prompt). It talks to the host over the control pipe only SYSTEM and
//! elevated administrators can open, and shows pairing codes, the
//! connected browser and what the host says.

use std::collections::VecDeque;
use std::time::Duration;

use eframe::egui::{self, Color32, RichText};
use ws_host::service::control::{self, ServiceConsole};

const LOG_LINES: usize = 300;

pub fn run(pair: bool) -> anyhow::Result<()> {
    let (rgba, size) = crate::icon_rgba();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(format!("{} service", crate::TITLE))
            .with_inner_size([560.0, 480.0])
            .with_min_inner_size([420.0, 300.0])
            .with_icon(egui::IconData {
                rgba,
                width: size,
                height: size,
            }),
        ..Default::default()
    };
    eframe::run_native(
        "web-stream host service",
        options,
        Box::new(move |cc| Ok(Box::new(ConsoleApp::new(cc.egui_ctx.clone(), pair)))),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

struct ConsoleApp {
    ctx: egui::Context,
    console: Option<ServiceConsole>,
    error: Option<String>,
    log: VecDeque<String>,
    /// The latest pairing code line, while pairing.
    code: Option<String>,
    confirm_unpair: bool,
}

impl ConsoleApp {
    fn new(ctx: egui::Context, pair: bool) -> Self {
        let mut app = Self {
            ctx,
            console: None,
            error: None,
            log: VecDeque::new(),
            code: None,
            confirm_unpair: false,
        };
        app.connect();
        if pair {
            app.send("pair");
        }
        app
    }

    fn connect(&mut self) {
        let ctx = self.ctx.clone();
        match control::connect(move || ctx.request_repaint()) {
            Ok(c) => {
                self.console = Some(c);
                self.error = None;
            }
            Err(e) => self.error = Some(format!("{e:#}")),
        }
    }

    fn send(&mut self, line: &str) {
        let sent = self.console.as_ref().is_some_and(|c| c.send(line));
        if !sent {
            self.console = None;
            self.error = Some("not connected to the service's host".into());
        }
    }

    fn receive(&mut self) {
        let Some(console) = &self.console else { return };
        loop {
            match console.lines.try_recv() {
                Ok(line) if line.trim().is_empty() => {}
                Ok(line) => {
                    if line.contains("PAIRING CODE") {
                        self.code = Some(line.trim().to_owned());
                    } else if line.starts_with("paired ") || line.contains("pairing is locked") {
                        self.code = None;
                    }
                    if self.log.len() == LOG_LINES {
                        self.log.pop_front();
                    }
                    self.log.push_back(line);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => return,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.console = None;
                    self.code = None;
                    return;
                }
            }
        }
    }
}

impl eframe::App for ConsoleApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.receive();
        egui::CentralPanel::default().show(ctx, |ui| {
            let connected = self.console.is_some();
            ui.horizontal(|ui| {
                let (text, color) = if connected {
                    ("Connected to the service's host", Color32::GREEN)
                } else {
                    ("Not connected", Color32::GRAY)
                };
                ui.label(RichText::new(text).strong().color(color));
                if !connected && ui.button("Connect again").clicked() {
                    self.connect();
                }
            });
            if let Some(e) = &self.error {
                ui.colored_label(Color32::RED, e);
            }
            if let Some(code) = &self.code {
                ui.add_space(6.0);
                ui.label(RichText::new(code).size(22.0).strong().monospace());
                ui.add_space(6.0);
            }
            ui.add_enabled_ui(connected, |ui| {
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .button("Pair a browser")
                        .on_hover_text("Opens pairing for two minutes; the code shows here")
                        .clicked()
                    {
                        self.send("pair");
                    }
                    if ui.button("Disconnect the browser").clicked() {
                        self.send("drop");
                    }
                    if ui
                        .button("Share files...")
                        .on_hover_text("Offer files to the connected browser")
                        .clicked()
                    {
                        if let Some(paths) = rfd::FileDialog::new().pick_files() {
                            for p in paths {
                                self.send(&format!("send {}", p.display()));
                            }
                        }
                    }
                    if self.confirm_unpair {
                        if ui
                            .button(RichText::new("Really unpair all").color(Color32::RED))
                            .clicked()
                        {
                            self.send("unpair-all");
                            self.confirm_unpair = false;
                        }
                        if ui.button("Cancel").clicked() {
                            self.confirm_unpair = false;
                        }
                    } else if ui.button("Unpair every browser").clicked() {
                        self.confirm_unpair = true;
                    }
                });
            });
            ui.separator();
            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for line in &self.log {
                        ui.add(egui::Label::new(RichText::new(line).monospace()).wrap());
                    }
                });
        });
        // Lines arrive with a repaint; this catches a pipe that broke.
        ctx.request_repaint_after(Duration::from_secs(1));
    }
}
