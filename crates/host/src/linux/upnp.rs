//! Opt-in UPnP port mapping from the network side: asks the router to forward the host's ports.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use igd_next::{PortMappingProtocol, SearchOptions};

/// How the mappings are marked on the router.
const DESCRIPTION: &str = "web-stream host";
const LEASE_SECS: u32 = 3600;
const RENEW: Duration = Duration::from_secs(20 * 60);
const RETRY: Duration = Duration::from_secs(60);
const SEARCH: Duration = Duration::from_secs(3);

/// Keeps `ports` (TCP when true, and the port) mapped to this PC on a thread of its own, for as long
/// as the process runs; the returned text says how it goes.
pub fn start(ports: Vec<(bool, u16)>) -> Arc<Mutex<String>> {
    let status = Arc::new(Mutex::new("looking for the router".to_string()));
    let shown = status.clone();
    let spawned = std::thread::Builder::new()
        .name("ws-upnp".into())
        .spawn(move || {
            loop {
                let (text, wait) = match map(&ports) {
                    Ok(text) => (text, RENEW),
                    Err(e) => (format!("{e:#}"), RETRY),
                };
                tracing::info!("UPnP: {text}");
                *shown.lock().unwrap() = text;
                std::thread::sleep(wait);
            }
        });
    if let Err(e) = spawned {
        *status.lock().unwrap() = format!("{e}");
    }
    status
}

fn map(ports: &[(bool, u16)]) -> anyhow::Result<String> {
    let gateway = match igd_next::search_gateway(SearchOptions {
        timeout: Some(SEARCH),
        single_search_timeout: Some(SEARCH),
        ..Default::default()
    }) {
        Ok(g) => g,
        Err(igd_next::SearchError::NoResponseWithinTimeout) => {
            anyhow::bail!("no router answered UPnP")
        }
        Err(e) => anyhow::bail!("searching for the router: {e}"),
    };
    let lan_ip = lan_ip_towards(gateway.addr)?;
    for &(tcp, port) in ports {
        let proto = if tcp {
            PortMappingProtocol::TCP
        } else {
            PortMappingProtocol::UDP
        };
        let local = SocketAddr::new(IpAddr::V4(lan_ip), port);
        gateway
            .add_port(proto, port, local, LEASE_SECS, DESCRIPTION)
            .map_err(|e| anyhow::anyhow!("forwarding {proto:?} {port}: {e}"))?;
    }
    let external = gateway
        .get_external_ip()
        .map_or_else(|_| "unknown".to_string(), |ip| ip.to_string());
    Ok(format!(
        "{} ports forwarded to {lan_ip}; public address {external}",
        ports.len()
    ))
}

/// The local address the system would use to reach `peer` (nothing is sent).
fn lan_ip_towards(peer: SocketAddr) -> anyhow::Result<Ipv4Addr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.connect(peer)?;
    match socket.local_addr()?.ip() {
        IpAddr::V4(ip) => Ok(ip),
        IpAddr::V6(ip) => anyhow::bail!("an IPv6 address ({ip})"),
    }
}
