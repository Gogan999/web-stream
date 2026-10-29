//! The first-run setup: reach the host from outside (a Cloudflare Tunnel,
//! opt-in UPnP, the port test), then pair a phone with a QR code. Shown when the app
//! opens until the user finishes or hides it; "Setup" in the footer brings
//! it back.

use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use eframe::egui::{self, Color32, RichText};

use crate::portcheck::{self, Input, Report, Verdict};
use crate::settings::Settings;
use crate::upnp;

#[derive(Default)]
struct Check {
    running: bool,
    report: Option<Report>,
}

pub struct FirstRun {
    check: Arc<Mutex<Check>>,
    /// The address the pairing QR code uses; `None`: the first one offered.
    pub qr_host: Option<String>,
}

impl FirstRun {
    pub fn new() -> Self {
        Self {
            check: Arc::default(),
            qr_host: None,
        }
    }

    /// Runs the port test on a thread of its own (it waits on the network).
    pub fn start_check(&self, input: Input, ctx: egui::Context) {
        let check = self.check.clone();
        {
            let mut c = check.lock().unwrap();
            if c.running {
                return;
            }
            c.running = true;
        }
        let spawned = std::thread::Builder::new()
            .name("ws-port-test".into())
            .spawn(move || {
                let report = portcheck::run(&input);
                let mut c = check.lock().unwrap();
                c.running = false;
                c.report = Some(report);
                ctx.request_repaint();
            });
        if spawned.is_err() {
            self.check.lock().unwrap().running = false;
        }
    }

    /// Whether the test is running, and its last report.
    pub fn report(&self) -> (bool, Option<Report>) {
        let c = self.check.lock().unwrap();
        (c.running, c.report.clone())
    }
}

pub fn report_ui(ui: &mut egui::Ui, report: &Report) {
    for line in &report.lines {
        let (mark, color) = match line.verdict {
            Verdict::Ok => ("✔", Color32::GREEN),
            Verdict::Warn => ("!", Color32::YELLOW),
            Verdict::Bad => ("✘", Color32::RED),
            Verdict::Info => ("•", ui.visuals().weak_text_color()),
        };
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(mark).strong().color(color));
            ui.label(&line.text);
        });
    }
}

pub fn upnp_status_ui(ui: &mut egui::Ui, status: &upnp::Status) {
    match status {
        upnp::Status::Off => {}
        upnp::Status::Working => {
            ui.weak("asking the router...");
        }
        upnp::Status::Mapped {
            external_ip,
            lan_ip,
        } => {
            let public = external_ip.map_or("unknown".to_string(), |ip| ip.to_string());
            ui.colored_label(
                Color32::GREEN,
                format!(
                    "The router forwards the ports to {lan_ip}; it says its internet address \
                     is {public}."
                ),
            );
        }
        upnp::Status::NoRouter => {
            ui.colored_label(
                Color32::YELLOW,
                "No router answered. UPnP may be off on the router: forward the ports on its \
                 settings page instead (see the guide).",
            );
        }
        upnp::Status::Failed(e) => {
            ui.colored_label(Color32::RED, format!("UPnP: {e}"));
        }
    }
}

/// Addresses a phone can open the client at, as `https://host[:port]`:
/// the Cloudflare Tunnel first (no certificate warning, and it works away
/// from home), then the LAN addresses, the host names and the public
/// address.
pub fn addresses(
    settings: &Settings,
    port: u16,
    tunnel_url: Option<&str>,
    lan: &[IpAddr],
    public: &[IpAddr],
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut add = |s: String| {
        if !out.contains(&s) {
            out.push(s);
        }
    };
    if let Some(url) = tunnel_url {
        add(url.trim_end_matches('/').to_string());
    }
    let mut names: Vec<String> = lan
        .iter()
        .filter(|ip| !ip.is_loopback())
        .map(|ip| host_part(*ip))
        .collect();
    names.extend(
        settings
            .hostnames
            .split([' ', ',', ';'])
            .map(|n| n.trim().to_string()),
    );
    names.extend(public.iter().map(|ip| host_part(*ip)));
    names.extend(
        settings
            .public_ips
            .split([' ', ',', ';'])
            .map(|n| n.trim().to_string()),
    );
    for name in names.into_iter().filter(|n| !n.is_empty()) {
        add(format!("https://{name}:{port}"));
    }
    out
}

fn host_part(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => format!("[{v6}]"),
    }
}

/// The client's address (`https://host[:port]`) with the pairing link on
/// the end. The secret goes after `#`, which browsers never send to a
/// server.
pub fn pairing_url(origin: &str, link: &str) -> String {
    format!("{origin}/#pair={link}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_in_order_without_repeats() {
        let s = Settings {
            hostnames: "home.example.net, 192.168.1.20".into(),
            public_ips: "203.0.113.5".into(),
            ..Settings::default()
        };
        let lan: Vec<IpAddr> = vec![
            "192.168.1.20".parse().unwrap(),
            "127.0.0.1".parse().unwrap(),
            "fd00::5".parse().unwrap(),
        ];
        let public: Vec<IpAddr> = vec!["203.0.113.5".parse().unwrap()];
        assert_eq!(
            addresses(&s, 8443, Some("https://pc.example.com"), &lan, &public),
            [
                "https://pc.example.com",
                "https://192.168.1.20:8443",
                "https://[fd00::5]:8443",
                "https://home.example.net:8443",
                "https://203.0.113.5:8443"
            ]
        );
        assert_eq!(addresses(&s, 8443, None, &lan, &public).len(), 4);
    }

    #[test]
    fn the_link_is_a_fragment() {
        assert_eq!(
            pairing_url("https://192.168.1.20:8443", "abc_-"),
            "https://192.168.1.20:8443/#pair=abc_-"
        );
        assert_eq!(
            pairing_url("https://pc.example.com", "abc_-"),
            "https://pc.example.com/#pair=abc_-"
        );
    }
}
