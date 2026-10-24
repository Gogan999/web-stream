//! The HTTPS server built into the host agent: it serves the web client,
//! pairs browsers with a one-time code, and carries WebRTC signaling over a
//! WebSocket. Messages are [`ws_proto::HostSignal`] and
//! [`ws_proto::ClientSignal`]; the full contract is in `docs/signaling.md`.

mod acl;
mod acme;
pub mod config;
mod listen;
mod pairing;
mod probe;
mod server;
mod store;
mod tls;
mod turn;

pub use acme::valid_name as valid_duckdns_name;
pub use config::{
    DuckDns, LETS_ENCRYPT, LETS_ENCRYPT_STAGING, SignalingConfig, TlsSource, TurnServer,
};
pub use probe::certificate_at;
pub use server::{
    Notice, SessionEvent, SignalingEvents, SignalingHandle, SignalingServer, SignalingSession,
};
pub use store::write_private;
pub use tls::CertInfo;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("tls: {0}")]
    Tls(String),
    #[error("device store: {0}")]
    Store(String),
    #[error("random source: {0}")]
    Random(String),
    #[error("{0}")]
    Acme(String),
}
