//! rinit — A Rust-based init system (PID 1)
//!
//! Architecture layers:
//!   1. Early bootstrap (mounts, hostname, signals)
//!   2. Unit file loading & dependency resolution
//!   3. Event-driven service supervision loop

use anyhow::{Context, Result};
use tracing::{info, warn};

mod bootstrap;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    if std::process::id() != 1 {
        anyhow::bail!(
            "rinit must run as PID 1 (current pid: {})",
            std::process::id()
        );
    }

    tracing_subscriber::fmt()
        .with_target(false)
        .with_level(true)
        .with_ansi(false)
        .init();

    info!("rinit starting as PID 1");

    bootstrap::early_init().context("early bootstrap failed")?;

    let unit_registry = init_unit::load_all_units().context("failed to load units")?;
    info!(
        "loaded {} unit(s) from config directories",
        unit_registry.len()
    );

    let mut runtime = init_event::Runtime::new(unit_registry)?;

    warn!("entering main event loop");
    runtime.run().await?;

    info!("rinit shutting down");
    Ok(())
}
