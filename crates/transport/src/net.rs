//! Sockets: one UDP socket and one ICE-TCP listener per local address. Every
//! received packet, from either, is funneled into one channel for the run
//! loop.
//!
//! The ports are open to the internet, so the readers filter before
//! anything reaches the run loop: a source is admitted only by a STUN
//! Binding request naming the current session's ICE ufrag, and after that
//! is rate capped. Everything else is dropped at the socket.

use std::collections::{HashMap, HashSet};
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{Semaphore, mpsc, watch};

use str0m::net::Protocol;

/// Most ICE-TCP connections a browser will reasonably open at once.
const MAX_TCP_CONNS: usize = 8;
/// ICE-TCP connections from one IP, so a single host cannot hold every
/// permit while it stalls through the first-check deadline.
const MAX_TCP_PER_IP: usize = 2;
/// RFC 4571 frames are length-prefixed with a u16.
const MAX_FRAME: usize = u16::MAX as usize;
/// An ICE-TCP connection must open with a STUN check this soon.
const TCP_FIRST_STUN: Duration = Duration::from_secs(2);
/// Browsers send consent checks every few seconds; silence this long means
/// the peer is gone.
const TCP_IDLE: Duration = Duration::from_secs(30);
/// Queued outgoing frames per ICE-TCP connection. Kept shallow, with a small
/// kernel buffer, so a backed-up TCP path drops instead of adding latency.
const TCP_WRITE_QUEUE: usize = 16;
const TCP_SNDBUF: usize = 128 * 1024;
/// Peers admitted at once (a browser uses a few candidate pairs).
const MAX_PEERS: usize = 16;
/// Per-source packet budget. Far above what a browser sends (feedback,
/// input, mic audio), low enough that a flood can't swamp the run loop.
const PEER_PPS: f64 = 5_000.0;
const PEER_BURST: f64 = 1_000.0;
/// STUN checks from sources not yet admitted, per reader.
const UNKNOWN_STUN_PPS: f64 = 100.0;
const UNKNOWN_STUN_BURST: f64 = 50.0;

pub struct Packet {
    pub proto: Protocol,
    pub source: SocketAddr,
    pub destination: SocketAddr,
    pub data: Vec<u8>,
}

/// Which packets the readers let through, set by the run loop for each new
/// session.
pub struct Filter {
    ufrag: Option<Vec<u8>>,
    peers: HashSet<SocketAddr>,
    /// Bumped on every reset; ICE-TCP connections of an older session close.
    epoch: watch::Sender<u64>,
}

impl Default for Filter {
    fn default() -> Self {
        Self {
            ufrag: None,
            peers: HashSet::new(),
            epoch: watch::Sender::new(0),
        }
    }
}

pub type SharedFilter = Arc<Mutex<Filter>>;

impl Filter {
    /// A new session: admit checks for `ufrag` (or nothing, for `None`) and
    /// forget earlier peers.
    pub fn reset(&mut self, ufrag: Option<&str>) {
        self.ufrag = ufrag.map(|u| u.as_bytes().to_vec());
        self.peers.clear();
        self.epoch.send_modify(|e| *e += 1);
    }

    fn is_valid_check(&self, data: &[u8]) -> bool {
        self.ufrag
            .as_deref()
            .is_some_and(|u| binding_request_for(data, u))
    }
}

/// True if `data` is a STUN Binding request whose USERNAME is
/// `<ufrag>:<remote ufrag>`. The integrity check is left to str0m; this is
/// only a cheap gate.
fn binding_request_for(data: &[u8], ufrag: &[u8]) -> bool {
    const HEADER: usize = 20;
    const MAGIC: [u8; 4] = [0x21, 0x12, 0xA4, 0x42];
    const USERNAME: u16 = 0x0006;
    if data.len() < HEADER || data[0..2] != [0x00, 0x01] || data[4..8] != MAGIC {
        return false;
    }
    let len = u16::from_be_bytes([data[2], data[3]]) as usize;
    let Some(mut attrs) = data.get(HEADER..HEADER + len) else {
        return false;
    };
    while attrs.len() >= 4 {
        let kind = u16::from_be_bytes([attrs[0], attrs[1]]);
        let n = u16::from_be_bytes([attrs[2], attrs[3]]) as usize;
        let Some(value) = attrs.get(4..4 + n) else {
            return false;
        };
        if kind == USERNAME {
            return value.len() > ufrag.len()
                && value.starts_with(ufrag)
                && value[ufrag.len()] == b':';
        }
        let padded = (4 + n + 3) & !3;
        attrs = attrs.get(padded..).unwrap_or(&[]);
    }
    false
}

