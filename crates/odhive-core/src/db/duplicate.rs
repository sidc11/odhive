//! Duplicating an Odoo database the way Odoo's own database manager does.
//!
//! [`duplicate`] runs `odoo-bin db -c <conf> duplicate [-n] <source> <new>`
//! (never through a shell): Odoo ends the other connections to the source,
//! creates the copy with `CREATE DATABASE … TEMPLATE <source>`, neutralizes
//! it when asked, gives it a new database UUID and copies the source's
//! filestore. OdHive then checks the copy: it exists, its UUID differs from
//! the source's, it is neutralized when asked, and its filestore is there
//! when the source has one. A failed or cancelled duplicate removes the
//! copy again.
//!
//! Which databases may be copied (the project lists the source, no
//! instance runs on it) is the caller's business.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_postgres::Client;

use super::PgTarget;
use super::create::{
    CreateError, DropGuard, OdooCommand, TAIL_LINES, drop_database, validate_new_db_name,
};
use super::list::pg_message;
use super::restore::NEUTRALIZED_KEY;
use crate::envcheck::postgres;
use crate::names::{self, NameError};
use crate::proc::{self, RunError};

/// Odoo's `ir.config_parameter` key holding the database UUID.
pub const UUID_KEY: &str = "database.uuid";

/// What to duplicate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateRequest {
    /// The database to copy.
    pub source: String,
    /// Name of the copy.
    pub name: String,
    /// Run Odoo's neutralization on the copy (mail servers off, crons off, …).
    pub neutralize: bool,
}

/// A step of [`duplicate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DuplicateStep {
    /// Stopping the instances that run on the source (reported by the
    /// caller, which does it).
    StopInstances,
    /// Odoo copies the database (and neutralizes it) and its filestore.
    Copy,
    /// Checking the copy.
    Check,
    /// Removing the half-made copy after a failure.
    Cleanup,
    /// Starting the stopped instances again (reported by the caller).
    RestartInstances,
}

impl fmt::Display for DuplicateStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Copy => "copying the database",
            Self::Check => "checking the copy",
            Self::Cleanup => "removing the half-made copy",
            Self::StopInstances => "stopping the instances on the source",
            Self::RestartInstances => "starting the instances again",
        })
    }
}

/// Progress of [`duplicate`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum DuplicateEvent {
    /// A step started.
    Step {
        /// The step.
        step: DuplicateStep,
    },
    /// A line of `odoo-bin` output.
    Output {
        /// The line.
        line: String,
    },
}

/// What [`duplicate`] made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct DuplicatedDatabase {
    /// Name of the copy.
    pub name: String,
    /// The database it was copied from.
    pub source: String,
    /// Whether the copy was neutralized.
    pub neutralized: bool,
    /// Whether the source had a filestore, now copied.
    pub filestore_copied: bool,
}

