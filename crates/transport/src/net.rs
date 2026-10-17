//! Sockets: one UDP socket and one ICE-TCP listener per local address. Every
//! received packet, from either, is funneled into one channel for the run
//! loop.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::mpsc;

use str0m::net::Protocol;

/// Most ICE-TCP connections a browser will reasonably open at once.
const MAX_TCP_CONNS: usize = 8;
/// RFC 4571 frames are length-prefixed with a u16.
const MAX_FRAME: usize = u16::MAX as usize;

pub struct Packet {
    pub proto: Protocol,
    pub source: SocketAddr,
    pub destination: SocketAddr,
    pub data: Vec<u8>,
}

type TcpWriters = Arc<Mutex<HashMap<(SocketAddr, SocketAddr), mpsc::Sender<Vec<u8>>>>>;

pub struct Net {
    udp: Vec<Arc<UdpSocket>>,
    tcp_writers: TcpWriters,
    pub udp_addrs: Vec<SocketAddr>,
    pub tcp_addrs: Vec<SocketAddr>,
}

impl Net {
    pub async fn bind(
        ips: &[IpAddr],
        udp_port: u16,
        tcp_port: Option<u16>,
        incoming: mpsc::Sender<Packet>,
    ) -> io::Result<Net> {
        let tcp_writers: TcpWriters = Arc::default();
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
            net.udp.push(socket.clone());
            tokio::spawn(udp_reader(socket, local, incoming.clone()));

            if let Some(port) = tcp_port {
                let listener = TcpListener::bind(SocketAddr::new(*ip, port)).await?;
                net.tcp_addrs.push(listener.local_addr()?);
                tokio::spawn(tcp_acceptor(
                    listener,
                    tcp_writers.clone(),
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
                let Some(sock) = self
                    .udp
                    .iter()
                    .find(|s| s.local_addr().ok() == Some(source))
                else {
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

async fn udp_reader(socket: Arc<UdpSocket>, local: SocketAddr, incoming: mpsc::Sender<Packet>) {
    let mut buf = vec![0u8; 2048];
    loop {
        match socket.recv_from(&mut buf).await {
            Ok((n, source)) => {
                let pkt = Packet {
                    proto: Protocol::Udp,
                    source,
                    destination: local,
                    data: buf[..n].to_vec(),
                };
                if incoming.send(pkt).await.is_err() {
                    return;
                }
            }
            // Windows reports ICMP port unreachable from an earlier send as
            // a receive error; it is harmless.
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
            Err(e) => {
                tracing::warn!(%local, "udp receive failed: {e}");
                return;
            }
        }
    }
}

async fn tcp_acceptor(listener: TcpListener, writers: TcpWriters, incoming: mpsc::Sender<Packet>) {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!("tcp accept: {e}");
                continue;
            }
        };
        let Ok(local) = stream.local_addr() else {
            continue;
        };
        if writers.lock().expect("tcp writers lock").len() >= MAX_TCP_CONNS {
            tracing::debug!(%peer, "too many ICE-TCP connections");
            continue;
        }
        let _ = stream.set_nodelay(true);
        tokio::spawn(tcp_conn(
            stream,
            local,
            peer,
            writers.clone(),
            incoming.clone(),
        ));
    }
}

async fn tcp_conn(
    stream: TcpStream,
    local: SocketAddr,
    peer: SocketAddr,
    writers: TcpWriters,
    incoming: mpsc::Sender<Packet>,
) {
    let (mut rd, mut wr) = stream.into_split();
    // Small queue: if the TCP path backs up, drop rather than build latency.
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
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

    let mut len = [0u8; 2];
    let mut buf = vec![0u8; MAX_FRAME];
    loop {
        if rd.read_exact(&mut len).await.is_err() {
            break;
        }
        let n = u16::from_be_bytes(len) as usize;
        if rd.read_exact(&mut buf[..n]).await.is_err() {
            break;
        }
        let pkt = Packet {
            proto: Protocol::Tcp,
            source: peer,
            destination: local,
            data: buf[..n].to_vec(),
        };
        if incoming.send(pkt).await.is_err() {
            break;
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