struct Bucket {
    tokens: f64,
    at: Instant,
}

impl Bucket {
    fn new(burst: f64, now: Instant) -> Self {
        Self {
            tokens: burst,
            at: now,
        }
    }

    fn take(&mut self, rate: f64, burst: f64, now: Instant) -> bool {
        let dt = now.duration_since(self.at).as_secs_f64();
        self.at = now;
        self.tokens = (self.tokens + dt * rate).min(burst);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

type TcpWriters = Arc<Mutex<HashMap<(SocketAddr, SocketAddr), mpsc::Sender<Vec<u8>>>>>;

pub struct Net {
    udp: Vec<(SocketAddr, Arc<UdpSocket>)>,
    tcp_writers: TcpWriters,
    pub udp_addrs: Vec<SocketAddr>,
    pub tcp_addrs: Vec<SocketAddr>,
}

impl Net {
    pub async fn bind(
        ips: &[IpAddr],
        udp_port: u16,
        tcp_port: Option<u16>,
        filter: SharedFilter,
        incoming: mpsc::Sender<Packet>,
    ) -> io::Result<Net> {
        let tcp_writers: TcpWriters = Arc::default();
        let tcp_permits = Arc::new(Semaphore::new(MAX_TCP_CONNS));
        let mut net = Net {
            udp: Vec::new(),
            tcp_writers: tcp_writers.clone(),
            udp_addrs: Vec::new(),
            tcp_addrs: Vec::new(),
        };
        for ip in ips {
            let socket = Arc::new(UdpSocket::bind(SocketAddr::new(*ip, udp_port)).await?);
            let local = socket.local_addr()?;
            net.udp_addrs.push(local);
            net.udp.push((local, socket.clone()));
            tokio::spawn(udp_reader(socket, local, filter.clone(), incoming.clone()));

            if let Some(port) = tcp_port {
                let listener = TcpListener::bind(SocketAddr::new(*ip, port)).await?;
                net.tcp_addrs.push(listener.local_addr()?);
                tokio::spawn(tcp_acceptor(
                    listener,
                    tcp_permits.clone(),
                    tcp_writers.clone(),
                    filter.clone(),
                    incoming.clone(),
                ));
            }
        }
        Ok(net)
    }

    /// Send without waiting. A full socket buffer drops the packet, which is
    /// what we want for media: never queue behind old data.
    pub fn send(&self, proto: Protocol, source: SocketAddr, destination: SocketAddr, data: &[u8]) {
        match proto {
            Protocol::Udp => {
                let Some((_, sock)) = self.udp.iter().find(|(a, _)| *a == source) else {
                    tracing::debug!(%source, "no UDP socket for source");
                    return;
                };
                if let Err(e) = sock.try_send_to(data, destination) {
                    if e.kind() != io::ErrorKind::WouldBlock {
                        tracing::debug!(%destination, "udp send: {e}");
                    }
                }
            }
            Protocol::Tcp => {
                let writers = self.tcp_writers.lock().expect("tcp writers lock");
                if let Some(tx) = writers.get(&(source, destination)) {
                    let _ = tx.try_send(data.to_vec());
                }
            }
            other => tracing::debug!(?other, "unsupported transmit protocol"),
        }
    }
}

async fn udp_reader(
    socket: Arc<UdpSocket>,
    local: SocketAddr,
    filter: SharedFilter,
    incoming: mpsc::Sender<Packet>,
) {
    let mut buf = vec![0u8; 2048];
    let mut unknown = Bucket::new(UNKNOWN_STUN_BURST, Instant::now());
    let mut peers: HashMap<SocketAddr, Bucket> = HashMap::new();
    loop {
        let (n, source) = match socket.recv_from(&mut buf).await {
            Ok(r) => r,
            // Windows reports ICMP port unreachable from an earlier send as
            // a receive error; it is harmless.
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => continue,
            Err(e) => {
                tracing::warn!(%local, "udp receive failed: {e}");
                return;
            }
        };
        let data = &buf[..n];
        let now = Instant::now();
        let admitted = {
            let mut f = filter.lock().expect("filter lock");
            if f.peers.contains(&source) {
                true
            } else if f.peers.len() < MAX_PEERS
                // Budget only checks that pass, so junk cannot use it up.
                && f.is_valid_check(data)
                && unknown.take(UNKNOWN_STUN_PPS, UNKNOWN_STUN_BURST, now)
            {
                f.peers.insert(source);
                true
            } else {
                false
            }
        };
        if !admitted {
            continue;
        }
        if peers.len() > MAX_PEERS {
            peers.retain(|a, _| filter.lock().expect("filter lock").peers.contains(a));
        }
        let bucket = peers
            .entry(source)
            .or_insert_with(|| Bucket::new(PEER_BURST, now));
        if !bucket.take(PEER_PPS, PEER_BURST, now) {
            continue;
        }
        let pkt = Packet {
            proto: Protocol::Udp,
            source,
            destination: local,
            data: data.to_vec(),
        };
        // Never wait on the run loop: if it is behind, drop.
        match incoming.try_send(pkt) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => {}
            Err(mpsc::error::TrySendError::Closed(_)) => return,
        }
    }
}

async fn tcp_acceptor(
    listener: TcpListener,
    permits: Arc<Semaphore>,
    writers: TcpWriters,
    filter: SharedFilter,
    incoming: mpsc::Sender<Packet>,
) {
    let per_ip: PerIp = Arc::default();
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!("tcp accept: {e}");
                continue;
            }
        };
        // Taken before spawning, so the cap holds however fast connections
        // arrive; released when the connection task ends.
        let Some(ip_slot) = IpSlot::take(&per_ip, peer.ip()) else {
            tracing::debug!(%peer, "too many ICE-TCP connections from this address");
            continue;
        };
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            tracing::debug!(%peer, "too many ICE-TCP connections");
            continue;
        };
        let Ok(local) = stream.local_addr() else {
            continue;
        };
        let _ = stream.set_nodelay(true);
        let _ = socket2::SockRef::from(&stream).set_send_buffer_size(TCP_SNDBUF);
        let (writers, filter, incoming) = (writers.clone(), filter.clone(), incoming.clone());
        tokio::spawn(async move {
            tcp_conn(stream, local, peer, writers, filter, incoming).await;
            drop((permit, ip_slot));
        });
    }
}

