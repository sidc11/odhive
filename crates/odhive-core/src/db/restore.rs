//! Restoring a database dump the way Odoo's own database manager does, with
//! OdHive checking the file first.
//!
//! [`restore`] takes a dump already on disk (see [`archive::detect`] for the
//! formats) and, never through a shell:
//!
//! 1. **Inspect**: tells the format; an Odoo backup zip is checked entry by
//!    entry and unpacked into a private staging folder
//!    ([`archive::extract_odoo_zip`]). Its `manifest.json` must name the
//!    project's Odoo series when it names one.
//! 2. **Create**: `createdb --encoding=UTF8 --template=template0 --lc-collate=C <db>`
//!    (Odoo's `_create_empty_database`; the project's `db_template` if set).
//! 3. **Load**: `psql` for SQL (with `\restrict <random key>`, so the script
//!    cannot run psql meta-commands such as `\!`), `pg_restore --no-owner`
//!    for custom and tar archives.
//! 4. **Check**: the database must hold Odoo's `base` module of the
//!    project's series.
//! 5. **Neutralize** (unless asked not to): `odoo-bin neutralize`, then the
//!    `database.is_neutralized` flag must be set.
//! 6. **Filestore**: the zip's `filestore/` moves to `<data_dir>/filestore/<db>`.
//! 7. **Finalize**: `odoo-bin shell` runs [`FINALIZE_RESTORE_SCRIPT`]: it
//!    loads the registry and, for a copy, gives the database a new UUID.
//!
//! The PostgreSQL client tools get the connection in libpq variables
//! (`PGHOST`, `PGPORT`, `PGUSER`, `PGPASSWORD`, `PGSSLMODE`), never on a
//! command line. If a step after the database exists fails, or the work is
//! dropped (job cancelled), the database and its filestore are removed with
//! `odoo-bin db drop`.
//!
//! A dump is SQL run as the project's role: restore only dumps you trust.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::process::Command;

use super::PgTarget;
use super::archive::{self, ArchiveError, DumpFormat, ExtractLimits, Extracted};
use super::create::{
    CreateError, DropGuard, OdooCommand, READY_MARKER, TAIL_LINES, drop_database,
    validate_new_db_name,
};
use super::list::{odoo_facts, pg_message, series};
use crate::conf::OdooOptions;
use crate::envcheck::postgres::{self, Place};
use crate::names;
use crate::proc::{self, RunError};

/// Run by `odoo-bin shell` (on stdin) as the last step. Fixed text.
pub const FINALIZE_RESTORE_SCRIPT: &str = r#"import os
if os.environ.get('ODHIVE_DB_NEW_UUID') == '1':
    env['ir.config_parameter'].init(force=True)
env.cr.commit()
print('odhive: database ready', flush=True)
"#;

/// `ir.config_parameter` key Odoo's neutralization sets.
pub const NEUTRALIZED_KEY: &str = "database.is_neutralized";

/// Most `--version` output read from `psql`.
const MAX_VERSION_BYTES: u64 = 4096;

/// libpq variables that would point the client tools somewhere else than
/// the target; removed from their environment.
const PG_ENV_CLEARED: [&str; 5] = [
    "PGHOSTADDR",
    "PGSERVICE",
    "PGSERVICEFILE",
    "PGDATABASE",
    "PGOPTIONS",
];

/// A restore to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreRequest {
    /// Name of the new database.
    pub name: String,
    /// Run Odoo's neutralization (no mail, no crons, …). On by default.
    pub neutralize: bool,
    /// The dump is a copy: give the database a new UUID (Odoo's default).
    /// Off keeps the UUID, for a database that was moved.
    pub copy: bool,
}

impl RestoreRequest {
    /// `name`, neutralized, as a copy.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            neutralize: true,
            copy: true,
        }
    }
}

/// Where and how to restore.
#[derive(Debug, Clone)]
pub struct RestoreOptions {
    /// `<data_dir>/filestore`: the filestore goes to `<this>/<db>`.
    pub filestore_root: PathBuf,
    /// A private, empty folder for unpacking (on the same file system as
    /// `filestore_root`, so the filestore is moved, not copied).
    pub staging: PathBuf,
    /// The project's Odoo series, e.g. `19.0`; the dump must match it.
    pub series: Option<String>,
    /// Template database (`db_template`), `template0` by default.
    pub template: String,
    /// Unpacking limits.
    pub limits: ExtractLimits,
    /// Connection timeout.
    pub timeout: Duration,
}

