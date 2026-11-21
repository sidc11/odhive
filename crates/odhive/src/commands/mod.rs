//! Subcommand implementations. All logic lives in `odhive-core`; this layer
//! parses, prints and maps results to exit codes.

pub mod logs;
pub mod project;
pub mod run;
pub mod source;
pub mod status;

use std::io::Write;
use std::process::ExitCode;

use anyhow::Context;
use odhive_core::config::{AppPaths, GlobalConfig};
use odhive_core::pidfile::RunDir;
use odhive_core::workspace::{Workspace, bootstrap};

use crate::cli::{Command, ProjectCommand, SourceCommand};

/// Paths, global config and workspace of one CLI invocation.
pub struct Ctx {
    pub paths: AppPaths,
    pub cfg: GlobalConfig,
    pub ws: Workspace,
}

impl Ctx {
    pub fn open() -> anyhow::Result<Self> {
        let paths = AppPaths::discover()?;
        let boot = bootstrap(&paths).context("could not prepare the OdHive workspace")?;
        Ok(Self {
            paths,
            cfg: boot.config,
            ws: boot.workspace,
        })
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let file = self.paths.config_file();
        self.cfg
            .save(&file)
            .with_context(|| format!("could not save {}", file.display()))
    }

    pub fn run_dir(&self) -> RunDir {
        RunDir::new(self.paths.run_dir())
    }
}

pub async fn dispatch(command: Command) -> anyhow::Result<ExitCode> {
    let mut ctx = Ctx::open()?;
    match command {
        Command::Project(ProjectCommand::Add(args)) => project::add(&mut ctx, args).await,
        Command::Project(ProjectCommand::Setup(args)) => project::setup(&mut ctx, args).await,
        Command::Project(ProjectCommand::List) => project::list(&ctx),
        Command::Project(ProjectCommand::Remove { name }) => project::remove(&mut ctx, &name),
        Command::Source(SourceCommand::Add {
            path,
            enterprise,
            odoo_version,
        }) => source::add(
            &mut ctx,
            &path,
            enterprise.then_some(odoo_version).flatten(),
        ),
        Command::Source(SourceCommand::Clone { version }) => source::clone(&mut ctx, version).await,
        Command::Source(SourceCommand::List) => source::list(&ctx),
        Command::Source(SourceCommand::Remove { id }) => source::remove(&mut ctx, &id),
        Command::Start(args) => run::start(&ctx, &args.target.target, args.quiet).await,
        Command::Restart(args) => run::restart(&ctx, &args.target.target, args.quiet).await,
        Command::Stop(args) => run::stop(&ctx, &args.target).await,
        Command::Status(args) => status::status(&ctx, args.target.as_ref()),
        Command::Logs(args) => logs::logs(&ctx, &args.target.target, args.follow, args.lines).await,
    }
}

/// Prints a line to stdout, ignoring a closed pipe (`odhive logs | head`).
pub fn say(line: impl std::fmt::Display) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
}

/// Prints a table with left-aligned columns.
pub fn table(header: &[&str], rows: &[Vec<String>]) {
    let mut widths: Vec<usize> = header.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let line = |cells: Vec<&str>| {
        let mut s = String::new();
        for (i, (cell, w)) in cells.iter().zip(&widths).enumerate() {
            if i + 1 == cells.len() {
                s.push_str(cell);
            } else {
                s.push_str(&format!("{cell:<w$}  "));
            }
        }
        s.trim_end().to_owned()
    };
    say(line(header.to_vec()));
    for row in rows {
        say(line(row.iter().map(String::as_str).collect()));
    }
}
