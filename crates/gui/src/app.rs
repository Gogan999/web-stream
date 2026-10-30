//! The window: status, pairing, the connected browser and settings.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText};
#[cfg(windows)]
use ws_host::serve::Notice;
use ws_host::serve::{Command, Event};

use crate::firstrun::{self, FirstRun};
use crate::host::{FileStatus, Host, PairingCode, Status};
use crate::settings::{CODECS, GAMEPADS, QUALITIES, Settings, TUNNELS};
use crate::{portcheck, qr, upnp};

pub struct App {
    host: Host,
    /// What the host was last started with (and what is saved).
    settings: Settings,
    /// What the settings form shows.
    draft: Settings,
    data_dir: PathBuf,
    displays: Vec<(u32, String)>,
    /// Playback devices, for game audio and the browser mic.
    audio_devices: Vec<String>,
    /// Restart the host once the running one has stopped.
    restart: bool,
    confirm_unpair: bool,
    settings_error: Option<String>,
    #[cfg(windows)]
    autostart: bool,
    /// The web-stream service, which hosts instead of this app while it is
    /// installed.
    #[cfg(windows)]
    service: crate::service_watch::Watcher,
    first_run: FirstRun,
    /// Shows the first-run setup at the top of the window.
    show_setup: bool,
    upnp: &'static upnp::Mapper,
    /// Frames until the window hides itself, for `--hidden`. eframe shows
    /// the window after the first frame whatever it was built with.
    #[cfg_attr(not(windows), allow(dead_code))]
    hide_in_frames: Option<u8>,
    /// Whether the pairing QR code is drawn. Off at first: the code to type
    /// is the way to pair, and the QR code carries the pairing secret, so it
    /// should not sit on screen unless asked for. Not saved.
    show_qr: bool,
}

impl App {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        settings: Settings,
        data_dir: PathBuf,
        hidden: bool,
        pair: bool,
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
        #[cfg(windows)]
        let service = {
            let repaint = cc.egui_ctx.clone();
            crate::service_watch::Watcher::start(host.clone(), pair, move || {
                repaint.request_repaint()
            })
        };

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

        let audio_devices = ws_host::audio_devices().unwrap_or_else(|e| {
            tracing::warn!("could not list audio devices: {e}");
            Vec::new()
        });

        let repaint = cc.egui_ctx.clone();
        let upnp = upnp::mapper(move || repaint.request_repaint());
        if settings.upnp {
            upnp.want(Some(settings.forwarded_ports()));
        }

        let mut app = Self {
            first_run: FirstRun::new(),
            show_setup: !settings.setup_done,
            upnp,
            host,
            draft: settings.clone(),
            settings,
            data_dir,
            displays,
            audio_devices,
            restart: false,
            confirm_unpair: false,
            settings_error: None,
            #[cfg(windows)]
            autostart: crate::win::autostart_enabled(),
            #[cfg(windows)]
            service,
            hide_in_frames: hidden.then_some(2),
            show_qr: false,
        };
        // A browser may ask for an update (when the settings allow it):
        // update.ps1 builds, then this app restarts itself on the build.
        let restarting = app.host.clone();
        ws_host::update::enable(move || crate::restart_after_update(&restarting));