/// The PostgreSQL client tools used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgTools {
    /// `createdb`.
    pub createdb: PathBuf,
    /// `psql`.
    pub psql: PathBuf,
    /// `pg_restore`.
    pub pg_restore: PathBuf,
    /// `pg_dump`.
    pub pg_dump: PathBuf,
    /// `psql`'s version, e.g. `(17, 6)`, when it could be read.
    pub psql_version: Option<(u32, u32)>,
}

impl PgTools {
    /// Finds the tools in `pg_path` (Odoo's option) if set, else on
    /// `path_var`, and asks `psql` its version.
    ///
    /// # Errors
    /// [`RestoreError::MissingTool`].
    pub async fn find(
        pg_path: Option<&Path>,
        path_var: Option<&OsStr>,
        timeout: Duration,
    ) -> Result<Self, RestoreError> {
        let dirs: OsString = match pg_path {
            Some(p) => p.as_os_str().to_owned(),
            None => path_var.map(OsStr::to_owned).unwrap_or_default(),
        };
        let find = |name: &'static str| {
            crate::python::find_executable(&dirs, name).ok_or(RestoreError::MissingTool { name })
        };
        let psql = find("psql")?;
        let psql_version = psql_version(&psql, timeout).await;
        Ok(Self {
            createdb: find("createdb")?,
            pg_restore: find("pg_restore")?,
            pg_dump: find("pg_dump")?,
            psql,
            psql_version,
        })
    }

    /// Whether `psql` knows `\restrict` (17.6, 16.10, 15.14, 14.19, 13.22
    /// and every release from 18 on), which stops a script from running
    /// meta-commands.
    #[must_use]
    pub fn psql_can_restrict(&self) -> bool {
        self.psql_version.is_some_and(restrict_supported)
    }
}

fn restrict_supported((major, minor): (u32, u32)) -> bool {
    match major {
        18.. => true,
        17 => minor >= 6,
        16 => minor >= 10,
        15 => minor >= 14,
        14 => minor >= 19,
        13 => minor >= 22,
        _ => false,
    }
}

/// `psql (PostgreSQL) 17.6 (Ubuntu 17.6-1)` → `(17, 6)`; `18beta1` → `(18, 0)`.
fn parse_psql_version(line: &str) -> Option<(u32, u32)> {
    let word = line
        .split_whitespace()
        .find(|w| w.starts_with(|c: char| c.is_ascii_digit()))?;
    let digits = |s: &str| -> Option<u32> {
        let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
        s[..end].parse().ok()
    };
    let mut parts = word.splitn(2, '.');
    let major = digits(parts.next()?)?;
    let minor = parts.next().and_then(digits).unwrap_or(0);
    Some((major, minor))
}

async fn psql_version(psql: &Path, timeout: Duration) -> Option<(u32, u32)> {
    let (out, _) = crate::python::run_capped(
        Command::new(psql).arg("--version").env("LC_ALL", "C"),
        psql,
        timeout,
        MAX_VERSION_BYTES,
    )
    .await
    .ok()?;
    parse_psql_version(String::from_utf8_lossy(&out).lines().next()?)
}

/// `pg_path` from the Odoo options, unless unset (`None`, `False`, empty).
#[must_use]
pub fn pg_path_option(options: &OdooOptions) -> Option<PathBuf> {
    options
        .get("pg_path")
        .map(str::trim)
        .filter(|v| !v.is_empty() && *v != "None" && *v != "False")
        .map(PathBuf::from)
}

/// `db_template` from the Odoo options, `template0` when unset.
#[must_use]
pub fn template_option(options: &OdooOptions) -> String {
    options
        .get("db_template")
        .map(str::trim)
        .filter(|v| !v.is_empty() && *v != "False")
        .unwrap_or("template0")
        .to_owned()
}

/// A step of [`restore`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RestoreStep {
    /// Checking the file and unpacking a zip.
    Inspect,
    /// Creating the empty database.
    Create,
    /// Loading the dump.
    Load,
    /// Checking it is an Odoo database of the project's series.
    Check,
    /// Running Odoo's neutralization.
    Neutralize,
    /// Moving the filestore into place.
    Filestore,
    /// Loading it in Odoo (and a new UUID for a copy).
    Finalize,
    /// Removing the half-restored database after a failure.
    Cleanup,
}

