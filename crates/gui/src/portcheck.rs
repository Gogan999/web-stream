//! The first-run port test: can a phone away from home reach this host?
//!
//! With a Cloudflare Tunnel the web page comes through Cloudflare, and the
//! test reports whether the tunnel is up; only the media ports need
//! forwarding then.
//!
//! Nothing outside the network is asked. The test learns the public
//! address from the router (UPnP) or the address typed in the settings,
//! and connects to it from this PC. Most routers pass that back in
//! ("hairpin"), and the certificate that answers tells this host apart from
//! the router's own admin page on the same port. When the router doesn't
//! loop back, the test says so and the phone on mobile data is the test.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::time::Duration;

use crate::upnp;

const CONNECT: Duration = Duration::from_secs(3);

/// What the test needs from the settings and the running host.
#[derive(Debug, Clone)]
pub struct Input {
    pub port: u16,
    pub ice_tcp_port: Option<u16>,
    /// The running host's certificate, `None` when it isn't hosting.
    pub fingerprint: Option<String>,
    /// Whether a Cloudflare Tunnel carries the web page, and where it is up
    /// (`None` while it isn't).
    pub tunnel: Option<Option<String>>,
    /// Public addresses typed in the settings.
    pub public_ips: Vec<IpAddr>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Ok,
    Warn,
    Bad,
    /// Information; nothing to fix.
    Info,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub verdict: Verdict,
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub lines: Vec<Line>,
    /// The public address this host was found at (its own certificate
    /// answered there), for the QR code a phone away from home opens. Only
    /// a verified one: the router's word alone isn't enough, since any
    /// device on the network can answer UPnP.
    pub verified_ip: Option<IpAddr>,
}

impl Report {
    fn say(&mut self, verdict: Verdict, text: impl Into<String>) {
        self.lines.push(Line {
            verdict,
            text: text.into(),
        });
    }
}

/// Runs every check; blocks for up to about 15 seconds.
pub fn run(input: &Input) -> Report {
    let mut r = Report::default();
    let Some(fingerprint) = &input.fingerprint else {
        r.say(
            Verdict::Bad,
            "The host isn't running: press Start, then test again.",
        );
        return r;
    };

    // The router.
    let router = match upnp::find_router() {
        Ok(router) => router,
        Err(e) => {
            r.say(Verdict::Info, format!("Couldn't ask the router: {e:#}"));
            None
        }
    };
    let router_ip = router.as_ref().and_then(|g| g.external_ip());
    match (&router, router_ip) {
        (Some(g), Some(ip)) => r.say(
            Verdict::Info,
            format!(
                "The router answers UPnP; its internet address is {ip}; this PC is {}.",
                g.lan_ip
            ),
        ),
        (Some(g), None) => r.say(
            Verdict::Info,
            format!(
                "The router answers UPnP but didn't give its internet address; this PC is {}.",
                g.lan_ip
            ),
        ),
        (None, _) => r.say(
            Verdict::Info,
            "The router doesn't answer UPnP, so the ports have to be forwarded on its own \
             settings page (see the guide).",
        ),
    }

    // The web page.
    match &input.tunnel {
        Some(Some(url)) => r.say(
            Verdict::Ok,
            format!("The web page is up through Cloudflare at {url}/ (no port to forward for it)."),
        ),
        Some(None) => r.say(
            Verdict::Bad,
            "The Cloudflare Tunnel isn't up: check the token in Settings, and the log below.",
        ),
        None => {}
    }

    // Where the internet sees this network.
    let public = router_ip
        .filter(|ip| is_public(*ip))
        .or_else(|| input.public_ips.first().copied());
    if let Some(ip) = router_ip.filter(|ip| !is_public(*ip)) {
        r.say(
            Verdict::Bad,
            format!(
                "The router's internet address {ip} is not a public one: your internet provider \
                 shares one address between customers (CGNAT), or there is a second router in \
                 front of this one. Forwarding ports can't reach this PC from outside then. Use \
                 it at home, ask the provider for a public IPv4 address, or use Tailscale (see \
                 the guide)."
            ),
        );
        if public.is_none() {
            return r;
        }
    }
    let Some(public) = public else {
        r.say(
            Verdict::Warn,
            "This PC's public address isn't known (no UPnP, none in Settings), so the ports \
             can't be tested from here. Test from your phone instead, with Wi-Fi off.",
        );
        return r;
    };

    // The ports, through the public address. Through a tunnel nothing
    // answers on the HTTPS port from outside, and needn't.
    let https = SocketAddr::new(public, input.port);
    let check_https = input.tunnel.is_none();
    match check_https.then(|| certificate_at(https)) {
        None => {}
        Some(Ok(f)) if &f == fingerprint => {
            r.verified_ip = Some(public);
            r.say(
                Verdict::Ok,
                format!("TCP {}: this host answers at {https}.", input.port),
            );
        }
        Some(Ok(_)) => r.say(
            Verdict::Bad,
            format!(
                "TCP {}: something else answers at {https} (often the router's own remote \
                 admin page). Turn that off on the router, or change the port in Settings.",
                input.port
            ),
        ),
        Some(Err(_)) => r.say(
            Verdict::Warn,
            format!(
                "TCP {}: no answer at {https} from inside your network. Either the port isn't \
                 forwarded, or the router can't loop back to itself (many can't). On your \
                 phone, Wi-Fi off, open https://{https}/: if the page opens, it works. To pair \
                 the phone at that address, add {public} under Settings > Public addresses and \
                 the QR code can use it.",
                input.port
            ),
        ),
    }
    if let Some(tcp) = input.ice_tcp_port {
        let addr = SocketAddr::new(public, tcp);
        if TcpStream::connect_timeout(&addr, CONNECT).is_ok() {
            r.say(
                Verdict::Ok,
                format!("TCP {tcp} (fallback for networks that block UDP): open."),
            );
        } else {
            r.say(
                Verdict::Warn,
                format!(
                    "TCP {tcp} (fallback for networks that block UDP): no answer from inside. \
                     Forward it too, if it isn't."
                ),
            );
        }
    }
    r.say(
        Verdict::Info,
        format!(
            "UDP {} carries the video and can't be tested from here. If your phone connects \
             away from home but the stats say TCP, forward UDP {} as well.",
            input.port, input.port
        ),
    );
    r
}

