//! Signaling server. Relays [`ws_proto::SignalMessage`]s between the host and
//! client of a session over WebSocket and hands out TURN credentials. It never
//! carries media.

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    tracing::info!("ws-signaling: not implemented yet");
    Ok(())
}
