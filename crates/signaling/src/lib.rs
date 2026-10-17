//! The HTTPS server built into the host agent: it serves the web client,
//! pairs browsers with a one-time code, and carries WebRTC signaling over a
//! WebSocket. Messages are [`ws_proto::HostSignal`] and
//! [`ws_proto::ClientSignal`]; the full contract is in `docs/signaling.md`.

pub mod config;
mod pairing;
mod server;
mod store;
mod tls;
mod turn;

pub use config::{SignalingConfig, TlsSource, TurnServer};
pub use server::{Notice, SignalingEvents, SignalingHandle, SignalingServer, SignalingSession};

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
}