/// Errors from [`duplicate`].
#[derive(Debug, Error)]
pub enum DuplicateError {
    /// A database name is invalid.
    #[error(transparent)]
    Name(#[from] NameError),
    /// The name of the copy is not one Odoo accepts.
    #[error(transparent)]
    Request(CreateError),
    /// The copy would have the source's name.
    #[error("the copy needs a name other than {0:?}")]
    SameName(String),
    /// No database has the source's name (or the project does not list it).
    #[error("no database named {0:?}")]
    NotFound(String),
    /// A database with the copy's name exists.
    #[error("a database named {0:?} already exists")]
    Exists(String),
    /// A filestore folder for the copy exists.
    #[error("a filestore {} already exists; move it away first", .0.display())]
    FilestoreExists(PathBuf),
    /// Instances of the project run on the source.
    #[error("instance(s) {} run on database {name:?}; they must be stopped to copy it", instances.join(", "))]
    InUse {
        /// Source database name.
        name: String,
        /// Ids of the running instances.
        instances: Vec<String>,
    },
    /// The PostgreSQL server cannot be used.
    #[error("PostgreSQL: {0}")]
    Server(String),
    /// `odoo-bin` could not be started.
    #[error("could not run {program}: {source}")]
    Spawn {
        /// Program.
        program: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// A step failed; the copy was removed.
    #[error("{step} failed (exit code {code:?}); the copy was removed. Last output:\n{tail}")]
    Failed {
        /// The step.
        step: DuplicateStep,
        /// Exit code, if it exited normally.
        code: Option<i32>,
        /// Last output lines, or what the check found.
        tail: String,
    },
}

impl From<CreateError> for DuplicateError {
    fn from(e: CreateError) -> Self {
        match e {
            CreateError::Name(e) => Self::Name(e),
            CreateError::Server(msg) => Self::Server(msg),
            other => Self::Server(other.to_string()),
        }
    }
}

impl DuplicateRequest {
    /// Checks both names: the source by OdHive's rule, the copy also by
    /// Odoo's, and that they differ.
    ///
    /// # Errors
    /// [`DuplicateError::Name`], [`DuplicateError::Request`] or
    /// [`DuplicateError::SameName`].
    pub fn check(&self) -> Result<(), DuplicateError> {
        names::validate_db_name(&self.source)?;
        validate_new_db_name(&self.name).map_err(|e| match e {
            CreateError::Name(e) => DuplicateError::Name(e),
            other => DuplicateError::Request(other),
        })?;
        if self.name == self.source {
            return Err(DuplicateError::SameName(self.source.clone()));
        }
        Ok(())
    }

    fn args(&self, conf: &Path) -> Vec<String> {
        let mut args = vec![
            "db".into(),
            "-c".into(),
            conf.display().to_string(),
            "duplicate".into(),
        ];
        if self.neutralize {
            args.push("--neutralize".into());
        }
        // Both names passed the checks: neither starts with `-`.
        args.push(self.source.clone());
        args.push(self.name.clone());
        args
    }
}

/// Copies `req.source` to `req.name` (see the module docs) on the server
/// `target` points at, which must be the one `odoo`'s config connects to.
/// `filestore_root` is the project's `data/filestore`. Reports progress to
/// `on_event`.
///
/// On failure, and when the returned future is dropped, the copy is removed
/// again (on drop, in a background task); the source is never touched.
///
/// # Errors
/// See [`DuplicateError`].
pub async fn duplicate(
    target: &PgTarget,
    odoo: &OdooCommand,
    req: &DuplicateRequest,
    filestore_root: &Path,
    timeout: Duration,
    mut on_event: impl FnMut(DuplicateEvent),
) -> Result<DuplicatedDatabase, DuplicateError> {
    req.check()?;
    let conn = postgres::connect(target, timeout).await.map_err(|e| {
        DuplicateError::Server(postgres::failure_check(target, e, None, None).summary)
    })?;
    if !db_exists(&conn.client, &req.source).await? {
        return Err(DuplicateError::NotFound(req.source.clone()));
    }
    if db_exists(&conn.client, &req.name).await? {
        return Err(DuplicateError::Exists(req.name.clone()));
    }
    let place = conn.place.clone();
    drop(conn);
    let new_fs = filestore_root.join(&req.name);
    if new_fs.symlink_metadata().is_ok() {
        return Err(DuplicateError::FilestoreExists(new_fs));
    }
    let source_fs = filestore_root.join(&req.source);
    let had_filestore = source_fs.is_dir();
    let source_uuid = {
        let client = postgres::connect_db(target, &place, &req.source, timeout)
            .await
            .map_err(DuplicateError::Server)?;
        param(&client, UUID_KEY).await?
    };

    // From here on a failure or a drop removes the copy.
    let mut guard = DropGuard(Some((odoo.clone(), req.name.clone())));
    let outcome = copy_and_check(
        target,
        &place,
        odoo,
        req,
        source_uuid.as_deref(),
        had_filestore.then_some(new_fs.as_path()),
        timeout,
        &mut on_event,
    )
    .await;
    guard.0 = None;
    match outcome {
        Ok(()) => Ok(DuplicatedDatabase {
            name: req.name.clone(),
            source: req.source.clone(),
            neutralized: req.neutralize,
            filestore_copied: had_filestore,
        }),
        Err(e) => {
            on_event(DuplicateEvent::Step {
                step: DuplicateStep::Cleanup,
            });
            if let Err(why) = drop_database(odoo, &req.name, |line| {
                on_event(DuplicateEvent::Output { line });
            })
            .await
            {
                tracing::warn!(db = %req.name, "could not remove the half-made copy: {why}");
            }
            Err(e)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn copy_and_check(
    target: &PgTarget,
    place: &postgres::Place,
    odoo: &OdooCommand,
    req: &DuplicateRequest,
    source_uuid: Option<&str>,
    filestore: Option<&Path>,
    timeout: Duration,
    on_event: &mut impl FnMut(DuplicateEvent),
) -> Result<(), DuplicateError> {
    on_event(DuplicateEvent::Step {
        step: DuplicateStep::Copy,
    });
    let done = proc::run_streaming(odoo.command(&req.args(&odoo.conf)), TAIL_LINES, |line| {
        on_event(DuplicateEvent::Output { line });
    })
    .await
    .map_err(|e| match e {
        RunError::Spawn(source) => DuplicateError::Spawn {
            program: odoo.python.clone(),
            source,
        },
        RunError::Io(e) => DuplicateError::Failed {
            step: DuplicateStep::Copy,
            code: None,
            tail: e.to_string(),
        },
    })?;
    if !done.status.success() {
        return Err(DuplicateError::Failed {
            step: DuplicateStep::Copy,
            code: done.status.code(),
            tail: done.tail,
        });
    }

    on_event(DuplicateEvent::Step {
        step: DuplicateStep::Check,
    });
    let check_failed = |why: String| DuplicateError::Failed {
        step: DuplicateStep::Check,
        code: Some(0),
        tail: why,
    };
    let client = postgres::connect_db(target, place, &req.name, timeout)
        .await
        .map_err(|why| check_failed(format!("the copy cannot be opened: {why}")))?;
    let uuid = param(&client, UUID_KEY).await?;
    if source_uuid.is_some() && uuid.as_deref() == source_uuid {
        return Err(check_failed(
            "the copy kept the source's database UUID".into(),
        ));
    }
    if req.neutralize
        && !param(&client, NEUTRALIZED_KEY)
            .await?
            .is_some_and(|v| !v.trim().is_empty())
    {
        return Err(check_failed("the copy was not neutralized".into()));
    }
    if let Some(fs) = filestore
        && !fs.is_dir()
    {
        return Err(check_failed(format!(
            "the filestore was not copied to {}",
            fs.display()
        )));
    }
    Ok(())
}

async fn db_exists(client: &Client, name: &str) -> Result<bool, DuplicateError> {
    client
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_database WHERE datname = $1)",
            &[&name],
        )
        .await
        .and_then(|r| r.try_get(0))
        .map_err(|e| DuplicateError::Server(pg_message(&e)))
}

/// An `ir_config_parameter` value; `None` when the table or key is missing.
async fn param(client: &Client, key: &str) -> Result<Option<String>, DuplicateError> {
    let server = |e: tokio_postgres::Error| DuplicateError::Server(pg_message(&e));
    let table: bool = client
        .query_one(
            "SELECT pg_catalog.to_regclass('public.ir_config_parameter') IS NOT NULL",
            &[],
        )
        .await
        .and_then(|r| r.try_get(0))
        .map_err(server)?;
    if !table {
        return Ok(None);
    }
    let value: Option<Option<String>> = client
        .query_opt(
            "SELECT value FROM public.ir_config_parameter WHERE key = $1",
            &[&key],
        )
        .await
        .and_then(|r| r.map(|r| r.try_get(0)).transpose())
        .map_err(server)?;
    Ok(value.flatten())
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    fn req(source: &str, name: &str) -> DuplicateRequest {
        DuplicateRequest {
            source: source.into(),
            name: name.into(),
            neutralize: false,
        }
    }

    #[test]
    fn names_are_checked() {
        assert!(req("shop", "shop_copy").check().is_ok());
        assert!(matches!(
            req("shop", "shop").check(),
            Err(DuplicateError::SameName(_))
        ));
        for bad in [
            "x;DROP DATABASE postgres",
            "a b",
            "-x",
            "a'b",
            "",
            "_x",
            "x",
        ] {
            assert!(req("shop", bad).check().is_err(), "copy {bad:?} accepted");
        }
        for bad in ["x;DROP DATABASE postgres", "a b", "-x", "a'b", ""] {
            assert!(req(bad, "copy").check().is_err(), "source {bad:?} accepted");
        }
    }

    #[test]
    fn arguments_put_the_names_last() {
        let conf = Path::new("/p/odoo.conf");
        assert_eq!(
            req("shop", "shop_copy").args(conf),
            ["db", "-c", "/p/odoo.conf", "duplicate", "shop", "shop_copy"]
        );
        let mut r = req("shop", "shop_copy");
        r.neutralize = true;
        assert_eq!(
            r.args(conf),
            [
                "db",
                "-c",
                "/p/odoo.conf",
                "duplicate",
                "--neutralize",
                "shop",
                "shop_copy"
            ]
        );
    }
}