impl fmt::Display for RestoreStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Inspect => "checking the file",
            Self::Create => "creating the database",
            Self::Load => "loading the dump",
            Self::Check => "checking the database",
            Self::Neutralize => "neutralizing the database",
            Self::Filestore => "moving the filestore",
            Self::Finalize => "finishing the database",
            Self::Cleanup => "removing the half-restored database",
        })
    }
}

/// Progress of [`restore`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum RestoreEvent {
    /// A step started.
    Step {
        /// The step.
        step: RestoreStep,
    },
    /// What the file is (after inspecting).
    Inspected {
        /// Its format.
        format: DumpFormat,
        /// Bytes unpacked from a zip.
        unpacked_bytes: u64,
        /// Filestore files in a zip.
        filestore_files: u64,
    },
    /// A line of tool output.
    Output {
        /// The line.
        line: String,
    },
}

/// What [`restore`] made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RestoredDatabase {
    /// Database name.
    pub name: String,
    /// Format of the dump.
    pub format: DumpFormat,
    /// `base` module version, e.g. `19.0.1.3`.
    pub odoo_version: Option<String>,
    /// Whether it was neutralized.
    pub neutralized: bool,
    /// Whether it got a new UUID (restored as a copy).
    pub new_uuid: bool,
    /// Filestore files restored.
    pub filestore_files: u64,
    /// Name of the database the backup was made from (zip manifest).
    pub source_name: Option<String>,
}

