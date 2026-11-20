//! `odhive` — the daily driver for Odoo developers.
#![forbid(unsafe_code)]

use anyhow::Context;
use clap::Parser;
use odhive_core::config::AppPaths;
use odhive_core::platform::{effective_uid, ensure_not_root};
use odhive_core::ports::{DEFAULT_PORT_ATTEMPTS, DEFAULT_UI_PORT, bind_loopback};
use odhive_core::workspace::bootstrap;
use tracing_subscriber::EnvFilter;

/// Your Odoo servers, one hive.
#[derive(Debug, Parser)]
#[command(name = "odhive", version, about)]
struct Cli {
    /// Preferred UI port; the next free port is used if it is busy.
    #[arg(long, default_value_t = DEFAULT_UI_PORT, value_parser = clap::value_parser!(u16).range(1..))]
    port: u16,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    ensure_not_root(effective_uid())?;

    let paths = AppPaths::discover()?;
    let boot = bootstrap(&paths).context("could not prepare the OdHive workspace")?;
    if boot.config_created {
        tracing::info!(path = %paths.config_file().display(), "created config");
    }
    for dir in &boot.created {
        tracing::info!(path = %dir.display(), "created folder");
    }
    tracing::info!(root = %boot.workspace.root().display(), "workspace ready");

    let listeners = bind_loopback(cli.port, DEFAULT_PORT_ATTEMPTS)
        .await
        .context("could not bind the OdHive UI port")?;
    if listeners.port != cli.port {
        tracing::warn!(
            requested = cli.port,
            actual = listeners.port,
            "port {} is busy, using {}",
            cli.port,
            listeners.port
        );
    }

    odhive_server::serve(listeners, async {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("shutting down");
    })
    .await
    .context("server error")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn default_port_is_7069() {
        let cli = Cli::try_parse_from(["odhive"]).expect("parse");
        assert_eq!(cli.port, 7069);
    }

    #[test]
    fn port_zero_is_rejected() {
        assert!(Cli::try_parse_from(["odhive", "--port", "0"]).is_err());
    }
}
