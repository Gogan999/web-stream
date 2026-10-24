//! Opt-in UPnP port mapping: asks the router to forward the host's ports
//! to this PC, so a friend doesn't have to find the router's forwarding
//! page. Off unless the user turns it on in the first-run setup. Mappings
//! are leased for an hour and renewed while the tray app runs, so a PC
//! that is switched off, or gets another address, stops being forwarded
//! to within the hour; quitting and uninstalling also remove them. A
//! router that only takes permanent forwards is refused: one left behind
//! would forward to whatever device gets this address next.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::Context;
use igd_next::{AddPortError, Gateway, PortMappingProtocol, SearchOptions};

/// How the mappings are marked on the router, so only ours are removed.
pub const DESCRIPTION: &str = "web-stream host";
const LEASE_SECS: u32 = 3600;
const RENEW: Duration = Duration::from_secs(20 * 60);
const SEARCH: Duration = Duration::from_secs(3);
/// Mapping entries looked through for ours; home routers keep few.
const MAX_ENTRIES: u32 = 64;
/// How long removing them may take at most (each UPnP call can wait on a
/// slow router for half a minute).
pub const REMOVE_DEADLINE: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proto {
    Tcp,
    Udp,
}

impl Proto {
    fn igd(self) -> PortMappingProtocol {
        match self {
            Proto::Tcp => PortMappingProtocol::TCP,
            Proto::Udp => PortMappingProtocol::UDP,
        }
    }
}

pub type Port = (Proto, u16);

/// The ports the host needs forwarded: HTTPS and media on `port`, and the
/// ICE-TCP fallback when it is on.
pub fn ports(port: u16, ice_tcp: Option<u16>) -> Vec<Port> {
    let mut out = vec![(Proto::Tcp, port), (Proto::Udp, port)];
    out.extend(ice_tcp.map(|p| (Proto::Tcp, p)));
    out
}

/// What the mapper is doing, for the window.
#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Off,
    Working,
    Mapped {
        /// As the router reports it (unverified: any device on the network
        /// can answer UPnP).
        external_ip: Option<IpAddr>,
        lan_ip: Ipv4Addr,
    },
    NoRouter,
    Failed(String),
}

/// A router that answered UPnP, and this PC's address towards it.
pub struct Router {
    gateway: Gateway,
    pub lan_ip: Ipv4Addr,
}

/// The router on this network, or `None` if none answers UPnP (it is off on
/// many routers, or not supported).
pub fn find_router() -> anyhow::Result<Option<Router>> {
    let gateway = match igd_next::search_gateway(SearchOptions {
        timeout: Some(SEARCH),
        single_search_timeout: Some(SEARCH),
        ..Default::default()
    }) {
        Ok(g) => g,
        Err(igd_next::SearchError::NoResponseWithinTimeout) => return Ok(None),
        Err(igd_next::SearchError::IoError(e))
            if matches!(
                e.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            ) =>
        {
            return Ok(None);
        }
        Err(e) => return Err(anyhow::anyhow!("searching for the router: {e}")),
    };
    let lan_ip = lan_ip_towards(gateway.addr).context("this PC's address on the network")?;
    Ok(Some(Router { gateway, lan_ip }))
}

/// The local address the system would use to reach `peer` (nothing is sent).
pub fn lan_ip_towards(peer: SocketAddr) -> anyhow::Result<Ipv4Addr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.connect(peer)?;
    match socket.local_addr()?.ip() {
        IpAddr::V4(ip) => Ok(ip),
        IpAddr::V6(ip) => anyhow::bail!("an IPv6 address ({ip})"),
    }
}

impl Router {
    pub fn external_ip(&self) -> Option<IpAddr> {
        self.gateway.get_external_ip().ok()
    }

    /// Maps each port to the same port on this PC, leased.
    pub fn map(&self, ports: &[Port]) -> anyhow::Result<()> {
        for &(proto, port) in ports {
            let local = SocketAddr::new(IpAddr::V4(self.lan_ip), port);
            let r = self
                .gateway
                .add_port(proto.igd(), port, local, LEASE_SECS, DESCRIPTION);
            r.map_err(|e| match e {
                AddPortError::OnlyPermanentLeasesSupported => anyhow::anyhow!(
                    "this router only takes permanent forwards, which would outlive this PC's \
                     address; forward the ports on the router's settings page instead"
                ),
                AddPortError::PortInUse => anyhow::anyhow!(
                    "the router already forwards {proto:?} {port} to another device"
                ),
                AddPortError::ActionNotAuthorized => {
                    anyhow::anyhow!(
                        "the router doesn't let this PC add forwards (UPnP is read-only)"
                    )
                }
                e => anyhow::anyhow!("forwarding {proto:?} {port}: {e}"),
            })?;
        }
        Ok(())
    }

    /// Removes every mapping marked as ours that points at this PC's
    /// address; how many went.
    pub fn unmap_ours(&self) -> usize {
        self.unmap_where(|client| client == Some(self.lan_ip))
    }