type PerIp = Arc<Mutex<HashMap<IpAddr, usize>>>;

/// One of an IP's ICE-TCP connection slots, given back on drop.
struct IpSlot(PerIp, IpAddr);

impl IpSlot {
    fn take(per_ip: &PerIp, ip: IpAddr) -> Option<Self> {
        let mut map = per_ip.lock().expect("per-ip lock");
        let n = map.entry(ip).or_default();
        if *n >= MAX_TCP_PER_IP {
            return None;
        }
        *n += 1;
        Some(Self(per_ip.clone(), ip))
    }
}

impl Drop for IpSlot {
    fn drop(&mut self) {
        let mut map = self.0.lock().expect("per-ip lock");
        if let Some(n) = map.get_mut(&self.1) {
            *n -= 1;
            if *n == 0 {
                map.remove(&self.1);
            }
        }
    }
}

async fn read_frame<'a>(
    rd: &mut (impl AsyncReadExt + Unpin),
    buf: &'a mut [u8],
) -> io::Result<&'a [u8]> {
    let mut len = [0u8; 2];
    rd.read_exact(&mut len).await?;
    let n = u16::from_be_bytes(len) as usize;
    rd.read_exact(&mut buf[..n]).await?;
    Ok(&buf[..n])
}

async fn tcp_conn(
    stream: TcpStream,
    local: SocketAddr,
    peer: SocketAddr,
    writers: TcpWriters,
    filter: SharedFilter,
    incoming: mpsc::Sender<Packet>,
) {
    let (mut rd, mut wr) = stream.into_split();
    let mut buf = vec![0u8; MAX_FRAME];
    let mut epoch = filter.lock().expect("filter lock").epoch.subscribe();

    // The first frame must be a check for the current session.
    let first = match tokio::time::timeout(TCP_FIRST_STUN, read_frame(&mut rd, &mut buf)).await {
        Ok(Ok(f)) if filter.lock().expect("filter lock").is_valid_check(f) => f.to_vec(),
        _ => {
            tracing::debug!(%peer, "ICE-TCP connection without a valid first check");
            return;
        }
    };

    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(TCP_WRITE_QUEUE);
    writers
        .lock()
        .expect("tcp writers lock")
        .insert((local, peer), tx);

    let writer = tokio::spawn(async move {
        while let Some(data) = rx.recv().await {
            if data.len() > MAX_FRAME {
                continue;
            }
            let mut frame = Vec::with_capacity(2 + data.len());
            frame.extend_from_slice(&(data.len() as u16).to_be_bytes());
            frame.extend_from_slice(&data);
            if wr.write_all(&frame).await.is_err() {
                return;
            }
        }
    });

    let mut bucket = Bucket::new(PEER_BURST, Instant::now());
    let mut next = Some(first);
    loop {
        let data = match next.take() {
            Some(d) => d,
            None => tokio::select! {
                read = tokio::time::timeout(TCP_IDLE, read_frame(&mut rd, &mut buf)) => match read {
                    Ok(Ok(f)) => f.to_vec(),
                    _ => break,
                },
                // A new session (or none): this connection belonged to the old one.
                _ = epoch.changed() => break,
            },
        };
        if !bucket.take(PEER_PPS, PEER_BURST, Instant::now()) {
            continue;
        }
        let pkt = Packet {
            proto: Protocol::Tcp,
            source: peer,
            destination: local,
            data,
        };
        match incoming.try_send(pkt) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => {}
            Err(mpsc::error::TrySendError::Closed(_)) => break,
        }
    }
    writers
        .lock()
        .expect("tcp writers lock")
        .remove(&(local, peer));
    writer.abort();
}

