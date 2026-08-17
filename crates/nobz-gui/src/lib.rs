//! Nobz GUI library (eframe app).
//!
//! Exposed as a library so the binary is a thin shim and tests can drive the
//! app state directly.

pub type Result<T> = std::result::Result<T, anyhow::Error>;

pub fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tracing::info!("nobz starting (GUI stub)");

    // M5 will replace this with a real eframe app.
    Ok(())
}
