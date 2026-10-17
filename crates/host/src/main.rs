//! Host agent entry point.

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    match ws_capture::list_displays() {
        Ok(displays) => {
            for d in displays {
                tracing::info!(id = d.id.0, name = %d.name, "{}x{}", d.width, d.height);
            }
        }
        Err(e) => tracing::warn!("listing displays failed: {e}"),
    }
    Ok(())
}
