//! Environment check: whether this machine has what Odoo development needs.
//!
//! Checks PostgreSQL (reachable, server version, role may create databases),
//! Python interpreters for each Odoo series, `wkhtmltopdf`, git and uv. Each
//! check ends with a status and, when something is off, a fix: advice and
//! install commands as **text** for the host's package manager. Like
//! [`crate::syslib`], OdHive never runs these commands.
//!
//! Tools are found on `PATH` and asked for `--version` directly (no shell,
//! no stdin, output capped, killed on timeout).

pub(crate) mod postgres;
mod tools;

#[cfg(test)]
mod tests;

use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use serde::Serialize;

use crate::python::{self, Compat, DiscoverOptions, Discovery, Interpreter, PySupport};
use crate::syslib::PackageManager;
use crate::version::{self, OdooVersion};

pub use crate::db::PgTarget;

/// How a check came out, from best to worst.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    serde::Deserialize,
    utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
#[schema(as = EnvStatus)]
pub enum Status {
    /// Present and usable.
    Ok,
    /// Missing, but optional.
    Info,
    /// Usable with limits (e.g. too old for one Odoo series).
    Warning,
    /// Missing or unusable: Odoo cannot run without it.
    Error,
}

/// What a check looked at.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    serde::Deserialize,
    utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
#[schema(as = EnvCheckId)]
pub enum CheckId {
    /// The PostgreSQL server Odoo connects to by default.
    Postgresql,
    /// Python interpreters for each Odoo series.
    Python,
    /// The program Odoo prints PDF reports with.
    Wkhtmltopdf,
    /// Needed to clone Odoo.
    Git,
    /// Faster venvs and installs (pip is used without it).
    Uv,
}

/// How to fix a check: advice and commands for the user to run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, serde::Deserialize, utoipa::ToSchema)]
#[schema(as = EnvFix)]
pub struct Fix {
    /// What to do, as a sentence.
    pub text: String,
    /// Commands to run, in order (text only; OdHive never runs them).
    pub commands: Vec<String>,
    /// Where to read more or download from.
    pub url: Option<String>,
}

/// The result of one check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize, utoipa::ToSchema)]
#[schema(as = EnvCheck)]
pub struct Check {
    /// What was checked.
    pub id: CheckId,
    /// How it came out.
    pub status: Status,
    /// One line describing the result.
    pub summary: String,
    /// Version found, e.g. `16.4` or `2.47.1`.
    pub version: Option<String>,
    /// Program found, or where PostgreSQL was reached.
    pub location: Option<String>,
    /// Further findings, one per line.
    pub details: Vec<String>,
    /// How to fix it; `None` when nothing needs fixing.
    pub fix: Option<Fix>,
}

impl Check {
    fn new(id: CheckId, status: Status, summary: impl Into<String>) -> Self {
        Self {
            id,
            status,
            summary: summary.into(),
            version: None,
            location: None,
            details: Vec::new(),
            fix: None,
        }
    }
}

/// Every check, in display order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize, utoipa::ToSchema)]
#[schema(as = EnvCheckReport)]
pub struct Report {
    /// Package manager the install commands are written for; `None` when the
    /// host's is unknown (commands are then left out).
    pub package_manager: Option<PackageManager>,
    /// The checks: PostgreSQL, Python, wkhtmltopdf, git, uv.
    pub checks: Vec<Check>,
}

impl Report {
    /// The worst status of any check.
    #[must_use]
    pub fn worst(&self) -> Status {
        self.checks
            .iter()
            .map(|c| c.status)
            .max()
            .unwrap_or(Status::Ok)
    }
}

/// What an Odoo series needs from Python and PostgreSQL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requirement {
    /// Shown in findings, e.g. `Odoo 19.0` or `Odoo 19.0 (odoo-19.0)`.
    pub label: String,
    /// Supported Python range.
    pub python: PySupport,
    /// Oldest supported PostgreSQL major version.
    pub min_pg: u32,
}