/// Errors from [`restore`].
#[derive(Debug, Error)]
pub enum RestoreError {
    /// The database name is invalid.
    #[error(transparent)]
    Request(CreateError),
    /// A database with this name exists.
    #[error("a database named {0:?} already exists")]
    Exists(String),
    /// Its filestore folder exists already.
    #[error("{} exists: remove it or choose another name", .0.display())]
    FilestoreExists(PathBuf),
    /// The PostgreSQL server cannot be used.
    #[error("PostgreSQL: {0}")]
    Server(String),
    /// A client tool is not installed.
    #[error("{name} not found: install the PostgreSQL client tools or set pg_path")]
    MissingTool {
        /// Tool name.
        name: &'static str,
    },
    /// `psql` cannot run a script safely.
    #[error(
        "psql {version} cannot restrict meta-commands in a script: update the PostgreSQL client tools (17.6, 16.10, 15.14 or later)"
    )]
    PsqlTooOld {
        /// `psql` version, or `unknown`.
        version: String,
    },
    /// The file cannot be used.
    #[error(transparent)]
    Archive(#[from] ArchiveError),
    /// The dump is from another Odoo series.
    #[error("the dump is from Odoo {dump}, the project runs Odoo {project}")]
    VersionMismatch {
        /// Series of the dump.
        dump: String,
        /// Series of the project.
        project: String,
    },
    /// The loaded database has no Odoo `base` module.
    #[error(
        "the dump is not an Odoo database (no installed base module); the database was removed"
    )]
    NotOdoo,
    /// `neutralize` ran but the flag is not set.
    #[error("the database was not neutralized; it was removed")]
    NotNeutralized,
    /// A tool could not be started.
    #[error("could not run {}: {source}", program.display())]
    Spawn {
        /// Program.
        program: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// A step failed.
    #[error("{step} failed (exit code {code:?}). Last output:\n{tail}")]
    Failed {
        /// The step.
        step: RestoreStep,
        /// Exit code, if it exited normally.
        code: Option<i32>,
        /// Last output lines.
        tail: String,
    },
    /// A file operation failed.
    #[error("{action} {}: {source}", path.display())]
    Io {
        /// What was done.
        action: &'static str,
        /// Path.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
}

fn server_error(target: &PgTarget, e: postgres::ConnectError) -> RestoreError {
    let c = postgres::failure_check(target, e, None, None);
    let mut msg = c.summary;
    if let Some(d) = c.details.first() {
        msg.push_str(": ");
        msg.push_str(d);
    }
    RestoreError::Server(msg)
}

/// libpq variables for the client tools: where [`postgres::connect`]
/// reached the server, the role, password and TLS mode.
pub(crate) fn pg_env(target: &PgTarget, place: &Place) -> Vec<(&'static str, OsString)> {
    let mut env = vec![
        (
            "PGHOST",
            match place {
                Place::Socket(dir) => dir.as_os_str().to_owned(),
                Place::Tcp(host) => host.into(),
            },
        ),
        ("PGPORT", target.port.to_string().into()),
        ("PGSSLMODE", target.ssl_mode.as_str().into()),
        ("PGAPPNAME", "odhive".into()),
    ];
    if let Some(u) = &target.user {
        env.push(("PGUSER", u.into()));
    }
    if let Some(p) = &target.password {
        env.push(("PGPASSWORD", p.into()));
    }
    env
}

/// A client tool command: the project env, then the connection variables.
pub(super) fn pg_command(
    odoo: &OdooCommand,
    program: &Path,
    env: &[(&'static str, OsString)],
) -> Command {
    let mut cmd = Command::new(program);
    cmd.current_dir(&odoo.cwd).envs(&odoo.env.0);
    for k in PG_ENV_CLEARED {
        cmd.env_remove(k);
    }
    cmd.envs(env.iter().map(|(k, v)| (*k, v)));
    cmd.env("LC_ALL", "C");
    cmd
}

/// `createdb` arguments, as Odoo creates an empty database.
fn createdb_args(name: &str, template: &str) -> Vec<String> {
    let mut args = vec!["--encoding=UTF8".into(), format!("--template={template}")];
    if template == "template0" {
        args.push("--lc-collate=C".into());
    }
    args.push(name.to_owned());
    args
}

/// `psql` arguments for an SQL script: no `.psqlrc`, quiet, query output
/// discarded, meta-commands locked by a random key.
fn psql_args(name: &str, key: &str, script: &Path) -> Vec<OsString> {
    vec![
        "-X".into(),
        "-q".into(),
        "--output=/dev/null".into(),
        format!("--dbname={name}").into(),
        "-c".into(),
        format!("\\restrict {key}").into(),
        "-f".into(),
        script.as_os_str().to_owned(),
    ]
}

/// `pg_restore` arguments, as Odoo restores a raw dump.
fn pg_restore_args(name: &str, dump: &Path) -> Vec<OsString> {
    vec![
        "--no-owner".into(),
        format!("--dbname={name}").into(),
        dump.as_os_str().to_owned(),
    ]
}

fn random_key() -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("system random source");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Sets the flag when dropped: stops blocking work when its future is dropped.
struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Restores the dump at `dump` (see the module docs) on the server `target`
/// points at, which must be the one `odoo`'s config connects to. Reports
/// progress to `on_event`.
///
/// # Errors
/// See [`RestoreError`].
pub async fn restore(
    target: &PgTarget,
    odoo: &OdooCommand,
    tools: &PgTools,
    dump: &Path,
    req: &RestoreRequest,
    opts: &RestoreOptions,
    mut on_event: impl FnMut(RestoreEvent),
) -> Result<RestoredDatabase, RestoreError> {
    validate_new_db_name(&req.name).map_err(RestoreError::Request)?;
    names::validate_db_name(&opts.template)
        .map_err(|e| RestoreError::Request(CreateError::Name(e)))?;
    let conn = postgres::connect(target, opts.timeout)
        .await
        .map_err(|e| server_error(target, e))?;
    let found: bool = conn
        .client
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_database WHERE datname = $1)",
            &[&req.name],
        )
        .await
        .and_then(|r| r.try_get(0))
        .map_err(|e| RestoreError::Server(pg_message(&e)))?;
    if found {
        return Err(RestoreError::Exists(req.name.clone()));
    }
    let place = conn.place.clone();
    drop(conn);
    let filestore_dest = opts.filestore_root.join(&req.name);
    if filestore_dest.symlink_metadata().is_ok() {
        return Err(RestoreError::FilestoreExists(filestore_dest));
    }

    on_event(RestoreEvent::Step {
        step: RestoreStep::Inspect,
    });
    let format = archive::detect(dump)?;
    if format.uses_psql() && !tools.psql_can_restrict() {
        return Err(RestoreError::PsqlTooOld {
            version: tools
                .psql_version
                .map_or_else(|| "unknown".into(), |(a, b)| format!("{a}.{b}")),
        });
    }
    let extracted = if format == DumpFormat::Zip {
        Some(unpack(dump, &opts.staging, opts.limits).await?)
    } else {
        None
    };
    on_event(RestoreEvent::Inspected {
        format,
        unpacked_bytes: extracted.as_ref().map_or(0, |x| x.bytes),
        filestore_files: extracted.as_ref().map_or(0, |x| x.filestore_files),
    });
    let manifest = extracted.as_ref().and_then(|x| x.manifest.clone());
    if let (Some(project), Some(dump)) = (
        &opts.series,
        manifest.as_ref().and_then(|m| m.version.as_ref()),
    ) && project != dump
    {
        return Err(RestoreError::VersionMismatch {
            dump: dump.clone(),
            project: project.clone(),
        });
    }

    let env = pg_env(target, &place);
    on_event(RestoreEvent::Step {
        step: RestoreStep::Create,
    });
    let mut cmd = pg_command(odoo, &tools.createdb, &env);
    cmd.args(createdb_args(&req.name, &opts.template));
    run_tool(&tools.createdb, RestoreStep::Create, cmd, &mut on_event).await?;

    // From here on a failure or a drop removes the database.
    let mut guard = DropGuard(Some((odoo.clone(), req.name.clone())));
    let script = extracted.as_ref().map(|x| x.dump_sql.as_path());
    let outcome = load_and_finish(
        target,
        &place,
        odoo,
        tools,
        &env,
        format,
        script.unwrap_or(dump),
        extracted.as_ref(),
        &filestore_dest,
        req,
        opts,
        &mut on_event,
    )
    .await;
    guard.0 = None;
    match outcome {
        Ok(odoo_version) => Ok(RestoredDatabase {
            name: req.name.clone(),
            format,
            odoo_version,
            neutralized: req.neutralize,
            new_uuid: req.copy,
            filestore_files: extracted.as_ref().map_or(0, |x| x.filestore_files),
            source_name: manifest.and_then(|m| m.db_name),
        }),
        Err(e) => {
            on_event(RestoreEvent::Step {
                step: RestoreStep::Cleanup,
            });
            if let Err(why) = drop_database(odoo, &req.name, |line| {
                on_event(RestoreEvent::Output { line });
            })
            .await
            {
                tracing::warn!(db = %req.name, "could not remove the half-restored database: {why}");
            }
            Err(e)
        }
    }
}

async fn unpack(
    dump: &Path,
    staging: &Path,
    limits: ExtractLimits,
) -> Result<Extracted, RestoreError> {
    let cancel = Arc::new(AtomicBool::new(false));
    let _stop = CancelOnDrop(cancel.clone());
    let (zip, dest) = (dump.to_path_buf(), staging.join("unpacked"));
    tokio::task::spawn_blocking(move || archive::extract_odoo_zip(&zip, &dest, limits, &cancel))
        .await
        .map_err(|e| RestoreError::Archive(ArchiveError::Zip(e.to_string())))?
        .map_err(RestoreError::Archive)
}

#[allow(clippy::too_many_arguments)]
async fn load_and_finish(
    target: &PgTarget,
    place: &Place,
    odoo: &OdooCommand,
    tools: &PgTools,
    env: &[(&'static str, OsString)],
    format: DumpFormat,
    file: &Path,
    extracted: Option<&Extracted>,
    filestore_dest: &Path,
    req: &RestoreRequest,
    opts: &RestoreOptions,
    on_event: &mut impl FnMut(RestoreEvent),
) -> Result<Option<String>, RestoreError> {
    on_event(RestoreEvent::Step {
        step: RestoreStep::Load,
    });
    let (program, mut cmd) = if format.uses_psql() {
        let mut cmd = pg_command(odoo, &tools.psql, env);
        cmd.args(psql_args(&req.name, &random_key(), file));
        (&tools.psql, cmd)
    } else {
        let mut cmd = pg_command(odoo, &tools.pg_restore, env);
        cmd.args(pg_restore_args(&req.name, file));
        (&tools.pg_restore, cmd)
    };
    cmd.kill_on_drop(true);
    run_tool(program, RestoreStep::Load, cmd, on_event).await?;

    on_event(RestoreEvent::Step {
        step: RestoreStep::Check,
    });
    let client = postgres::connect_db(target, place, &req.name, opts.timeout)
        .await
        .map_err(RestoreError::Server)?;
    let facts = odoo_facts(&client)
        .await
        .map_err(|e| RestoreError::Server(pg_message(&e)))?;
    let Some(version) = facts.version else {
        return Err(RestoreError::NotOdoo);
    };
    if let Some(project) = &opts.series {
        let dump = series(&version);
        if &dump != project {
            return Err(RestoreError::VersionMismatch {
                dump,
                project: project.clone(),
            });
        }
    }

    if req.neutralize {
        on_event(RestoreEvent::Step {
            step: RestoreStep::Neutralize,
        });
        let cmd = odoo.command(&[
            "neutralize".into(),
            "-c".into(),
            odoo.conf.display().to_string(),
            "-d".into(),
            req.name.clone(),
        ]);
        run_tool(&odoo.python, RestoreStep::Neutralize, cmd, on_event).await?;
        if !is_neutralized(&client)
            .await
            .map_err(|e| RestoreError::Server(pg_message(&e)))?
        {
            return Err(RestoreError::NotNeutralized);
        }
    }
    drop(client);

    if let Some(src) = extracted.and_then(|x| x.filestore.as_deref()) {
        on_event(RestoreEvent::Step {
            step: RestoreStep::Filestore,
        });
        move_filestore(src, filestore_dest)?;
    }

    on_event(RestoreEvent::Step {
        step: RestoreStep::Finalize,
    });
    let mut cmd = odoo.command(&[
        "shell".into(),
        "-c".into(),
        odoo.conf.display().to_string(),
        "-d".into(),
        req.name.clone(),
        "--no-http".into(),
    ]);
    cmd.env("ODHIVE_DB_NEW_UUID", if req.copy { "1" } else { "0" });
    let mut ready = false;
    let done = proc::run_streaming_with_input(
        cmd,
        Some(FINALIZE_RESTORE_SCRIPT.as_bytes().to_vec()),
        TAIL_LINES,
        |line| {
            if line.trim() == READY_MARKER {
                ready = true;
            }
            on_event(RestoreEvent::Output { line });
        },
    )
    .await
    .map_err(|e| run_error(&odoo.python, RestoreStep::Finalize, e))?;
    if !done.status.success() || !ready {
        return Err(RestoreError::Failed {
            step: RestoreStep::Finalize,
            code: done.status.code(),
            tail: done.tail,
        });
    }
    Ok(Some(version))
}

/// Whether `database.is_neutralized` is set (Odoo reads any non-empty
/// value as set).
async fn is_neutralized(client: &tokio_postgres::Client) -> Result<bool, tokio_postgres::Error> {
    let table: bool = client
        .query_one(
            "SELECT pg_catalog.to_regclass('public.ir_config_parameter') IS NOT NULL",
            &[],
        )
        .await?
        .try_get(0)?;
    if !table {
        return Ok(false);
    }
    let value: Option<Option<String>> = client
        .query_opt(
            "SELECT value FROM public.ir_config_parameter WHERE key = $1",
            &[&NEUTRALIZED_KEY],
        )
        .await?
        .map(|r| r.try_get(0))
        .transpose()?;
    Ok(value.flatten().is_some_and(|v| !v.trim().is_empty()))
}

fn run_error(program: &Path, step: RestoreStep, e: RunError) -> RestoreError {
    match e {
        RunError::Spawn(source) => RestoreError::Spawn {
            program: program.to_path_buf(),
            source,
        },
        RunError::Io(e) => RestoreError::Failed {
            step,
            code: None,
            tail: e.to_string(),
        },
    }
}

async fn run_tool(
    program: &Path,
    step: RestoreStep,
    cmd: Command,
    on_event: &mut impl FnMut(RestoreEvent),
) -> Result<(), RestoreError> {
    let done = proc::run_streaming(cmd, TAIL_LINES, |line| {
        on_event(RestoreEvent::Output { line });
    })
    .await
    .map_err(|e| run_error(program, step, e))?;
    if done.status.success() {
        Ok(())
    } else {
        Err(RestoreError::Failed {
            step,
            code: done.status.code(),
            tail: done.tail,
        })
    }
}

/// Moves the unpacked filestore to `dest` (a rename on the same file
/// system, else a copy of the plain files and folders OdHive wrote).
fn move_filestore(src: &Path, dest: &Path) -> Result<(), RestoreError> {
    let io = |action, path: &Path| {
        let path = path.to_path_buf();
        move |source| RestoreError::Io {
            action,
            path,
            source,
        }
    };
    if let Some(parent) = dest.parent() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(io("cannot create", parent))?;
    }
    if dest.symlink_metadata().is_ok() {
        return Err(RestoreError::FilestoreExists(dest.to_path_buf()));
    }
    match fs::rename(src, dest) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(rustix::io::Errno::XDEV.raw_os_error()) => {
            copy_tree(src, dest).map_err(io("cannot copy the filestore to", dest))
        }
        Err(e) => Err(io("cannot move the filestore to", dest)(e)),
    }
}

fn copy_tree(src: &Path, dest: &Path) -> std::io::Result<()> {
    fs::DirBuilder::new().mode(0o700).create(dest)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let to = dest.join(entry.file_name());
        if kind.is_dir() {
            copy_tree(&entry.path(), &to)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn psql_versions_and_restrict_support_are_read() {
        assert_eq!(parse_psql_version("psql (PostgreSQL) 18.1"), Some((18, 1)));
        assert_eq!(
            parse_psql_version("psql (PostgreSQL) 17.6 (Ubuntu 17.6-1.pgdg24.04+1)"),
            Some((17, 6))
        );
        assert_eq!(
            parse_psql_version("psql (PostgreSQL) 19beta1"),
            Some((19, 0))
        );
        assert_eq!(parse_psql_version("psql"), None);
        for ok in [
            (18, 0),
            (19, 0),
            (17, 6),
            (16, 10),
            (15, 14),
            (14, 19),
            (13, 22),
        ] {
            assert!(restrict_supported(ok), "{ok:?}");
        }
        for old in [(17, 5), (16, 9), (15, 13), (14, 18), (13, 21), (12, 99)] {
            assert!(!restrict_supported(old), "{old:?}");
        }
    }

    #[test]
    fn tool_arguments_never_hold_the_connection_secrets() {
        assert_eq!(
            createdb_args("shop", "template0"),
            [
                "--encoding=UTF8",
                "--template=template0",
                "--lc-collate=C",
                "shop"
            ]
        );
        assert_eq!(
            createdb_args("shop", "mytpl"),
            ["--encoding=UTF8", "--template=mytpl", "shop"]
        );
        let a = psql_args("shop", "k3y", Path::new("/s/dump.sql"));
        assert_eq!(
            a,
            [
                "-X",
                "-q",
                "--output=/dev/null",
                "--dbname=shop",
                "-c",
                "\\restrict k3y",
                "-f",
                "/s/dump.sql"
            ]
            .map(OsString::from)
        );
        assert_eq!(
            pg_restore_args("shop", Path::new("/s/x.dump")),
            ["--no-owner", "--dbname=shop", "/s/x.dump"].map(OsString::from)
        );
        let target = PgTarget {
            user: Some("odoo".into()),
            password: Some("s3cret".into()),
            ..PgTarget::nowhere()
        };
        let env = pg_env(&target, &Place::Tcp("db.local".into()));
        let get = |k| env.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone());
        assert_eq!(get("PGHOST"), Some("db.local".into()));
        assert_eq!(get("PGPORT"), Some("5432".into()));
        assert_eq!(get("PGUSER"), Some("odoo".into()));
        assert_eq!(get("PGPASSWORD"), Some("s3cret".into()));
        assert_eq!(get("PGSSLMODE"), Some("prefer".into()));
        assert_ne!(random_key(), random_key());
        assert_eq!(random_key().len(), 32);
    }

    #[test]
    fn options_name_the_tools_folder_and_template() {
        let o = |pairs: &[(&str, &str)]| {
            OdooOptions(
                pairs
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect(),
            )
        };
        assert_eq!(pg_path_option(&o(&[])), None);
        assert_eq!(pg_path_option(&o(&[("pg_path", "None")])), None);
        assert_eq!(
            pg_path_option(&o(&[("pg_path", "/usr/lib/postgresql/17/bin")])),
            Some(PathBuf::from("/usr/lib/postgresql/17/bin"))
        );
        assert_eq!(template_option(&o(&[])), "template0");
        assert_eq!(template_option(&o(&[("db_template", "tpl")])), "tpl");
    }
}