    /// Removes every mapping marked as ours, whatever address it points at
    /// (an old one of this PC's, say). For uninstalling; another web-stream
    /// host on the network renews its own within 20 minutes.
    pub fn unmap_all_ours(&self) -> usize {
        self.unmap_where(|_| true)
    }

    fn unmap_where(&self, client: impl Fn(Option<Ipv4Addr>) -> bool) -> usize {
        let mut ours = Vec::new();
        for i in 0..MAX_ENTRIES {
            let Ok(e) = self.gateway.get_generic_port_mapping_entry(i) else {
                break;
            };
            if e.port_mapping_description == DESCRIPTION && client(e.internal_client.parse().ok()) {
                ours.push((e.protocol, e.external_port));
            }
        }
        ours.into_iter()
            .filter(|&(proto, port)| self.gateway.remove_port(proto, port).is_ok())
            .count()
    }
}

enum Msg {
    Want(Option<Vec<Port>>),
    Stop(mpsc::Sender<()>),
}

/// Keeps the wanted ports mapped on a thread of its own (UPnP calls block).
pub struct Mapper {
    tx: mpsc::Sender<Msg>,
    status: Arc<Mutex<Status>>,
}

static MAPPER: OnceLock<Mapper> = OnceLock::new();

/// The app's one mapper; `changed` is called whenever its status changes.
pub fn mapper(changed: impl Fn() + Send + 'static) -> &'static Mapper {
    MAPPER.get_or_init(|| {
        let (tx, rx) = mpsc::channel();
        let status = Arc::new(Mutex::new(Status::Off));
        let shown = status.clone();
        let spawned = std::thread::Builder::new()
            .name("ws-upnp".into())
            .spawn(move || run(rx, shown, changed));
        if let Err(e) = spawned {
            *status.lock().unwrap() = Status::Failed(format!("{e}"));
        }
        Mapper { tx, status }
    })
}

/// Removes the mappings before the app exits (waits up to a few seconds).
pub fn shutdown() {
    if let Some(m) = MAPPER.get() {
        let (ack, done) = mpsc::channel();
        if m.tx.send(Msg::Stop(ack)).is_ok() {
            let _ = done.recv_timeout(Duration::from_secs(5));
        }
    }
}

impl Mapper {
    /// `Some(ports)` maps them (replacing what was mapped); `None` removes
    /// the mappings.
    pub fn want(&self, ports: Option<Vec<Port>>) {
        let _ = self.tx.send(Msg::Want(ports));
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }
}

fn run(rx: mpsc::Receiver<Msg>, status: Arc<Mutex<Status>>, changed: impl Fn()) {
    let set = |s: Status| {
        *status.lock().unwrap() = s;
        changed();
    };
    let mut want: Option<Vec<Port>> = None;
    let mut mapped: Option<(Router, Vec<Port>)> = None;
    loop {
        let msg = if want.is_some() {
            rx.recv_timeout(RENEW)
        } else {
            rx.recv().map_err(|_| RecvTimeoutError::Disconnected)
        };
        match msg {
            Ok(Msg::Want(w)) => {
                if w == want && mapped.is_some() {
                    continue;
                }
                want = w;
            }
            // Time to renew the lease.
            Err(RecvTimeoutError::Timeout) => {}
            Ok(Msg::Stop(ack)) => {
                if let Some((router, _)) = mapped.take() {
                    router.unmap_ours();
                }
                let _ = ack.send(());
                return;
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
        let Some(ports) = &want else {
            if let Some((router, _)) = mapped.take() {
                let n = router.unmap_ours();
                tracing::info!(n, "UPnP mappings removed");
            }
            set(Status::Off);
            continue;
        };
        set(Status::Working);
        let router = match find_router() {
            Ok(Some(r)) => r,
            Ok(None) => {
                set(Status::NoRouter);
                continue;
            }
            Err(e) => {
                set(Status::Failed(format!("{e:#}")));
                continue;
            }
        };
        // Different ports, or a different PC address: drop the old ones,
        // through what they were mapped with (the old address).
        if let Some((old, old_ports)) = &mapped {
            if old_ports != ports || old.lan_ip != router.lan_ip {
                old.unmap_ours();
            }
        }
        match router.map(ports) {
            Ok(()) => {
                tracing::info!(?ports, lan = %router.lan_ip, "UPnP ports mapped");
                set(Status::Mapped {
                    external_ip: router.external_ip(),
                    lan_ip: router.lan_ip,
                });
                mapped = Some((router, ports.clone()));
            }
            Err(e) => {
                tracing::warn!("UPnP: {e:#}");
                set(Status::Failed(format!("{e:#}")));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_ports() {
        assert_eq!(
            ports(8443, Some(8444)),
            vec![(Proto::Tcp, 8443), (Proto::Udp, 8443), (Proto::Tcp, 8444)]
        );
        assert_eq!(ports(9000, None).len(), 2);
    }

    #[test]
    fn lan_address_towards_a_peer() {
        // Loopback always routes; nothing is sent.
        assert_eq!(
            lan_ip_towards("127.0.0.1:1900".parse().unwrap()).unwrap(),
            Ipv4Addr::LOCALHOST
        );
    }
}
