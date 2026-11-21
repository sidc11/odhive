//! `odhive` — the daily driver for Odoo developers.
#![forbid(unsafe_code)]

mod cli;
mod commands;

use std::process::ExitCode;

use anyhow::Context;
use clap::Parser;
use odhive_core::config::AppPaths;
use odhive_core::platform::{effective_uid, ensure_not_root};
use odhive_core::ports::{DEFAULT_PORT_ATTEMPTS, bind_loopback};
use odhive_core::workspace::bootstrap;
use tracing_subscriber::EnvFilter;

use crate::cli::Cli;

#[tokio::main]
async fn main() -> anyhow::Result<ExitCode> {
    let cli = Cli::parse();
    // Commands print their own output; the UI server logs what it does.
    let default_level = if cli.command.is_some() {
        "warn"
    } else {
        "info"
    };
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level)),
        )
        .init();
    ensure_not_root(effective_uid())?;

    match cli.command {
        Some(command) => commands::dispatch(command).await,
        None => serve(cli.port).await.map(|()| ExitCode::SUCCESS),
    }
}

async fn serve(port: u16) -> anyhow::Result<()> {
    let paths = AppPaths::discover()?;
    let boot = bootstrap(&paths).context("could not prepare the OdHive workspace")?;
    if boot.config_created {
        tracing::info!(path = %paths.config_file().display(), "created config");
    }
    for dir in &boot.created {
        tracing::info!(path = %dir.display(), "created folder");
    }
    tracing::info!(root = %boot.workspace.root().display(), "workspace ready");

    let listeners = bind_loopback(port, DEFAULT_PORT_ATTEMPTS)
        .await
        .context("could not bind the OdHive UI port")?;
    if listeners.port != port {
        tracing::warn!(
            requested = port,
            actual = listeners.port,
            "port {} is busy, using {}",
            port,
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
    use odhive_core::launch::Target;

    use crate::cli::{Command, ProjectCommand, SourceCommand};

    #[test]
    fn cli_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn default_port_is_7069() {
        let cli = Cli::try_parse_from(["odhive"]).expect("parse");
        assert_eq!(cli.port, 7069);
        assert!(cli.command.is_none());
    }

    #[test]
    fn port_zero_is_rejected() {
        assert!(Cli::try_parse_from(["odhive", "--port", "0"]).is_err());
    }

    #[test]
    fn start_takes_a_project_or_instance_target() {
        let cli = Cli::try_parse_from(["odhive", "start", "shop#test", "-q"]).expect("parse");
        let Some(Command::Start(args)) = cli.command else {
            panic!("{:?}", cli.command);
        };
        assert_eq!(
            args.target.target,
            "shop#test".parse::<Target>().expect("t")
        );
        assert!(args.quiet);
        for bad in ["Shop", "shop#", "shop#a#b", "../etc"] {
            assert!(
                Cli::try_parse_from(["odhive", "stop", bad]).is_err(),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn project_add_requires_a_source() {
        assert!(Cli::try_parse_from(["odhive", "project", "add", "shop"]).is_err());
        let cli = Cli::try_parse_from([
            "odhive",
            "project",
            "add",
            "shop",
            "--source",
            "odoo-19.0",
            "--addons",
            "/a",
            "--addons",
            "/b",
            "--no-requirements",
        ])
        .expect("parse");
        let Some(Command::Project(ProjectCommand::Add(args))) = cli.command else {
            panic!("{:?}", cli.command);
        };
        assert_eq!(args.addons.len(), 2);
        assert!(args.env.no_requirements && !args.no_setup);
    }

    #[test]
    fn enterprise_sources_need_a_version() {
        assert!(Cli::try_parse_from(["odhive", "source", "add", "/e", "--enterprise"]).is_err());
        let cli = Cli::try_parse_from([
            "odhive",
            "source",
            "add",
            "/e",
            "--enterprise",
            "--odoo-version",
            "19.0",
        ])
        .expect("parse");
        assert!(matches!(
            cli.command,
            Some(Command::Source(SourceCommand::Add {
                enterprise: true,
                odoo_version: Some(_),
                ..
            }))
        ));
        assert!(Cli::try_parse_from(["odhive", "source", "clone", "18.0"]).is_err());
    }

    #[test]
    fn logs_defaults_to_fifty_lines() {
        let cli = Cli::try_parse_from(["odhive", "logs", "shop", "-f"]).expect("parse");
        let Some(Command::Logs(args)) = cli.command else {
            panic!("{:?}", cli.command);
        };
        assert!(args.follow);
        assert_eq!(args.lines, 50);
    }
}
