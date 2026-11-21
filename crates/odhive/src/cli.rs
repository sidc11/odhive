//! Command-line interface definition.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use odhive_core::launch::Target;
use odhive_core::ports::DEFAULT_UI_PORT;
use odhive_core::version::OdooVersion;

/// Your Odoo servers, one hive.
///
/// Without a command, OdHive serves its web UI on 127.0.0.1.
#[derive(Debug, Parser)]
#[command(name = "odhive", version, about)]
pub struct Cli {
    /// Preferred UI port; the next free port is used if it is busy.
    #[arg(long, default_value_t = DEFAULT_UI_PORT, value_parser = clap::value_parser!(u16).range(1..))]
    pub port: u16,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create, list and remove projects.
    #[command(subcommand)]
    Project(ProjectCommand),
    /// Register Odoo source trees shared by projects.
    #[command(subcommand)]
    Source(SourceCommand),
    /// Start instances and show their output until Ctrl+C.
    Start(RunArgs),
    /// Stop running instances, then start them in this terminal.
    Restart(RunArgs),
    /// Stop running instances.
    Stop(TargetArg),
    /// Show which instances run.
    Status(StatusArgs),
    /// Show an instance's log.
    Logs(LogsArgs),
}

#[derive(Debug, Subcommand)]
pub enum ProjectCommand {
    /// Create a project in the workspace and set up its Python environment.
    Add(ProjectAddArgs),
    /// Create or update a project's venv and install its requirements.
    Setup(ProjectSetupArgs),
    /// List projects.
    List,
    /// Unregister a project. Its folder is kept.
    Remove {
        /// Project name.
        name: String,
    },
}

#[derive(Debug, Args)]
pub struct ProjectAddArgs {
    /// Project name: lowercase letters, digits, '_' and '-'.
    pub name: String,
    /// Odoo Community source: a registered id or the path of an Odoo tree.
    #[arg(long, value_name = "ID|PATH")]
    pub source: String,
    /// Odoo Enterprise source: a registered id or the path of an Enterprise tree.
    #[arg(long, value_name = "ID|PATH")]
    pub enterprise: Option<String>,
    /// Extra addons folder (repeatable).
    #[arg(long = "addons", value_name = "DIR")]
    pub addons: Vec<PathBuf>,
    #[command(flatten)]
    pub env: EnvArgs,
    /// Only create the project; set up its venv later with `odhive project setup`.
    #[arg(long)]
    pub no_setup: bool,
}

#[derive(Debug, Args)]
pub struct ProjectSetupArgs {
    /// Project name.
    pub name: String,
    #[command(flatten)]
    pub env: EnvArgs,
    /// Delete and rebuild an existing venv.
    #[arg(long)]
    pub recreate: bool,
}

#[derive(Debug, Args)]
pub struct EnvArgs {
    /// Python interpreter for the venv (default: newest one Odoo supports).
    #[arg(long, value_name = "PATH")]
    pub python: Option<PathBuf>,
    /// Do not install Python requirements.
    #[arg(long)]
    pub no_requirements: bool,
}

#[derive(Debug, Subcommand)]
pub enum SourceCommand {
    /// Register an existing Odoo Community or Enterprise tree.
    Add {
        /// Path of the tree.
        path: PathBuf,
        /// It is an Enterprise tree (needs --odoo-version).
        #[arg(long, requires = "odoo_version")]
        enterprise: bool,
        /// Odoo version of an Enterprise tree, e.g. 19.0.
        #[arg(long, value_name = "VERSION")]
        odoo_version: Option<OdooVersion>,
    },
    /// Shallow-clone Odoo Community into the workspace.
    Clone {
        /// Odoo version, e.g. 19.0.
        version: OdooVersion,
    },
    /// List sources.
    List,
    /// Unregister a source. Its files are kept.
    Remove {
        /// Source id.
        id: String,
    },
}

#[derive(Debug, Args)]
pub struct TargetArg {
    /// `<project>` (all its instances) or `<project>#<instance>`.
    #[arg(value_name = "PROJECT[#INSTANCE]")]
    pub target: Target,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    #[command(flatten)]
    pub target: TargetArg,
    /// Do not print instance output (it is still written to the log file).
    #[arg(short, long)]
    pub quiet: bool,
}

#[derive(Debug, Args)]
pub struct StatusArgs {
    /// `<project>` or `<project>#<instance>`; all projects when omitted.
    #[arg(value_name = "PROJECT[#INSTANCE]")]
    pub target: Option<Target>,
}

#[derive(Debug, Args)]
pub struct LogsArgs {
    #[command(flatten)]
    pub target: TargetArg,
    /// Keep printing new lines.
    #[arg(short, long)]
    pub follow: bool,
    /// Number of lines to show first.
    #[arg(short = 'n', long, default_value_t = 50)]
    pub lines: usize,
}