fn certificate_at(addr: SocketAddr) -> anyhow::Result<String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    Ok(rt.block_on(ws_signaling::certificate_at(addr, CONNECT))?)
}

/// Reachable from the internet: not private, shared (CGNAT), loopback or
/// link-local.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let cgnat = v4.octets()[0] == 100 && (v4.octets()[1] & 0xC0) == 64;
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || cgnat
                || v4 == Ipv4Addr::new(192, 0, 0, 0))
        }
        IpAddr::V6(v6) => !(v6.is_loopback() || v6.is_unspecified()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_and_private_addresses_are_not_public() {
        for ip in [
            "10.1.2.3",
            "192.168.0.1",
            "172.16.5.5",
            "100.64.0.1",
            "100.127.255.254",
            "127.0.0.1",
            "169.254.1.1",
        ] {
            assert!(!is_public(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["203.0.113.5", "100.63.0.1", "100.128.0.1", "8.8.8.8"] {
            assert!(is_public(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn not_running_says_so() {
        let r = run(&Input {
            port: 8443,
            ice_tcp_port: Some(8444),
            fingerprint: None,
            tunnel: None,
            public_ips: vec![],
        });
        assert_eq!(r.lines.len(), 1);
        assert_eq!(r.lines[0].verdict, Verdict::Bad);
    }

    #[test]
    fn finds_this_host_at_a_typed_address() {
        // A host on loopback stands in for the router's forward.
        let dir = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let config = ws_signaling::SignalingConfig::new(addr, dir.path().to_path_buf());
        let (server, _events) = ws_signaling::SignalingServer::new(config).unwrap();
        let fingerprint = server.certificate().fingerprint;
        rt.spawn(server.serve());
        std::thread::sleep(Duration::from_millis(200));

        // 127.0.0.1 isn't public, but a typed address is taken as given.
        let input = Input {
            port: addr.port(),
            ice_tcp_port: None,
            fingerprint: Some(fingerprint),
            tunnel: None,
            public_ips: vec![addr.ip()],
        };
        let r = run(&input);
        assert!(
            r.lines.iter().any(|l| l.verdict == Verdict::Ok),
            "{:#?}",
            r.lines
        );
        let other = run(&Input {
            fingerprint: Some("AA:BB".into()),
            ..input
        });
        assert!(other.lines.iter().any(|l| l.verdict == Verdict::Bad));
    }
}
