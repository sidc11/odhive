//! Snapshots: instant restore points of a project's databases.
//!
//! A snapshot of database `<db>` is a PostgreSQL copy of it named
//! `<db>__snap_<YYYYMMDDHHMMSS>` (UTC) plus a copy of its filestore. The
//! copy is made with `createdb --template=<db>` (PostgreSQL copies the
//! database's files, much faster than a dump), then marked a template that
//! nobody may connect to: Odoo and OdHive's database list leave templates
//! out, and nothing can change it. The filestore goes to
//! `backups/snapshots/<snapshot>/filestore`, as hard links where possible:
//! Odoo never rewrites a filestore file (a changed attachment is a new
//! file, an old one is unlinked), so a link stays the file as it was, and
//! takes no space. Files that cannot be linked (another file system) are
//! copied. Links and special files inside a filestore are skipped, never
//! followed.
//!
//! The folder `backups/snapshots/<snapshot>/` is also what makes a snapshot
//! the project's: [`Snapshots::list`], [`Snapshots::restore`] and
//! [`Snapshots::delete`] only use snapshots that have one, so a project
//! never touches another project's snapshots on a shared server.
//!
//! Restoring copies the snapshot to `<db>__restoring` and the filestore
//! to a staging folder first; only then is `<db>` dropped (other
//! connections ended) and the copy renamed to `<db>`, and the filestore
//! put in place. The snapshot stays, to restore again.
//!
//! Every step runs a PostgreSQL client tool with an argument vector (never
//! a shell); names reach SQL only as `psql` variables quoted by `psql`
//! (`:"name"`) or as query parameters.

use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::process::Command;
use tokio_postgres::Client;

use super::PgTarget;
use super::backup::civil_from_days;
use super::create::TAIL_LINES;
use super::list::pg_message;
use super::restore::{PgTools, pg_command_in, pg_env};
use crate::envcheck::postgres::{self, Connection, Place};
use crate::names::{self, MAX_DB_NAME_LEN, NameError};
use crate::proc::{self, RunError};
use crate::project::EnvVars;

/// Folder of the snapshots inside the project's `backups/`.
pub const SNAPSHOTS_DIR: &str = "snapshots";

/// Between the database name and the time in a snapshot name.
const INFIX: &str = "__snap_";

/// `YYYYMMDDHHMMSS`.
const STAMP_LEN: usize = 14;

/// Longest name of a database that can be snapshotted: the snapshot name
/// must fit PostgreSQL's 63 bytes.
pub const MAX_SNAPSHOT_SOURCE_LEN: usize = MAX_DB_NAME_LEN - INFIX.len() - STAMP_LEN;

/// Suffix of the copy a restore makes before swapping it in.
pub const RESTORING_SUFFIX: &str = "__restoring";

/// Prefix of the staging folders (never listed).
const STAGING_PREFIX: &str = ".odhive-snap-";

/// `createdb --template` attempts while other sessions use the source.
const BUSY_TRIES: u32 = 5;

/// Folders deeper than this inside a filestore are skipped (Odoo uses two
/// levels).
const MAX_DEPTH: usize = 16;

/// Marks a new snapshot read-only and hidden.
const MARK_SCRIPT: &str =
    "ALTER DATABASE :\"name\" WITH IS_TEMPLATE true ALLOW_CONNECTIONS false;\n";

/// Drops a database, a snapshot (template) or not.
const DROP_SCRIPT: &str =
    "ALTER DATABASE :\"name\" WITH IS_TEMPLATE false;\nDROP DATABASE :\"name\" WITH (FORCE);\n";

/// Replaces `db` by the restored copy `tmp`.
const SWAP_SCRIPT: &str =
    "DROP DATABASE IF EXISTS :\"db\" WITH (FORCE);\nALTER DATABASE :\"tmp\" RENAME TO :\"db\";\n";

/// A snapshot of the project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Snapshot {
    /// Snapshot name, `<database>__snap_<YYYYMMDDHHMMSS>`.
    pub name: String,
    /// The database it was taken of.
    pub database: String,
    /// When it was taken, milliseconds since the Unix epoch.
    pub created_at: u64,
    /// Size of its PostgreSQL copy in bytes, when known.
    pub size: Option<u64>,
    /// Whether it holds a filestore.
    pub filestore: bool,
    /// The PostgreSQL copy is gone (dropped outside OdHive): it cannot be
    /// restored, only deleted.
    pub broken: bool,
}