/// Private and other non-loopback IPv4 addresses of this machine's
/// interfaces, for when none are configured.
pub fn lan_ips() -> Vec<IpAddr> {
    let mut ips: Vec<IpAddr> = local_ip_address::list_afinet_netifas()
        .unwrap_or_default()
        .into_iter()
        .map(|(_, ip)| ip)
        .filter(|ip| match ip {
            IpAddr::V4(v4) => !v4.is_loopback() && !v4.is_link_local() && !v4.is_unspecified(),
            IpAddr::V6(_) => false,
        })
        .collect();
    ips.sort();
    ips.dedup();
    ips
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(username: &[u8]) -> Vec<u8> {
        let mut attrs = Vec::new();
        // An unrelated attribute first, to exercise padding.
        attrs.extend_from_slice(&[0x80, 0x29, 0x00, 0x03, 1, 2, 3, 0]);
        attrs.extend_from_slice(&0x0006u16.to_be_bytes());
        attrs.extend_from_slice(&(username.len() as u16).to_be_bytes());
        attrs.extend_from_slice(username);
        while attrs.len() % 4 != 0 {
            attrs.push(0);
        }
        let mut m = vec![0x00, 0x01];
        m.extend_from_slice(&(attrs.len() as u16).to_be_bytes());
        m.extend_from_slice(&[0x21, 0x12, 0xA4, 0x42]);
        m.extend_from_slice(&[7; 12]);
        m.extend_from_slice(&attrs);
        m
    }

    #[test]
    fn admits_only_checks_for_our_ufrag() {
        assert!(binding_request_for(&binding(b"abcd:wxyz"), b"abcd"));
        assert!(!binding_request_for(&binding(b"abcde:wxyz"), b"abcd"));
        assert!(!binding_request_for(&binding(b"zzzz:abcd"), b"abcd"));
        assert!(!binding_request_for(&binding(b"abcd"), b"abcd"));
        let mut response = binding(b"abcd:wxyz");
        response[0..2].copy_from_slice(&[0x01, 0x01]);
        assert!(!binding_request_for(&response, b"abcd"));
        let truncated = binding(b"abcd:wxyz");
        assert!(!binding_request_for(
            &truncated[..truncated.len() - 4],
            b"abcd"
        ));
        assert!(!binding_request_for(&[0x80; 200], b"abcd"));
    }

    #[test]
    fn per_ip_slots() {
        let per_ip = PerIp::default();
        let ip: IpAddr = [10, 0, 0, 1].into();
        let a = IpSlot::take(&per_ip, ip).unwrap();
        let _b = IpSlot::take(&per_ip, ip).unwrap();
        assert!(IpSlot::take(&per_ip, ip).is_none());
        assert!(IpSlot::take(&per_ip, [10, 0, 0, 2].into()).is_some());
        drop(a);
        assert!(IpSlot::take(&per_ip, ip).is_some());
    }

    #[test]
    fn bucket_caps_rate() {
        let now = Instant::now();
        let mut b = Bucket::new(10.0, now);
        assert_eq!((0..20).filter(|_| b.take(100.0, 10.0, now)).count(), 10);
        let later = now + Duration::from_millis(100);
        assert_eq!((0..20).filter(|_| b.take(100.0, 10.0, later)).count(), 10);
    }
}
