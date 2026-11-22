//! `odhive` — the daily driver for Odoo developers.
#![forbid(unsafe_code)]

mod autostart;
mod cli;
mod client;
mod commands;
mod service;

use std::process::ExitCode;

use clap::Parser;
use odhive_core::platform::{effective_uid, ensure_not_root};
use tracing_subscriber::EnvFilter;

use crate::cli::{Cli, Command, ServiceCommand};

#[tokio::main]
async fn main() -> anyhow::Result<ExitCode> {
    let cli = Cli::parse();
    // Commands print their own output; the service logs what it does.
    let serving = matches!(&cli.command, Some(Command::Serve(a)) if a.foreground);
    let default_level = if serving { "info" } else { "warn" };
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        // No colour codes in the service log.
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level)),
        )
        .init();
    ensure_not_root(effective_uid())?;

    match cli.command {
        None => service::open(cli.port, !cli.no_browser).await,
        Some(Command::Serve(args)) if args.foreground => {
            let mode = if args.detached {
                service::Mode::Detached
            } else {
                service::Mode::Foreground
            };
            service::run(cli.port, mode).await
        }
        Some(Command::Serve(_)) => service::start(cli.port).await,
        Some(Command::Quit) => service::quit().await,
        Some(Command::Service(ServiceCommand::Install)) => autostart::install(cli.port).await,
        Some(Command::Service(ServiceCommand::Uninstall)) => autostart::uninstall().await,
        Some(command) => commands::dispatch(command).await,
    }
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
        assert!(!cli.no_browser);
    }

    #[test]
    fn serve_runs_detached_only_in_the_foreground() {
        let cli = Cli::try_parse_from(["odhive", "--port", "7070", "serve", "--foreground"])
            .expect("parse");
        assert!(matches!(
            cli.command,
            Some(Command::Serve(cli::ServeArgs {
                foreground: true,
                detached: false
            }))
        ));
        assert_eq!(cli.port, 7070);
        assert!(Cli::try_parse_from(["odhive", "serve", "--detached"]).is_err());
        assert!(Cli::try_parse_from(["odhive", "quit"]).is_ok());
        let cli =
            Cli::try_parse_from(["odhive", "--port", "7070", "service", "install"]).expect("parse");
        assert!(matches!(
            cli.command,
            Some(Command::Service(ServiceCommand::Install))
        ));
        assert!(Cli::try_parse_from(["odhive", "service", "uninstall"]).is_ok());
        assert!(Cli::try_parse_from(["odhive", "service"]).is_err());
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