/// What [`Snapshots::restore`] did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RestoredSnapshot {
    /// The database, as it was when the snapshot was taken.
    pub database: String,
    /// The snapshot restored (kept).
    pub snapshot: String,
    /// Whether a filestore was put back.
    pub filestore: bool,
}

/// A step of taking or restoring a snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotStep {
    /// Stopping the instances on the database (reported by the caller).
    StopInstances,
    /// Copying the database.
    Copy,
    /// Copying the filestore.
    Filestore,
    /// Putting the restored copy in place of the database.
    Swap,
    /// Removing what a failed step left.
    Cleanup,
    /// Starting the stopped instances again (reported by the caller).
    RestartInstances,
}

impl fmt::Display for SnapshotStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::StopInstances => "stopping the instances on the database",
            Self::Copy => "copying the database",
            Self::Filestore => "copying the filestore",
            Self::Swap => "replacing the database",
            Self::Cleanup => "removing the half-made copy",
            Self::RestartInstances => "starting the instances again",
        })
    }
}

/// Progress of taking or restoring a snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum SnapshotEvent {
    /// A step started.
    Step {
        /// The step.
        step: SnapshotStep,
    },
    /// A line of tool output.
    Output {
        /// The line.
        line: String,
    },
}

/// Errors from snapshots.
#[derive(Debug, Error)]
pub enum SnapshotError {
    /// A database name is invalid.
    #[error(transparent)]
    Name(#[from] NameError),
    /// Not a snapshot name.
    #[error("{0:?} is not a snapshot name (<database>__snap_<YYYYMMDDHHMMSS>)")]
    InvalidName(String),
    /// The database name is too long for a snapshot name.
    #[error(
        "database name {0:?} is too long to snapshot (at most {MAX_SNAPSHOT_SOURCE_LEN} characters)"
    )]
    TooLong(String),
    /// No database has this name (or the project does not list it).
    #[error("no database named {0:?}")]
    NotFound(String),
    /// The project has no such snapshot, or its PostgreSQL copy is gone.
    #[error("no snapshot named {0:?}")]
    SnapshotNotFound(String),
    /// A database or snapshot with this name exists.
    #[error("{0:?} already exists; try again in a second, or drop it first")]
    Exists(String),
    /// Instances of the project run on the database.
    #[error("instance(s) {} run on database {name:?}; they must be stopped first", instances.join(", "))]
    InUse {
        /// Database name.
        name: String,
        /// Ids of the running instances.
        instances: Vec<String>,
    },
    /// The PostgreSQL server cannot be used.
    #[error("PostgreSQL: {0}")]
    Server(String),
    /// A tool could not be started.
    #[error("could not run {program}: {source}")]
    Spawn {
        /// Program.
        program: PathBuf,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
    /// A step failed; what it made was removed.
    #[error("{step} failed (exit code {code:?}). Last output:\n{tail}")]
    Failed {
        /// The step.
        step: SnapshotStep,
        /// Exit code, if a tool exited normally.
        code: Option<i32>,
        /// Last output lines, or what went wrong.
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
        source: io::Error,
    },
}

fn io_err(action: &'static str, path: &Path) -> impl FnOnce(io::Error) -> SnapshotError {
    let path = path.to_path_buf();
    move |source| SnapshotError::Io {
        action,
        path,
        source,
    }
}