        // The service hosts on the same ports.
        if app.service_installed().is_some() {
            #[cfg(windows)]
            app.host.log(if app.service.elevated {
                "the Windows service hosts this PC: this app shows it and applies saved \
                 settings to it"
            } else {
                "the Windows service hosts this PC: \"Connect\" (administrator) shows it here"
            });
        } else if app.settings.start_hosting || pair {
            app.start();
            if pair {
                app.host.command(Command::OpenPairing);
            }
        }
        app
    }

    /// Whether the service is installed (`Some`), and running.
    fn service_installed(&self) -> Option<bool> {
        #[cfg(windows)]
        return self.service.installed();
        #[cfg(not(windows))]
        None
    }

    /// Starts this app's own host, unless the service is installed: only
    /// one host may run.
    fn start(&mut self) {
        if self.service_installed().is_some() {
            self.host.log(
                "the Windows service is installed and hosts this PC, so this app doesn't \
                 start a host of its own (uninstall the service first)",
            );
            return;
        }
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
        // Saved to its own file; the form doesn't keep it.
        self.draft.tunnel_token.clear();
        self.settings_error = None;
        self.settings = self.draft.clone();
        // The ports may have changed.
        self.upnp
            .want(self.settings.upnp.then(|| self.settings.forwarded_ports()));
        true
    }

    /// Changes one setting the setup page shows (not part of the form) and
    /// saves it, leaving unsaved edits in the form as they are.
    fn save_one(&mut self, change: impl Fn(&mut Settings)) {
        change(&mut self.settings);
        change(&mut self.draft);
        if let Err(e) = self.settings.save(&self.data_dir) {
            self.host.log(format!("could not save the settings: {e:#}"));
        }
    }

    /// The first-run setup: reaching the host from outside, then pairing a
    /// phone.
    fn setup_ui(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("Set up this PC").strong().size(16.0));
        ui.add_space(4.0);
        ui.label(RichText::new("1. Use it away from home (optional)").strong());
        ui.label(
            "At home on the same Wi-Fi nothing is needed. From elsewhere, a Cloudflare Tunnel \
             (Settings below) serves the page with no certificate warning, and the router has \
             to forward these ports to this PC:",
        );
        ui.monospace(forward_ports(&self.settings));
        match (
            self.settings.tunnel_on(),
            self.host.state().tunnel_url.clone(),
        ) {
            (true, Some(url)) => {
                ui.colored_label(Color32::GREEN, format!("Cloudflare Tunnel up at {url}/"));
            }
            (true, None) => {
                ui.colored_label(
                    Color32::YELLOW,
                    "Cloudflare Tunnel not up yet (it is when the host is running and the \
                     token is right).",
                );
            }
            (false, _) => {
                ui.weak("Cloudflare Tunnel: off (Settings > Cloudflare Tunnel).");
            }
        }
        let mut on = self.settings.upnp;
        if ui
            .checkbox(&mut on, "Let the router forward them (UPnP)")
            .on_hover_text(
                "Asks the router to forward only these ports to this PC, renewed every 20 \
                 minutes while the app runs. Quitting and uninstalling remove them. Leave it off \
                 to forward them yourself on the router's settings page",
            )
            .changed()
        {
            self.save_one(|s| s.upnp = on);
            self.upnp.want(on.then(|| self.settings.forwarded_ports()));
        }
        firstrun::upnp_status_ui(ui, &self.upnp.status());

        let (testing, report) = self.first_run.report();
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!testing, egui::Button::new("Test the ports"))
                .on_hover_text(
                    "Connects to this PC's public address the way a phone away from home would",
                )
                .clicked()
            {
                let fingerprint = match &self.host.state().status {
                    Status::Running { fingerprint, .. } => Some(fingerprint.clone()),
                    _ => None,
                };
                let input = portcheck::Input {
                    port: self.settings.port,
                    ice_tcp_port: self.settings.ice_tcp.then(|| {
                        self.settings
                            .ice_tcp_port
                            .unwrap_or(self.settings.port.saturating_add(1))
                    }),
                    fingerprint,
                    tunnel: self
                        .settings
                        .tunnel_on()
                        .then(|| self.host.state().tunnel_url.clone()),
                    public_ips: self
                        .settings
                        .public_ips
                        .split([' ', ',', ';'])
                        .filter_map(|s| s.trim().parse().ok())
                        .collect(),
                };
                self.first_run.start_check(input, ui.ctx().clone());
            }
            if testing {
                ui.spinner();
                ui.weak("testing...");
            }
        });
        if let Some(report) = &report {
            firstrun::report_ui(ui, report);
        }

        ui.add_space(6.0);
        ui.label(RichText::new("2. Pair your phone").strong());
        let running = matches!(self.host.state().status, Status::Running { .. });
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(running, egui::Button::new("Pair a phone"))
                .clicked()
            {
                self.host.command(Command::OpenPairing);
            }
            ui.label("then scan the QR code that appears below with the phone's camera.");
        });
        ui.add_space(6.0);
        if ui
            .button("Done")
            .on_hover_text("\"Setup\" at the bottom brings this back")
            .clicked()
        {
            self.show_setup = false;
            self.save_one(|s| s.setup_done = true);
        }
    }

    /// The public address the port test found this host at, for the QR
    /// code. Not the one the router reports: any device on the network can
    /// answer UPnP, and the QR code carries the pairing secret.
    fn public_ips(&self) -> Vec<std::net::IpAddr> {
        match self.first_run.report() {
            (_, Some(r)) => r.verified_ip.into_iter().collect(),
            _ => Vec::new(),
        }
    }

    /// The addresses a device could open the client at, and the one in use.
    /// Shared by the instructions and the QR code so both name the same
    /// address. `None` while the host is not running or has nothing to offer.
    fn pairing_addresses(&self) -> Option<(Vec<String>, String)> {
        let (port, lan, tunnel_url) = {
            let state = self.host.state();
            match &state.status {
                Status::Running { port, ips, .. } => (*port, ips.clone(), state.tunnel_url.clone()),
                _ => return None,
            }
        };
        let choices = firstrun::addresses(
            &self.settings,
            port,
            tunnel_url.as_deref(),
            &lan,
            &self.public_ips(),
        );
        let first = choices.first().cloned()?;
        let host = match &self.first_run.qr_host {
            Some(h) if choices.contains(h) => h.clone(),
            _ => first,
        };
        Some((choices, host))
    }

    /// The choice of address, remembered for the instructions and the QR code.
    fn address_combo(&mut self, ui: &mut egui::Ui, choices: &[String], host: &str) {
        let mut selected = host.to_string();
        egui::ComboBox::from_id_salt("qr-host")
            .selected_text(&selected)
            .show_ui(ui, |ui| {
                for c in choices {
                    ui.selectable_value(&mut selected, c.clone(), c);
                }
            })
            .response
            .on_hover_text(
                "The Cloudflare Tunnel address works anywhere. A home address works on the \
                 same Wi-Fi; the public address away from home, once the ports are forwarded",
            );
        if selected != host {
            self.first_run.qr_host = Some(selected);
        }
    }

    /// What to do while pairing is open and no device has asked yet. The code
    /// only exists once a device asks for it, so this says where to ask.
    fn pairing_instructions_ui(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        match self.pairing_addresses() {
            Some((choices, host)) => {
                ui.label(
                    RichText::new(format!(
                        "On the device, open {host} and ask for a pairing code there."
                    ))
                    .size(18.0)
                    .strong(),
                );
                ui.horizontal(|ui| {
                    ui.label("Address:");
                    self.address_combo(ui, &choices, &host);
                });
            }
            None => {
                ui.label(
                    RichText::new(
                        "On the device, open this PC's address and ask for a \
                         pairing code there.",
                    )
                    .size(18.0)
                    .strong(),
                );
            }
        }
        ui.weak("The code appears here, with the device's name.");
    }

    /// The pairing link as a QR code. `with_choice` adds the address choice;
    /// the instructions already show it while waiting for a code.
    fn pairing_qr_ui(&mut self, ui: &mut egui::Ui, link: &str, with_choice: bool) {
        let Some((choices, host)) = self.pairing_addresses() else {
            ui.weak(
                "No address to put in a QR code yet: the host is not running or has no \
                 address.",
            );
            return;
        };
        if with_choice {
            ui.horizontal(|ui| {
                ui.label("Scan with the phone's camera. Address:");
                self.address_combo(ui, &choices, &host);
            });
        } else {
            ui.label("Scan with the phone's camera.");
        }
        let url = firstrun::pairing_url(&host, link);
        qr::show(ui, &url, 200.0).on_hover_text("Pairs one browser, once, while pairing is open");
        ui.weak("Pairs once, while pairing is open. Anyone who can see this code can pair.");
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
        #[cfg(windows)]
        if let Some(running) = self.service_installed() {
            self.service_status_ui(ui, running, &status);
        } else {
            self.own_status_ui(ui, active, &status);
        }
        #[cfg(not(windows))]
        self.own_status_ui(ui, active, &status);
        self.running_ui(ui, &status);
    }

    /// Start and Stop for this app's own host.
    fn own_status_ui(&mut self, ui: &mut egui::Ui, active: bool, status: &Status) {
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
    }

    /// The service's state, and what this app can do about it: attached
    /// (elevated), it shows the service's host like its own.
    #[cfg(windows)]
    fn service_status_ui(&mut self, ui: &mut egui::Ui, running: bool, status: &Status) {
        let elevated = self.service.elevated;
        let attached = self.host.attached();
        ui.horizontal(|ui| {
            let (text, color) = match (running, status) {
                (false, _) => ("The Windows service is stopped".to_string(), Color32::GRAY),
                (true, Status::Running { port, .. }) if attached => (
                    format!("The Windows service is hosting on port {port}"),
                    Color32::GREEN,
                ),
                (true, _) if elevated => (
                    "The Windows service is starting its host...".to_string(),
                    Color32::YELLOW,
                ),
                (true, _) => (
                    "The Windows service hosts this PC".to_string(),
                    Color32::GREEN,
                ),
            };
            ui.label(RichText::new(text).strong().color(color))
                .on_hover_text(
                    "While the service is installed it hosts this PC, and this app doesn't: \
                     only one host can have the ports",
                );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if !running {
                    if ui.button("Start the service").clicked() {
                        self.run_service(&["start"]);
                    }
                } else if !elevated {
                    if ui
                        .button("Connect (administrator)")
                        .on_hover_text(
                            "Opens this app as administrator, which pairing, the connected \
                             browser and files for the service take",
                        )
                        .clicked()
                    {
                        self.hand_over(false);
                    }
                } else if ui
                    .button("Stop the service")
                    .on_hover_text("Until you start it again, or Windows restarts")
                    .clicked()
                {
                    self.run_service(&["stop"]);
                }
            });
        });
        if running && !attached && !elevated {
            ui.weak(
                "Pairing, the connected browser and sending files take administrator rights \
                 while the service hosts: click Connect.",
            );
        }
    }

    /// Hands over to a copy of this app run as administrator (a UAC
    /// prompt), which attaches to the service's host; with `pair`, it opens
    /// pairing once attached. This copy exits once that one has started.
    #[cfg(windows)]
    fn hand_over(&mut self, pair: bool) {
        let mut args = vec!["--after".to_string(), std::process::id().to_string()];
        if pair {
            args.push("--pair".into());
        }
        match crate::win::run_elevated(&args) {
            Ok(()) => crate::win::hand_over(&self.host),
            Err(e) => self.host.log(format!("connect to the service: {e:#}")),
        }
    }

    /// Runs `service <rest>` as administrator: in a child process when this
    /// app already is one (the result goes to the log), else with a UAC
    /// prompt (the result shows in a message box).
    #[cfg(windows)]
    fn run_service(&mut self, rest: &[&str]) {
        let args: Vec<String> = std::iter::once("service")
            .chain(rest.iter().copied())
            .map(String::from)
            .collect();
        let what = rest.first().copied().unwrap_or_default().to_string();
        if self.service.elevated {
            let host = self.host.clone();
            let service = self.service.clone();
            host.log(format!("service {what}..."));
            let failed = format!("service {what}");
            let spawned = std::thread::Builder::new()
                .name("ws-service-cmd".into())
                .spawn(move || {
                    let result = std::env::current_exe().and_then(|exe| {
                        std::process::Command::new(exe)
                            .args(&args)
                            .arg("--quiet")
                            .output()
                    });
                    match result {
                        Ok(out) if out.status.success() => {
                            host.log(format!("service {what}: done"))
                        }
                        Ok(out) => {
                            let text = String::from_utf8_lossy(&out.stdout);
                            let last = text.lines().rev().find(|l| !l.trim().is_empty());
                            host.log(format!(
                                "service {what} failed: {}",
                                last.unwrap_or("see service.log")
                            ));
                        }
                        Err(e) => host.log(format!("service {what}: {e}")),
                    }
                    service.refresh();
                });
            if let Err(e) = spawned {
                self.host.log(format!("{failed}: {e}"));
            }
        } else if let Err(e) = crate::win::run_elevated(&args) {
            self.host.log(format!("service: {e:#}"));
        }
        self.service.refresh();
    }

    /// Where the running host (this app's own, or the service's) can be
    /// reached.
    fn running_ui(&mut self, ui: &mut egui::Ui, status: &Status) {
        match status {
            Status::Failed(e) => {
                ui.colored_label(Color32::RED, e);
            }
            Status::Running {
                port,
                ips,
                fingerprint,
                self_signed,
                web_root,
            } => {
                let tunnel_url = self.host.state().tunnel_url.clone();
                egui::Grid::new("running").num_columns(2).show(ui, |ui| {
                    ui.label("Open in the browser");
                    ui.vertical(|ui| {
                        if let Some(url) = &tunnel_url {
                            let url = format!("{url}/");
                            if ui
                                .link(RichText::new(&url).monospace())
                                .on_hover_text("Through the Cloudflare Tunnel. Click to copy")
                                .clicked()
                            {
                                ui.ctx().copy_text(url);
                            }
                        }
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
                    ui.label("Forward on the router")
                        .on_hover_text("Only to reach this PC from outside your network");
                    ui.monospace(forward_ports(&self.settings));
                    ui.end_row();
                    ui.label("Certificate SHA-256");
                    ui.vertical(|ui| {
                        ui.add(
                            egui::Label::new(RichText::new(fingerprint).monospace().small()).wrap(),
                        );
                        if *self_signed {
                            ui.weak(
                                "Self-signed: browsers warn until you accept it. Compare this \
                                 fingerprint before you do. The Cloudflare Tunnel address \
                                 (Settings) has no warning.",
                            )
                            .on_hover_text(
                                "Someone on the network could show a different certificate; \
                                 clicking past that warning would let them pair",
                            );
                        }
                    });
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

        let link = self.host.state().pairing_link.clone();
        let view = pairing_view(pairing_until, code.as_ref(), link.is_some(), locked, now);

        ui.horizontal(|ui| {
            // The service hosts, and its pairing takes an administrator.
            #[cfg(windows)]
            let hand_over = self.service_installed() == Some(true) && !self.service.elevated;
            #[cfg(not(windows))]
            let hand_over = false;
            if hand_over {
                if ui
                    .button("Pair a browser (administrator)")
                    .on_hover_text(
                        "Opens this app as administrator, attached to the service, and lets a \
                         browser ask for a pairing code for two minutes",
                    )
                    .clicked()
                {
                    #[cfg(windows)]
                    self.hand_over(true);
                }
            } else if ui
                .add_enabled(running, egui::Button::new("Pair a browser"))
                .on_hover_text("Lets a browser ask for a pairing code for two minutes")
                .clicked()
            {
                self.host.command(Command::OpenPairing);
            }
            if let Some(left) = view.countdown {
                // Locked codes can't be requested, so don't say to.
                let tail = if view.locked {
                    ""
                } else {
                    ": request a code in the browser"
                };
                ui.label(format!(
                    "Pairing open for {}:{:02}{tail}",
                    left / 60,
                    left % 60
                ));
            }
        });
        // The red notice lives here, not beside the button, so it is said once.
        if let Some(notice) = locked_notice(&view) {
            ui.colored_label(Color32::RED, notice);
        }
        match (view.stage, &code) {
            (PairingStage::Code, Some(code)) => {
                ui.add_space(4.0);
                ui.label(format!("Type this code in {:?}:", code.device_name));
                ui.label(RichText::new(&code.code).monospace().size(36.0).strong());
                let left = code.expires.saturating_duration_since(now).as_secs();
                ui.weak(format!("expires in {left}s"));
            }
            (PairingStage::AwaitingCode, _) => self.pairing_instructions_ui(ui),
            (PairingStage::CodesLocked, _) => {}
            _ => {}
        }
        // Whatever a browser does with codes: anyone who can reach the port
        // can ask for one.
        if let (true, Some(link)) = (view.offer_qr, &link) {
            ui.checkbox(&mut self.show_qr, "Show QR code");
            if self.show_qr {
                self.pairing_qr_ui(ui, link, view.stage != PairingStage::AwaitingCode);
            }
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
                ui.label("ICE-TCP fallback")
                    .on_hover_text("Media over TCP for networks that block UDP");
                ui.horizontal(|ui| {
                    ui.checkbox(&mut d.ice_tcp, "on TCP");
                    let next = d.port.saturating_add(1);
                    let mut port = d.ice_tcp_port.unwrap_or(next);
                    if ui
                        .add_enabled(d.ice_tcp, egui::DragValue::new(&mut port).range(1..=65535))
                        .changed()
                    {
                        // The port after the main one follows it.
                        d.ice_tcp_port = (port != next).then_some(port);
                    }
                });
                ui.end_row();
                ui.label("Public addresses");
                ui.add(
                    egui::TextEdit::singleline(&mut d.public_ips)
                        .hint_text("WAN IPs that forward here (optional)"),
                );
                ui.end_row();
                ui.label("Host names").on_hover_text(
                    "Names or IPs browsers use to reach this PC, put in its \
                         self-signed certificate. Changing them makes a new certificate, \
                         which browsers warn about once",
                );
                ui.add(
                    egui::TextEdit::singleline(&mut d.hostnames)
                        .hint_text("e.g. a DDNS name (optional)"),
                );
                ui.end_row();
                ui.label("Cloudflare Tunnel").on_hover_text(
                    "Serves the web page through Cloudflare, with Cloudflare's certificate: no \
                     warning in the browser, and no HTTPS port to forward. Video, audio and \
                     input still come straight to this PC. Named: a tunnel made in the \
                     Cloudflare dashboard, with a name that stays. Quick: no account, but a new \
                     trycloudflare.com address (and pairing again) every time the host starts",
                );
                let label = TUNNELS
                    .iter()
                    .find(|(v, _)| *v == d.tunnel)
                    .map_or("Off", |(_, l)| *l);
                egui::ComboBox::from_id_salt("tunnel")
                    .selected_text(label)
                    .show_ui(ui, |ui| {
                        for (value, label) in TUNNELS {
                            ui.selectable_value(&mut d.tunnel, value.to_string(), *label);
                        }
                    });
                ui.end_row();
                if d.tunnel == "named" {
                    let saved = self
                        .data_dir
                        .join(ws_host::options::TUNNEL_TOKEN_FILE)
                        .exists();
                    ui.label("Tunnel token");
                    ui.add(
                        egui::TextEdit::singleline(&mut d.tunnel_token)
                            .password(true)
                            .hint_text(if saved {
                                "saved (paste to replace)"
                            } else {
                                "from the Cloudflare dashboard"
                            }),
                    );
                    ui.end_row();
                    ui.label("Tunnel address").on_hover_text(
                        "The public hostname set for the tunnel in the dashboard. Empty: the \
                         one cloudflared reports",
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut d.tunnel_hostname)
                            .hint_text("e.g. pc.example.com (optional)"),
                    );
                    ui.end_row();
                }
                ui.label("Forget browsers");
                ui.add(
                    egui::DragValue::new(&mut d.forget_after_days)
                        .range(1..=3650)
                        .prefix("unused for ")
                        .suffix(" days"),
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
                ui.horizontal(|ui| {
                    ui.checkbox(&mut d.game_audio, "send, from");
                    ui.add_enabled_ui(d.game_audio, |ui| {
                        device_combo(
                            ui,
                            "audio-device",
                            &mut d.audio_device,
                            ("", "Default output"),
                            &self.audio_devices,
                        );
                    });
                });
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
                    ui.add_enabled_ui(d.mic, |ui| {
                        device_combo(
                            ui,
                            "mic-device",
                            &mut d.mic_device,
                            ("auto", "auto (CABLE Input, else Steam)"),
                            &self.audio_devices,
                        );
                    });
                });
                ui.end_row();
                ui.label("Gamepads");
                let name = GAMEPADS
                    .iter()
                    .find(|(v, _)| *v == d.gamepad)
                    .map_or(d.gamepad.as_str(), |(_, n)| n);
                egui::ComboBox::from_id_salt("gamepad")
                    .selected_text(format!("as {name}"))
                    .show_ui(ui, |ui| {
                        for (value, name) in GAMEPADS {
                            ui.selectable_value(&mut d.gamepad, value.to_string(), *name);
                        }
                    })
                    .response
                    .on_hover_text(
                        "The virtual controller each browser gamepad becomes (ViGEmBus)",
                    );
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
                     scripts\\update.ps1 here (pull main, build), then this app restarts on \
                     the new build. Only paired browsers can use it",
                );
                ui.end_row();
                ui.label("More options");
                ui.add(
                    egui::TextEdit::singleline(&mut d.extra_args)
                        .hint_text("host flags, e.g. --cert a.pem --key b.pem"),
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
        let service = self.service_installed().is_some();
        ui.horizontal(|ui| {
            let (label, hover) = if service {
                (
                    "Save and apply to the service",
                    "Reinstalls the service with these settings and restarts it (an \
                     administrator prompt, unless this app runs as one)",
                )
            } else {
                (
                    "Save and restart",
                    "Settings take effect when the host starts",
                )
            };
            if ui
                .add_enabled(changed, egui::Button::new(label))
                .on_hover_text(hover)
                .clicked()
                && self.save_settings()
            {
                if service {
                    #[cfg(windows)]
                    self.apply_to_service();
                } else if self.host.is_active() {
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
        });

        #[cfg(windows)]
        {
            ui.separator();
            self.service_ui(ui);
        }
    }

    /// Reinstalls the service with the saved settings, which restarts it.
    #[cfg(windows)]
    fn apply_to_service(&mut self) {
        match self.settings.service_args() {
            Ok(rest) => {
                let mut args = vec!["install"];
                args.extend(rest.iter().map(String::as_str));
                self.run_service(&args);
            }
            Err(e) => self.settings_error = Some(format!("{e:#}")),
        }
    }

    /// The web-stream service: streams the sign-in screen, UAC prompts and
    /// elevated programs. Each action takes an administrator.
    #[cfg(windows)]
    fn service_ui(&mut self, ui: &mut egui::Ui) {
        let installed = self.service_installed();
        ui.label(RichText::new("Windows service").strong())
            .on_hover_text(
                "Runs the host as a Windows service, so the browser can also use the sign-in \
                 screen, UAC prompts and programs running as administrator. It uses the saved \
                 settings except the web client folder, received files and browser updates, \
                 and has its own paired browsers",
            );
        ui.label(match installed {
            None => "Not installed: this app hosts",
            Some(_) => "Installed: it hosts this PC instead of this app, with the saved settings",
        });
        ui.horizontal_wrapped(|ui| match installed {
            None => {
                if ui
                    .button("Install the service")
                    .on_hover_text(
                        "With the saved settings. This app stops its own host first: they \
                         share the ports",
                    )
                    .clicked()
                {
                    // Once it is installed, this app's own host stops (see
                    // service_watch); the service retries until the ports
                    // are free.
                    self.apply_to_service();
                }
            }
            Some(_) => {
                if ui
                    .button("Update the service")
                    .on_hover_text("Copies this build to the service and restarts it")
                    .clicked()
                {
                    self.run_service(&["update"]);
                }
                if ui
                    .button("Uninstall")
                    .on_hover_text(
                        "Removes the service; its paired browsers and logs stay. Then this \
                         app hosts again (Start)",
                    )
                    .clicked()
                {
                    self.run_service(&["uninstall"]);
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

/// A playback device by name: `first` (value, label) then `devices`, and
/// the saved name if it is not among them (unplugged, or typed elsewhere).
fn device_combo(
    ui: &mut egui::Ui,
    id: &str,
    value: &mut String,
    first: (&str, &str),
    devices: &[String],
) {
    let selected = if value == first.0 {
        first.1
    } else {
        value.as_str()
    };
    egui::ComboBox::from_id_salt(id)
        .selected_text(selected.to_string())
        .width(220.0)
        .show_ui(ui, |ui| {
            ui.selectable_value(value, first.0.to_string(), first.1);
            if *value != first.0 && !devices.contains(value) {
                let v = value.clone();
                ui.selectable_value(value, v.clone(), v);
            }
            for d in devices {
                ui.selectable_value(value, d.clone(), d);
            }
        });
}

/// The ports a router forwards to this PC, as the host was started.
fn forward_ports(s: &Settings) -> String {
    let mut out = if s.tunnel_on() {
        format!("UDP {}", s.port)
    } else {
        format!("TCP {p}, UDP {p}", p = s.port)
    };
    if s.ice_tcp {
        let tcp = s.ice_tcp_port.unwrap_or(s.port.saturating_add(1));
        out.push_str(&format!(", TCP {tcp}"));
    }
    out
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
                if !self.show_setup && ui.button("Setup").clicked() {
                    self.show_setup = true;
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
                if self.show_setup {
                    egui::Frame::group(ui.style()).show(ui, |ui| self.setup_ui(ui));
                    ui.add_space(4.0);
                }
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
        upnp::shutdown();
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

/// Which part of the pairing flow the window is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PairingStage {
    /// Nothing to pair.
    Closed,
    /// Pairing is open and no device has asked for a code yet.
    AwaitingCode,
    /// A device asked and its code is on screen.
    Code,
    /// Pairing is open but codes are locked after wrong guesses. Its own
    /// stage so the "ask for a code" instructions can't show: asking can't
    /// work now, though the QR link and reopening pairing both still do.
    CodesLocked,
}

/// What the pairing area shows, decided apart from egui so it can be tested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PairingView {
    stage: PairingStage,
    /// Whole seconds until pairing closes, while it is open.
    countdown: Option<u64>,
    /// Codes are locked after wrong guesses.
    locked: bool,
    /// Whether the "Show QR code" toggle is offered: only with a link, which
    /// exists only while pairing is open.
    offer_qr: bool,
}

/// The code is minted per request and bound to the asking device, so until a
/// device asks there is none to show; the window says where to ask instead.
/// A code on screen wins over the instructions.
fn pairing_view(
    pairing_until: Option<Instant>,
    code: Option<&PairingCode>,
    has_link: bool,
    locked: bool,
    now: Instant,
) -> PairingView {
    let stage = match (code, pairing_until) {
        (Some(_), _) => PairingStage::Code,
        (None, Some(_)) if locked => PairingStage::CodesLocked,
        (None, Some(_)) => PairingStage::AwaitingCode,
        (None, None) => PairingStage::Closed,
    };
    PairingView {
        stage,
        countdown: pairing_until.map(|u| u.saturating_duration_since(now).as_secs()),
        locked,
        offer_qr: has_link,
    }
}

/// The red notice for locked codes, if any. A code lockout doesn't stop the
/// QR link, and pairing again clears the lockout, so an open window names
/// both; a closed one has no QR left to promise. A code on screen can't be
/// locked (the lock clears it), so that stage never gets a notice.
fn locked_notice(view: &PairingView) -> Option<&'static str> {
    if !view.locked {
        return None;
    }
    match view.stage {
        PairingStage::CodesLocked => Some(
            "Codes are locked after too many wrong tries. The QR code still works (tick \
             \"Show QR code\"), or click Pair a browser again to unlock codes.",
        ),
        PairingStage::Closed => Some(
            "Codes are locked after too many wrong tries. Click Pair a browser to \
             unlock them.",
        ),
        PairingStage::AwaitingCode | PairingStage::Code => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(now: Instant) -> PairingCode {
        PairingCode {
            code: "123456".into(),
            device_name: "Phone".into(),
            expires: now + Duration::from_secs(60),
        }
    }

    #[test]
    fn open_without_a_code_asks_the_device_for_one() {
        let now = Instant::now();
        let v = pairing_view(Some(now + Duration::from_secs(90)), None, true, false, now);
        assert_eq!(v.stage, PairingStage::AwaitingCode);
        assert_eq!(v.countdown, Some(90));
        assert!(v.offer_qr);
        assert!(!v.locked);
    }

    #[test]
    fn a_code_takes_over_from_the_instructions() {
        let now = Instant::now();
        let c = code(now);
        let v = pairing_view(
            Some(now + Duration::from_secs(90)),
            Some(&c),
            true,
            false,
            now,
        );
        assert_eq!(v.stage, PairingStage::Code);
        assert_eq!(v.countdown, Some(90));
        assert!(v.offer_qr);
    }

    #[test]
    fn locked_is_reported_whatever_the_stage() {
        let now = Instant::now();
        let v = pairing_view(None, None, false, true, now);
        assert_eq!(v.stage, PairingStage::Closed);
        assert!(v.locked);
        let open = pairing_view(Some(now + Duration::from_secs(5)), None, true, true, now);
        assert_eq!(open.stage, PairingStage::CodesLocked);
        assert!(open.locked);
    }

    #[test]
    fn locked_with_the_window_open_names_the_qr_and_reopening() {
        let now = Instant::now();
        let v = pairing_view(Some(now + Duration::from_secs(60)), None, true, true, now);
        assert_eq!(v.countdown, Some(60));
        assert!(v.offer_qr);
        let notice = locked_notice(&v).expect("a notice while locked");
        assert!(notice.contains("QR code still works"));
        assert!(notice.contains("Show QR code"));
        assert!(notice.contains("again to unlock"));
        assert!(!notice.contains("ask for"));
    }

    #[test]
    fn locked_with_the_window_closed_promises_no_qr() {
        let now = Instant::now();
        let v = pairing_view(None, None, false, true, now);
        assert!(!v.offer_qr);
        let notice = locked_notice(&v).expect("a notice while locked");
        assert!(!notice.contains("QR"));
        assert!(notice.contains("unlock"));
    }

    #[test]
    fn unlocked_has_no_locked_notice() {
        let now = Instant::now();
        let open = pairing_view(Some(now + Duration::from_secs(5)), None, true, false, now);
        assert_eq!(locked_notice(&open), None);
        assert_eq!(
            locked_notice(&pairing_view(None, None, false, false, now)),
            None
        );
    }

    #[test]
    fn closed_shows_nothing() {
        let now = Instant::now();
        let v = pairing_view(None, None, false, false, now);
        assert_eq!(
            v,
            PairingView {
                stage: PairingStage::Closed,
                countdown: None,
                locked: false,
                offer_qr: false,
            }
        );
    }

    #[test]
    fn a_deadline_already_past_counts_down_to_zero() {
        let deadline = Instant::now();
        let now = deadline + Duration::from_secs(30);
        let v = pairing_view(Some(deadline), None, true, false, now);
        assert_eq!(v.stage, PairingStage::AwaitingCode);
        assert_eq!(v.countdown, Some(0));
        assert!(v.offer_qr);
        assert!(!v.locked);
    }

    #[test]
    fn a_code_without_a_link_offers_no_qr_toggle() {
        let now = Instant::now();
        let c = code(now);
        let v = pairing_view(None, Some(&c), false, false, now);
        assert_eq!(v.stage, PairingStage::Code);
        assert!(!v.offer_qr);
    }
}