impl Requirement {
    /// Ranges shipped by Odoo 19.0 and 20.0 (checked 2026-11-24), used when
    /// no source is registered.
    #[must_use]
    pub fn builtin() -> Vec<Self> {
        [19, 20]
            .into_iter()
            .filter_map(|major| OdooVersion::new(major, 0, false).ok())
            .map(|v| Self {
                label: format!("Odoo {v}"),
                python: PySupport::for_odoo(v),
                min_pg: min_pg_for_odoo(v),
            })
            .collect()
    }

    /// The requirement of a Community source tree, from its `release.py`
    /// (falling back to the series' known values).
    #[must_use]
    pub fn for_source(id: &str, path: &Path, version: OdooVersion) -> Self {
        let min_pg = version::read_release_py(path)
            .ok()
            .and_then(|t| min_pg_from_release_py(&t))
            .unwrap_or_else(|| min_pg_for_odoo(version));
        Self {
            label: format!("Odoo {version} ({id})"),
            python: PySupport::for_source(path, version),
            min_pg,
        }
    }
}

/// PostgreSQL major version Odoo 19.0 (13) and 20.0 (16) require.
#[must_use]
pub fn min_pg_for_odoo(version: OdooVersion) -> u32 {
    if version.major >= 20 { 16 } else { 13 }
}

/// `MIN_PG_VERSION = 13` from `release.py` text (a top-level line).
#[must_use]
pub fn min_pg_from_release_py(text: &str) -> Option<u32> {
    text.lines().find_map(|line| {
        let rest = line.strip_prefix("MIN_PG_VERSION")?.trim_start();
        let rest = rest.strip_prefix('=')?;
        if rest.starts_with('=') {
            return None;
        }
        let value = rest.split('#').next()?.trim();
        (!value.is_empty() && value.len() <= 3 && value.bytes().all(|b| b.is_ascii_digit()))
            .then(|| value.parse().ok())
            .flatten()
    })
}

/// Inputs to [`run`].
#[derive(Debug, Clone)]
pub struct CheckOptions {
    /// `PATH`-style list wkhtmltopdf, git and uv are looked up in; `None`
    /// finds none of them.
    pub path_var: Option<OsString>,
    /// Where Python interpreters (and uv's) are looked for.
    pub discover: DiscoverOptions,
    /// The PostgreSQL server to try.
    pub postgres: PgTarget,
    /// What each Odoo series needs; empty = [`Requirement::builtin`].
    pub requirements: Vec<Requirement>,
    /// Package manager install commands are written for.
    pub package_manager: Option<PackageManager>,
    /// Time each tool probe and each PostgreSQL connection may take.
    pub timeout: Duration,
}

impl CheckOptions {
    /// Default time for a probe or a connection.
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

    /// The service's `PATH`, PostgreSQL from libpq's environment, the host's
    /// package manager and the built-in requirements.
    #[must_use]
    pub fn from_env(discover: DiscoverOptions) -> Self {
        Self {
            path_var: std::env::var_os("PATH"),
            discover,
            postgres: PgTarget::from_env(),
            requirements: Vec::new(),
            package_manager: PackageManager::host(),
            timeout: Self::DEFAULT_TIMEOUT,
        }
    }
}

/// Runs every check (concurrently). Never fails: problems are findings.
pub async fn run(opts: &CheckOptions) -> Report {
    let builtin;
    let requirements = if opts.requirements.is_empty() {
        builtin = Requirement::builtin();
        &builtin
    } else {
        &opts.requirements
    };
    let pm = opts.package_manager;
    let path_var = opts.path_var.as_deref();
    let (pg, found, wk, git, uv) = tokio::join!(
        postgres::check(&opts.postgres, path_var, requirements, pm, opts.timeout),
        python::discover(&opts.discover),
        tools::wkhtmltopdf(path_var, pm, opts.timeout),
        tools::git(path_var, pm, opts.timeout),
        tools::uv(path_var, pm, opts.timeout),
    );
    let has_uv = uv.status == Status::Ok;
    Report {
        package_manager: pm,
        checks: vec![
            pg,
            python_check(&found, requirements, pm, has_uv),
            wk,
            git,
            uv,
        ],
    }
}