/// `<database>__snap_<YYYYMMDDHHMMSS>` for a snapshot taken `unix_secs`
/// seconds after the epoch (UTC).
#[must_use]
pub fn snapshot_name(database: &str, unix_secs: u64) -> String {
    let days = i64::try_from(unix_secs / 86_400).unwrap_or(i64::MAX);
    let (y, m, d) = civil_from_days(days);
    let rest = unix_secs % 86_400;
    format!(
        "{database}{INFIX}{y:04}{m:02}{d:02}{:02}{:02}{:02}",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// The database a snapshot name was taken of, and when (seconds since the
/// epoch). Only names [`snapshot_name`] makes are accepted.
///
/// # Errors
/// [`SnapshotError::InvalidName`].
pub fn parse_snapshot_name(name: &str) -> Result<(String, u64), SnapshotError> {
    let invalid = || SnapshotError::InvalidName(name.to_owned());
    let (db, stamp) = name.rsplit_once(INFIX).ok_or_else(invalid)?;
    if stamp.len() != STAMP_LEN
        || !stamp.bytes().all(|b| b.is_ascii_digit())
        || db.len() > MAX_SNAPSHOT_SOURCE_LEN
        || names::validate_db_name(db).is_err()
    {
        return Err(invalid());
    }
    let num = |r: std::ops::Range<usize>| stamp[r].parse::<u32>().map_err(|_| invalid());
    let (y, mo, d, h, mi, s) = (
        num(0..4)?,
        num(4..6)?,
        num(6..8)?,
        num(8..10)?,
        num(10..12)?,
        num(12..14)?,
    );
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || s > 59 {
        return Err(invalid());
    }
    let days = days_from_civil(i64::from(y), mo, d);
    let secs = u64::try_from(days)
        .ok()
        .map(|days| days * 86_400 + u64::from(h * 3600 + mi * 60 + s))
        .ok_or_else(invalid)?;
    // A date like 31 February would come back as another day.
    if snapshot_name(db, secs) != name {
        return Err(invalid());
    }
    Ok((db.to_owned(), secs))
}

/// (year, month, day) → days since 1970-01-01, proleptic Gregorian.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Where a project's snapshots live and how to reach its server.
#[derive(Debug, Clone)]
pub struct Snapshots {
    /// The project's PostgreSQL server.
    pub target: PgTarget,
    /// `createdb` and `psql`.
    pub tools: PgTools,
    /// Working folder of the tools (the project folder).
    pub cwd: PathBuf,
    /// The project's `[env]`.
    pub env: EnvVars,
    /// The project's `backups/` (snapshots go in `backups/snapshots/`).
    pub backups_dir: PathBuf,
    /// The project's `data/filestore`.
    pub filestore_root: PathBuf,
    /// Connection timeout.
    pub timeout: Duration,
}

impl Snapshots {
    /// `backups/snapshots`.
    #[must_use]
    pub fn dir(&self) -> PathBuf {
        self.backups_dir.join(SNAPSHOTS_DIR)
    }

    async fn connect(&self) -> Result<Connection, SnapshotError> {
        postgres::connect(&self.target, self.timeout)
            .await
            .map_err(|e| {
                SnapshotError::Server(postgres::failure_check(&self.target, e, None, None).summary)
            })
    }

    fn tool(&self, program: &Path, place: &Place) -> Command {
        let env: Vec<(&'static str, OsString)> = pg_env(&self.target, place);
        pg_command_in(&self.cwd, &self.env, program, &env)
    }

    /// `psql` running `script` (from stdin) on the maintenance database,
    /// with `vars` as `psql` variables.
    async fn psql(
        &self,
        conn: &ConnInfo,
        step: SnapshotStep,
        script: &str,
        vars: &[(&str, &str)],
        on_event: &mut impl FnMut(SnapshotEvent),
    ) -> Result<(), SnapshotError> {
        let mut cmd = self.tool(&self.tools.psql, &conn.place);
        cmd.args(["-X", "-q", "-v", "ON_ERROR_STOP=1"])
            .arg(format!("--dbname={}", conn.database));
        for (k, v) in vars {
            cmd.arg("-v").arg(format!("{k}={v}"));
        }
        cmd.args(["-f", "-"]);
        run(
            &self.tools.psql,
            step,
            cmd,
            Some(script.as_bytes().to_vec()),
            on_event,
        )
        .await
    }

    /// Copies `source` to `new` with `createdb --template`, ending other
    /// sessions on `source` first and trying again while it is busy.
    async fn clone_db(
        &self,
        conn: &ConnInfo,
        client: &Client,
        source: &str,
        new: &str,
        on_event: &mut impl FnMut(SnapshotEvent),
    ) -> Result<(), SnapshotError> {
        for attempt in 1..=BUSY_TRIES {
            terminate(client, source, on_event).await;
            let mut cmd = self.tool(&self.tools.createdb, &conn.place);
            // Both names passed the checks: neither starts with `-`.
            cmd.arg(format!("--template={source}")).arg(new);
            match run(
                &self.tools.createdb,
                SnapshotStep::Copy,
                cmd,
                None,
                on_event,
            )
            .await
            {
                Err(SnapshotError::Failed { tail, .. })
                    if attempt < BUSY_TRIES && tail.contains("being accessed by other users") =>
                {
                    on_event(SnapshotEvent::Output {
                        line: format!("{source} is in use; trying again ({attempt}/{BUSY_TRIES})"),
                    });
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
                other => return other,
            }
        }
        unreachable!("the last attempt returns")
    }

    /// Drops `name` (a snapshot or a half-made copy) when it exists.
    async fn drop_db(
        &self,
        conn: &ConnInfo,
        client: &Client,
        name: &str,
        on_event: &mut impl FnMut(SnapshotEvent),
    ) -> Result<(), SnapshotError> {
        if !db_exists(client, name).await? {
            return Ok(());
        }
        self.psql(
            conn,
            SnapshotStep::Cleanup,
            DROP_SCRIPT,
            &[("name", name)],
            on_event,
        )
        .await
    }

    /// Takes a snapshot of `database` (see the module docs). Other
    /// sessions on it are ended: PostgreSQL copies only a database nobody
    /// uses. Reports progress to `on_event`.
    ///
    /// On failure, and when the returned future is dropped, the copy is
    /// removed again; `database` is never changed.
    ///
    /// # Errors
    /// See [`SnapshotError`].
    pub async fn take(
        &self,
        database: &str,
        mut on_event: impl FnMut(SnapshotEvent),
    ) -> Result<Snapshot, SnapshotError> {
        names::validate_db_name(database)?;
        if database.len() > MAX_SNAPSHOT_SOURCE_LEN {
            return Err(SnapshotError::TooLong(database.to_owned()));
        }
        let conn = self.connect().await?;
        if !db_exists(&conn.client, database).await? {
            return Err(SnapshotError::NotFound(database.to_owned()));
        }
        let dir = self.dir();
        // Names go by the second: one taken this second waits for the next.
        let mut tries = 0;
        let (secs, name, dest) = loop {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            let name = snapshot_name(database, now.as_secs());
            let dest = dir.join(&name);
            if !db_exists(&conn.client, &name).await? && dest.symlink_metadata().is_err() {
                break (now.as_secs(), name, dest);
            }
            tries += 1;
            if tries == 2 {
                return Err(SnapshotError::Exists(name));
            }
            let wait = Duration::from_secs(1)
                .saturating_sub(Duration::from_nanos(u64::from(now.subsec_nanos())));
            tokio::time::sleep(wait + Duration::from_millis(10)).await;
        };
        ensure_dir(&self.backups_dir)?;
        ensure_dir(&dir)?;
        // Removed when this function returns or its future is dropped.
        let staging = tempfile::Builder::new()
            .prefix(STAGING_PREFIX)
            .tempdir_in(&dir)
            .map_err(io_err("cannot create a staging folder in", &dir))?;
        let info = ConnInfo::of(&conn);

        let mut guard = DropOnCancel(Some((self.clone(), info.clone(), name.clone())));
        let outcome = self
            .take_steps(
                &info,
                &conn.client,
                database,
                &name,
                staging.path(),
                &mut on_event,
            )
            .await;
        guard.0 = None;
        let filestore = match outcome {
            Ok(filestore) => filestore,
            Err(e) => {
                on_event(SnapshotEvent::Step {
                    step: SnapshotStep::Cleanup,
                });
                if let Err(why) = self
                    .drop_db(&info, &conn.client, &name, &mut on_event)
                    .await
                {
                    tracing::warn!(snapshot = %name, "could not remove the half-made snapshot: {why}");
                }
                return Err(e);
            }
        };
        let kept = staging.keep();
        if let Err(e) = fs::rename(&kept, &dest) {
            let _ = fs::remove_dir_all(&kept);
            let _ = self
                .drop_db(&info, &conn.client, &name, &mut on_event)
                .await;
            return Err(io_err("cannot move the snapshot to", &dest)(e));
        }
        Ok(Snapshot {
            size: db_size(&conn.client, &name).await,
            name,
            database: database.to_owned(),
            created_at: secs * 1000,
            filestore,
            broken: false,
        })
    }

    /// Copy (marked a hidden template), then the filestore into
    /// `staging/filestore`. Whether there was a filestore.
    async fn take_steps(
        &self,
        info: &ConnInfo,
        client: &Client,
        database: &str,
        name: &str,
        staging: &Path,
        on_event: &mut impl FnMut(SnapshotEvent),
    ) -> Result<bool, SnapshotError> {
        on_event(SnapshotEvent::Step {
            step: SnapshotStep::Copy,
        });
        self.clone_db(info, client, database, name, on_event)
            .await?;
        self.psql(
            info,
            SnapshotStep::Copy,
            MARK_SCRIPT,
            &[("name", name)],
            on_event,
        )
        .await?;
        // The project's filestore may itself be a link (followed, as Odoo
        // does); nothing inside it is followed.
        let source = self.filestore_root.join(database);
        if !source.is_dir() {
            return Ok(false);
        }
        on_event(SnapshotEvent::Step {
            step: SnapshotStep::Filestore,
        });
        let stats = copy_tree_blocking(source, staging.join("filestore")).await?;
        on_event(SnapshotEvent::Output {
            line: stats.summary(),
        });
        Ok(true)
    }

    /// Puts database `<db>` back as it was when `snapshot` was taken, with
    /// its filestore (see the module docs). Other sessions on `<db>` are
    /// ended. `<db>` may be missing (deleted since). Reports progress to
    /// `on_event`.
    ///
    /// Until the swap the database is untouched; on failure, and when the
    /// returned future is dropped, the copy is removed again. The snapshot
    /// is never changed.
    ///
    /// # Errors
    /// See [`SnapshotError`].
    pub async fn restore(
        &self,
        snapshot: &str,
        mut on_event: impl FnMut(SnapshotEvent),
    ) -> Result<RestoredSnapshot, SnapshotError> {
        let (database, _) = parse_snapshot_name(snapshot)?;
        let snap_dir = self.dir().join(snapshot);
        if !is_real_dir(&snap_dir) {
            return Err(SnapshotError::SnapshotNotFound(snapshot.to_owned()));
        }
        let conn = self.connect().await?;
        if !db_exists(&conn.client, snapshot).await? {
            return Err(SnapshotError::SnapshotNotFound(snapshot.to_owned()));
        }
        let tmp = format!("{database}{RESTORING_SUFFIX}");
        if db_exists(&conn.client, &tmp).await? {
            return Err(SnapshotError::Exists(tmp));
        }
        ensure_dir(&self.filestore_root)?;
        let staging = tempfile::Builder::new()
            .prefix(STAGING_PREFIX)
            .tempdir_in(&self.filestore_root)
            .map_err(io_err(
                "cannot create a staging folder in",
                &self.filestore_root,
            ))?;
        let info = ConnInfo::of(&conn);

        let mut guard = DropOnCancel(Some((self.clone(), info.clone(), tmp.clone())));
        let outcome = self
            .restore_steps(
                &info,
                &conn.client,
                snapshot,
                &database,
                &tmp,
                &snap_dir.join("filestore"),
                staging.path(),
                &mut on_event,
            )
            .await;
        guard.0 = None;
        match outcome {
            Ok(filestore) => Ok(RestoredSnapshot {
                database,
                snapshot: snapshot.to_owned(),
                filestore,
            }),
            Err(e) => {
                on_event(SnapshotEvent::Step {
                    step: SnapshotStep::Cleanup,
                });
                if let Err(why) = self.drop_db(&info, &conn.client, &tmp, &mut on_event).await {
                    tracing::warn!(db = %tmp, "could not remove the half-made copy: {why}");
                }
                Err(e)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn restore_steps(
        &self,
        info: &ConnInfo,
        client: &Client,
        snapshot: &str,
        database: &str,
        tmp: &str,
        snap_fs: &Path,
        staging: &Path,
        on_event: &mut impl FnMut(SnapshotEvent),
    ) -> Result<bool, SnapshotError> {
        on_event(SnapshotEvent::Step {
            step: SnapshotStep::Copy,
        });
        self.clone_db(info, client, snapshot, tmp, on_event).await?;
        let new_fs = staging.join("new");
        let has_fs = is_real_dir(snap_fs);
        if has_fs {
            on_event(SnapshotEvent::Step {
                step: SnapshotStep::Filestore,
            });
            let stats = copy_tree_blocking(snap_fs.to_path_buf(), new_fs.clone()).await?;
            on_event(SnapshotEvent::Output {
                line: stats.summary(),
            });
        }

        on_event(SnapshotEvent::Step {
            step: SnapshotStep::Swap,
        });
        terminate(client, database, on_event).await;
        self.psql(
            info,
            SnapshotStep::Swap,
            SWAP_SCRIPT,
            &[("db", database), ("tmp", tmp)],
            on_event,
        )
        .await?;
        if !db_exists(client, database).await? {
            return Err(SnapshotError::Failed {
                step: SnapshotStep::Swap,
                code: Some(0),
                tail: format!("{database} is missing after the swap"),
            });
        }
        let current = self.filestore_root.join(database);
        if current.symlink_metadata().is_ok() {
            let old = staging.join("old");
            fs::rename(&current, &old).map_err(io_err("cannot move away", &current))?;
        }
        if has_fs {
            fs::rename(&new_fs, &current).map_err(io_err("cannot put in place", &current))?;
        }
        Ok(has_fs)
    }

    /// Deletes `snapshot`: its PostgreSQL copy (when still there) and its
    /// folder.
    ///
    /// # Errors
    /// [`SnapshotError::SnapshotNotFound`] when the project has no such
    /// snapshot; see [`SnapshotError`].
    pub async fn delete(&self, snapshot: &str) -> Result<(), SnapshotError> {
        parse_snapshot_name(snapshot)?;
        let dir = self.dir().join(snapshot);
        if dir.symlink_metadata().is_err() {
            return Err(SnapshotError::SnapshotNotFound(snapshot.to_owned()));
        }
        let conn = self.connect().await?;
        let info = ConnInfo::of(&conn);
        // A failure carries the output's tail.
        self.drop_db(&info, &conn.client, snapshot, &mut |_| {})
            .await?;
        remove_path(&dir).map_err(io_err("cannot remove", &dir))
    }

    /// The project's snapshots, newest first. A missing folder has none.
    ///
    /// # Errors
    /// [`SnapshotError::Server`] when the server cannot be used,
    /// [`SnapshotError::Io`] when the folder cannot be read.
    pub async fn list(&self) -> Result<Vec<Snapshot>, SnapshotError> {
        let dir = self.dir();
        let found = snapshot_folders(&dir)?;
        if found.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.connect().await?;
        let names: Vec<&str> = found.iter().map(|(n, ..)| n.as_str()).collect();
        let rows = conn
            .client
            .query(
                "SELECT datname::text, CASE WHEN pg_catalog.has_database_privilege(oid, 'CONNECT') \
                     THEN pg_catalog.pg_database_size(oid) END \
                 FROM pg_catalog.pg_database WHERE datname = ANY($1)",
                &[&names],
            )
            .await
            .map_err(|e| SnapshotError::Server(pg_message(&e)))?;
        let mut sizes = std::collections::BTreeMap::new();
        for row in rows {
            let name: String = row.get(0);
            let size: Option<i64> = row.get(1);
            sizes.insert(name, size.and_then(|s| u64::try_from(s).ok()));
        }
        let mut out: Vec<Snapshot> = found
            .into_iter()
            .map(|(name, database, secs, filestore)| Snapshot {
                size: sizes.get(&name).copied().flatten(),
                broken: !sizes.contains_key(&name),
                name,
                database,
                created_at: secs * 1000,
                filestore,
            })
            .collect();
        out.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok(out)
    }
}

/// Where a connection reached the server, for the tools.
#[derive(Debug, Clone)]
struct ConnInfo {
    place: Place,
    database: &'static str,
}

impl ConnInfo {
    fn of(conn: &Connection) -> Self {
        Self {
            place: conn.place.clone(),
            database: conn.database,
        }
    }
}

/// Drops a half-made copy in a background task when dropped armed (the
/// job was cancelled).
struct DropOnCancel(Option<(Snapshots, ConnInfo, String)>);

impl Drop for DropOnCancel {
    fn drop(&mut self) {
        let Some((snaps, info, name)) = self.0.take() else {
            return;
        };
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        rt.spawn(async move {
            let result = async {
                let conn = snaps.connect().await?;
                snaps.drop_db(&info, &conn.client, &name, &mut |_| {}).await
            }
            .await;
            if let Err(why) = result {
                tracing::warn!(db = %name, "could not remove the half-made copy: {why}");
            }
        });
    }
}

async fn run(
    program: &Path,
    step: SnapshotStep,
    cmd: Command,
    input: Option<Vec<u8>>,
    on_event: &mut impl FnMut(SnapshotEvent),
) -> Result<(), SnapshotError> {
    let done = proc::run_streaming_with_input(cmd, input, TAIL_LINES, |line| {
        on_event(SnapshotEvent::Output { line });
    })
    .await
    .map_err(|e| match e {
        RunError::Spawn(source) => SnapshotError::Spawn {
            program: program.to_path_buf(),
            source,
        },
        RunError::Io(e) => SnapshotError::Failed {
            step,
            code: None,
            tail: e.to_string(),
        },
    })?;
    if done.status.success() {
        Ok(())
    } else {
        Err(SnapshotError::Failed {
            step,
            code: done.status.code(),
            tail: done.tail,
        })
    }
}

/// Ends the other sessions on `db`, as Odoo does before copying. A role
/// that may not end some of them gets a warning line; the copy then says
/// the database is busy.
async fn terminate(client: &Client, db: &str, on_event: &mut impl FnMut(SnapshotEvent)) {
    let ended = client
        .query_one(
            "SELECT count(pg_catalog.pg_terminate_backend(pid)) FROM pg_catalog.pg_stat_activity \
             WHERE datname = $1 AND pid <> pg_catalog.pg_backend_pid()",
            &[&db],
        )
        .await
        .and_then(|r| r.try_get::<_, i64>(0));
    match ended {
        Ok(0) => {}
        Ok(n) => on_event(SnapshotEvent::Output {
            line: format!("ended {n} other session(s) on {db}"),
        }),
        Err(e) => on_event(SnapshotEvent::Output {
            line: format!(
                "could not end the other sessions on {db}: {}",
                pg_message(&e)
            ),
        }),
    }
}

async fn db_exists(client: &Client, name: &str) -> Result<bool, SnapshotError> {
    client
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_database WHERE datname = $1)",
            &[&name],
        )
        .await
        .and_then(|r| r.try_get(0))
        .map_err(|e| SnapshotError::Server(pg_message(&e)))
}

async fn db_size(client: &Client, name: &str) -> Option<u64> {
    client
        .query_one(
            "SELECT pg_catalog.pg_database_size(datname) FROM pg_catalog.pg_database WHERE datname = $1",
            &[&name],
        )
        .await
        .and_then(|r| r.try_get::<_, i64>(0))
        .ok()
        .and_then(|s| u64::try_from(s).ok())
}

/// The snapshot folders in `dir`: (name, database, seconds, has filestore).
/// Links, files, staging folders and other names are left out.
fn snapshot_folders(dir: &Path) -> Result<Vec<(String, String, u64, bool)>, SnapshotError> {
    if !is_real_dir(dir) {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).map_err(io_err("cannot read", dir))? {
        let entry = entry.map_err(io_err("cannot read", dir))?;
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok((database, secs)) = parse_snapshot_name(&name) else {
            continue;
        };
        let filestore = is_real_dir(&entry.path().join("filestore"));
        out.push((name, database, secs, filestore));
    }
    Ok(out)
}

/// A folder, not a link to one.
fn is_real_dir(path: &Path) -> bool {
    path.symlink_metadata().is_ok_and(|m| m.is_dir())
}

/// Creates `dir` (`0700`) when missing; refuses a link or a file there.
fn ensure_dir(dir: &Path) -> Result<(), SnapshotError> {
    match dir.symlink_metadata() {
        Ok(m) if m.is_dir() => Ok(()),
        Ok(_) => Err(io_err("not a folder:", dir)(io::Error::from(
            io::ErrorKind::NotADirectory,
        ))),
        Err(_) => fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(io_err("cannot create", dir)),
    }
}

/// Removes `path`: a folder with its contents (links inside removed, never
/// followed), or a file or link.
fn remove_path(path: &Path) -> io::Result<()> {
    match path.symlink_metadata() {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
        Ok(m) if m.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
    }
}

/// What [`copy_tree`] did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct TreeCopy {
    linked: u64,
    copied: u64,
    skipped: u64,
}

impl TreeCopy {
    fn summary(&self) -> String {
        let mut line = format!(
            "filestore: {} file(s) hard-linked, {} copied",
            self.linked, self.copied
        );
        if self.skipped > 0 {
            line.push_str(&format!(
                ", {} link(s), special file(s) or too-deep folder(s) skipped",
                self.skipped
            ));
        }
        line
    }
}

/// Sets the flag when dropped, so a blocking copy stops.
struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

async fn copy_tree_blocking(src: PathBuf, dst: PathBuf) -> Result<TreeCopy, SnapshotError> {
    let cancel = Arc::new(AtomicBool::new(false));
    let _stop = CancelOnDrop(cancel.clone());
    let at = dst.clone();
    tokio::task::spawn_blocking(move || {
        let mut stats = TreeCopy::default();
        copy_tree(&src, &dst, 0, &cancel, &mut stats).map(|()| stats)
    })
    .await
    .map_err(|e| io_err("cannot copy the filestore to", &at)(io::Error::other(e)))?
    .map_err(io_err("cannot copy the filestore to", &at))
}

/// Recreates `src` at `dst` (a new `0700` folder): plain files hard-linked,
/// else copied (`0600`); folders recreated; links and special files
/// skipped, never followed.
fn copy_tree(
    src: &Path,
    dst: &Path,
    depth: usize,
    cancel: &AtomicBool,
    stats: &mut TreeCopy,
) -> io::Result<()> {
    fs::DirBuilder::new().mode(0o700).create(dst)?;
    for entry in fs::read_dir(src)? {
        if cancel.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        // `file_type` does not follow links.
        let kind = entry.file_type()?;
        if kind.is_dir() && depth < MAX_DEPTH {
            copy_tree(&from, &to, depth + 1, cancel, stats)?;
        } else if kind.is_file() {
            if fs::hard_link(&from, &to).is_ok() {
                stats.linked += 1;
            } else {
                copy_file(&from, &to)?;
                stats.copied += 1;
            }
        } else {
            stats.skipped += 1;
        }
    }
    Ok(())
}

/// Copies a plain file without following a link at `from`, into a new
/// `0600` file.
fn copy_file(from: &Path, to: &Path) -> io::Result<()> {
    use rustix::fs::{Mode, OFlags};
    // NOFOLLOW: a link is refused; NONBLOCK: a FIFO does not hang the open.
    let fd = rustix::fs::open(
        from,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let mut src = fs::File::from(fd);
    if !src.metadata()?.is_file() {
        return Err(io::Error::other(format!(
            "{} is not a plain file",
            from.display()
        )));
    }
    let mut dst = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(to)?;
    io::copy(&mut src, &mut dst)?;
    Ok(())
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        // 2026-11-26 09:05:03 UTC.
        let secs = 1_795_683_903;
        let name = snapshot_name("shop_main", secs);
        assert_eq!(name, "shop_main__snap_20261126090503");
        assert_eq!(
            parse_snapshot_name(&name).expect("parse"),
            ("shop_main".to_owned(), secs)
        );
        let leap = snapshot_name("a.b-c", 951_782_400); // 2000-02-29
        assert_eq!(leap, "a.b-c__snap_20000229000000");
        assert_eq!(parse_snapshot_name(&leap).expect("parse").1, 951_782_400);
        let longest = "d".repeat(MAX_SNAPSHOT_SOURCE_LEN);
        assert_eq!(snapshot_name(&longest, secs).len(), MAX_DB_NAME_LEN);
    }

    #[test]
    fn only_snapshot_names_parse() {
        for bad in [
            "shop",
            "shop__snap_",
            "shop__snap_2026112609050",
            "shop__snap_202611260905031",
            "shop__snap_2026112609050x",
            "shop__snap_20260231000000", // 31 February
            "shop__snap_20261326000000",
            "shop__snap_20261126240000",
            "__snap_20261126090503",
            "a b__snap_20261126090503",
            "x;DROP__snap_20261126090503",
            "-x__snap_20261126090503",
            "../x__snap_20261126090503",
            &format!("{}__snap_20261126090503", "d".repeat(43)),
        ] {
            assert!(parse_snapshot_name(bad).is_err(), "{bad:?} accepted");
        }
    }

    #[test]
    fn a_tree_is_linked_without_following_links() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outside = dir.path().join("outside");
        fs::create_dir(&outside).expect("mkdir");
        fs::write(outside.join("secret"), "x").expect("write");
        let src = dir.path().join("src");
        fs::create_dir_all(src.join("ab")).expect("mkdir");
        fs::write(src.join("ab/cafe"), "attachment").expect("write");
        fs::write(src.join("top"), "t").expect("write");
        std::os::unix::fs::symlink(&outside, src.join("dir-link")).expect("symlink");
        std::os::unix::fs::symlink(outside.join("secret"), src.join("ab/file-link"))
            .expect("symlink");

        let dst = dir.path().join("dst");
        let mut stats = TreeCopy::default();
        copy_tree(&src, &dst, 0, &AtomicBool::new(false), &mut stats).expect("copy");
        assert_eq!(stats.linked, 2);
        assert_eq!(stats.skipped, 2);
        assert_eq!(
            fs::read_to_string(dst.join("ab/cafe")).expect("read"),
            "attachment"
        );
        assert!(dst.join("dir-link").symlink_metadata().is_err());
        assert!(dst.join("ab/file-link").symlink_metadata().is_err());

        // A cancelled copy stops.
        let err = copy_tree(
            &src,
            &dir.path().join("dst2"),
            0,
            &AtomicBool::new(true),
            &mut stats,
        )
        .expect_err("cancelled");
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
    }

    #[test]
    fn copies_refuse_links() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("real"), "x").expect("write");
        std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("link"))
            .expect("symlink");
        assert!(copy_file(&dir.path().join("link"), &dir.path().join("out")).is_err());
        copy_file(&dir.path().join("real"), &dir.path().join("out")).expect("copy");
        assert_eq!(
            fs::read_to_string(dir.path().join("out")).expect("read"),
            "x"
        );
    }
}
