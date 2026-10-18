//! TLS accept loop with the limits a host on the open internet needs:
//! connection caps (global and per IP), a TLS handshake deadline, a
//! request-header deadline that also closes idle keep-alive connections, and
//! HTTP/1.1 only.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio_rustls::TlsAcceptor;

use crate::Error;

pub const MAX_CONNECTIONS: usize = 64;
pub const MAX_CONNECTIONS_PER_IP: usize = 8;
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Applies to every request on a connection, so an idle keep-alive
/// connection is closed once it passes this long without a new request.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn serve(
    addr: SocketAddr,
    tls: Arc<rustls::ServerConfig>,
    app: Router,
) -> Result<(), Error> {
    let listener = TcpListener::bind(addr).await?;
    serve_listener(listener, tls, app).await
}

pub async fn serve_listener(
    listener: TcpListener,
    tls: Arc<rustls::ServerConfig>,
    app: Router,
) -> Result<(), Error> {
    let acceptor = TlsAcceptor::from(tls);
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let per_ip: Arc<Mutex<HashMap<IpAddr, usize>>> = Arc::default();
    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(c) => c,
            Err(e) => {
                // Usually fd exhaustion; back off instead of spinning.
                tracing::warn!("accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let Ok(slot) = slots.clone().try_acquire_owned() else {
            tracing::debug!(%peer, "connection limit reached");
            continue;
        };
        let Some(ip_guard) = IpGuard::acquire(&per_ip, peer.ip()) else {
            tracing::debug!(%peer, "per-IP connection limit reached");
            continue;
        };
        let _ = tcp.set_nodelay(true);
        let acceptor = acceptor.clone();
        let service = TowerToHyperService::new(app.clone());
        tokio::spawn(async move {
            let _slot = slot;
            let _ip = ip_guard;
            let tls = match tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, acceptor.accept(tcp)).await
            {
                Ok(Ok(s)) => s,
                _ => return,
            };
            let conn = http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(HEADER_READ_TIMEOUT)
                .max_buf_size(64 * 1024)
                .serve_connection(TokioIo::new(tls), service)
                .with_upgrades();
            if let Err(e) = conn.await {
                tracing::debug!(%peer, "connection: {e}");
            }
        });
    }
}

/// Counts a connection against its source IP until dropped.
struct IpGuard {
    map: Arc<Mutex<HashMap<IpAddr, usize>>>,
    ip: IpAddr,
}

impl IpGuard {
    fn acquire(map: &Arc<Mutex<HashMap<IpAddr, usize>>>, ip: IpAddr) -> Option<IpGuard> {
        let mut m = map.lock().expect("per-ip lock");
        let n = m.entry(ip).or_insert(0);
        if *n >= MAX_CONNECTIONS_PER_IP {
            return None;
        }
        *n += 1;
        Some(IpGuard {
            map: map.clone(),
            ip,
        })
    }
}

impl Drop for IpGuard {
    fn drop(&mut self) {
        let mut m = self.map.lock().expect("per-ip lock");
        if let Some(n) = m.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                m.remove(&self.ip);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_ip_cap_releases_on_drop() {
        let map = Arc::default();
        let ip: IpAddr = [10, 0, 0, 1].into();
        let guards: Vec<_> = (0..MAX_CONNECTIONS_PER_IP)
            .map(|_| IpGuard::acquire(&map, ip).unwrap())
            .collect();
        assert!(IpGuard::acquire(&map, ip).is_none());
        assert!(IpGuard::acquire(&map, [10, 0, 0, 2].into()).is_some());
        drop(guards);
        assert!(IpGuard::acquire(&map, ip).is_some());
        assert_eq!(map.lock().unwrap().len(), 0);
    }
}