/// Python: for each requirement, the interpreter a setup would pick.
fn python_check(
    found: &Discovery,
    requirements: &[Requirement],
    pm: Option<PackageManager>,
    has_uv: bool,
) -> Check {
    let picks: Vec<Option<&Interpreter>> = requirements
        .iter()
        .map(|r| python::pick(&found.interpreters, r.python).ok())
        .collect();
    let fitting = picks.iter().filter(|p| p.is_some()).count();
    let (status, summary) = if found.interpreters.is_empty() {
        (Status::Error, "No Python 3 interpreter found".to_owned())
    } else if fitting == requirements.len() {
        let count = found.interpreters.len();
        let s = if count == 1 { "" } else { "s" };
        (
            Status::Ok,
            format!("{count} interpreter{s} found; every Odoo series has one that fits"),
        )
    } else if fitting > 0 {
        let missing: Vec<&str> = requirements
            .iter()
            .zip(&picks)
            .filter(|(_, p)| p.is_none())
            .map(|(r, _)| r.label.as_str())
            .collect();
        (
            Status::Warning,
            format!("No fitting Python for {}", missing.join(", ")),
        )
    } else {
        (
            Status::Error,
            "No installed Python fits a supported Odoo series".to_owned(),
        )
    };
    let mut check = Check::new(CheckId::Python, status, summary);
    for (r, pick) in requirements.iter().zip(&picks) {
        check.details.push(match pick {
            Some(i) => format!("{}: Python {} ({})", r.label, i.version, i.path.display()),
            None => format!("{}: needs {}, none found", r.label, r.python),
        });
    }
    for i in &found.interpreters {
        let note = match (i.is_standard_cpython(), &i.implementation) {
            (true, _) => String::new(),
            (false, imp) if imp != "cpython" => format!(", {imp}: not used"),
            (false, _) => ", free-threaded: not used".to_owned(),
        };
        check.details.push(format!(
            "Found Python {} at {}{note}",
            i.version,
            i.path.display()
        ));
    }
    if let Some(e) = &found.uv_error {
        check.details.push(format!("uv's interpreters: {e}"));
    }
    if status != Status::Ok {
        // The newest range any unmet requirement accepts.
        let want = requirements
            .iter()
            .zip(&picks)
            .filter(|(_, p)| p.is_none())
            .map(|(r, _)| r.python)
            .max_by_key(|s| s.max)
            .unwrap_or_else(|| requirements[0].python);
        check.fix = Some(python_fix(want, pm, has_uv, &found.interpreters));
    }
    check
}

fn python_fix(
    want: PySupport,
    pm: Option<PackageManager>,
    has_uv: bool,
    found: &[Interpreter],
) -> Fix {
    // Prefer the newest minor in range that is not newer than the newest
    // supported one: wheels for brand-new Pythons lag behind.
    let (major, minor) = want.max;
    let version = format!("{major}.{minor}");
    let too_new = found
        .iter()
        .any(|i| want.check(i.version) == Compat::TooNew);
    let mut text = format!("Install a standard CPython in the {want} range.");
    if too_new {
        text.push_str(" Newer versions are installed, but Odoo does not support them yet.");
    }
    if has_uv {
        return Fix {
            text,
            commands: vec![format!("uv python install {version}")],
            url: None,
        };
    }
    let commands = match pm {
        Some(PackageManager::Apt) => {
            vec![PackageManager::Apt.install_command(&["python3", "python3-venv", "python3-dev"])]
        }
        Some(PackageManager::Brew) => {
            vec![PackageManager::Brew.install_command(&[&format!("python@{version}")])]
        }
        Some(m) => vec![m.install_command(&[if m == PackageManager::Pacman {
            "python"
        } else {
            "python3"
        }])],
        None => Vec::new(),
    };
    Fix {
        text,
        commands,
        url: Some("https://docs.astral.sh/uv/guides/install-python/".into()),
    }
}
