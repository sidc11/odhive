//! Deleting an Odoo database the way Odoo's own database manager does.
//!
//! [`delete`] runs `odoo-bin db -c <conf> drop <db>` (never through a
//! shell): Odoo ends the other connections to the database, drops it and
//! removes its filestore. OdHive then checks that the database is gone and
//! removes a filestore Odoo left behind (one in the project's filestore
//! folder that Odoo did not look for, e.g. after a `data_dir` change).
//!
//! Which databases may be deleted (the project lists it, the user typed its
//! name, no instance runs on it) is the caller's business.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::PgTarget;
use super::create::{CreateError, OdooCommand, drop_database, exists};
use crate::names::{self, NameError};

/// A step of [`delete`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DeleteStep {
    /// Taking a snapshot first, when asked (reported by the caller, which
    /// does it).
    Snapshot,
    /// Odoo drops the database and its filestore.
    Drop,
    /// Checking that the database and its filestore are gone.
    Check,
}

impl fmt::Display for DeleteStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Snapshot => "taking a snapshot first",
            Self::Drop => "dropping the database",
            Self::Check => "checking the database is gone",
        })
    }
}

/// Progress of [`delete`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum DeleteEvent {
    /// A step started.
    Step {
        /// The step.
        step: DeleteStep,
    },
    /// A line of `odoo-bin` output.
    Output {
        /// The line.
        line: String,
    },
}

/// What [`delete`] removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct DeletedDatabase {
    /// Database name.
    pub name: String,
    /// Whether a filestore folder existed and was removed.
    pub filestore_removed: bool,
}

/// Errors from [`delete`].
#[derive(Debug, Error)]
pub enum DeleteError {
    /// The database name is invalid.
    #[error(transparent)]
    Name(#[from] NameError),
    /// No database has this name (or the project does not list it).
    #[error("no database named {0:?}")]
    NotFound(String),
    /// The typed confirmation is not the database name.
    #[error("type the database name {0:?} exactly to delete it")]
    NotConfirmed(String),
    /// Instances of the project run on the database.
    #[error("instance(s) {} run on database {name:?}; stop them first", instances.join(", "))]
    InUse {
        /// Database name.
        name: String,
        /// Ids of the running instances.
        instances: Vec<String>,
    },
    /// The PostgreSQL server cannot be used.
    #[error("PostgreSQL: {0}")]
    Server(String),
    /// `odoo-bin db drop` failed or the database is still there.
    #[error("could not delete database {name:?}. Last output:\n{tail}")]
    Failed {
        /// Database name.
        name: String,
        /// Last output lines.
        tail: String,
    },
    /// The database is gone but its filestore could not be removed.
    #[error("database {name:?} was dropped but its filestore {} could not be removed: {source}", path.display())]
    Filestore {
        /// Database name.
        name: String,
        /// Filestore folder.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
}

impl From<CreateError> for DeleteError {
    fn from(e: CreateError) -> Self {
        match e {
            CreateError::Name(e) => Self::Name(e),
            other => Self::Server(other.to_string()),
        }
    }
}

/// Checks that `confirm` is exactly `name` (no trimming, no case folding).
///
/// # Errors
/// [`DeleteError::NotConfirmed`] otherwise.
pub fn check_confirmation(name: &str, confirm: &str) -> Result<(), DeleteError> {
    if confirm == name {
        Ok(())
    } else {
        Err(DeleteError::NotConfirmed(name.to_owned()))
    }
}

/// Deletes database `name` (see the module docs) on the server `target`
/// points at, which must be the one `odoo`'s config connects to.
/// `filestore_root` is the project's `data/filestore`. Reports progress to
/// `on_event`.
///
/// Dropping a database is atomic: a cancelled delete leaves it either
/// whole or gone (a filestore may then be left; deleting again removes it).
///
/// # Errors
/// See [`DeleteError`].
pub async fn delete(
    target: &PgTarget,
    odoo: &OdooCommand,
    name: &str,
    filestore_root: &Path,
    timeout: Duration,
    mut on_event: impl FnMut(DeleteEvent),
) -> Result<DeletedDatabase, DeleteError> {
    names::validate_db_name(name)?;
    if !exists(target, name, timeout).await? {
        return Err(DeleteError::NotFound(name.to_owned()));
    }
    let filestore = filestore_root.join(name);
    let had_filestore = filestore.symlink_metadata().is_ok();

    on_event(DeleteEvent::Step {
        step: DeleteStep::Drop,
    });
    let dropped = drop_database(odoo, name, |line| {
        on_event(DeleteEvent::Output { line });
    })
    .await;

    on_event(DeleteEvent::Step {
        step: DeleteStep::Check,
    });
    if exists(target, name, timeout).await? {
        return Err(DeleteError::Failed {
            name: name.to_owned(),
            tail: dropped.err().unwrap_or_else(|| {
                "odoo-bin db drop exited 0 but the database is still there".into()
            }),
        });
    }
    if let Err(tail) = dropped {
        // Gone anyway (e.g. Odoo failed after the drop): finish the job.
        tracing::warn!(db = %name, "odoo-bin db drop failed after dropping: {tail}");
    }
    remove_filestore(&filestore).map_err(|source| DeleteError::Filestore {
        name: name.to_owned(),
        path: filestore.clone(),
        source,
    })?;
    Ok(DeletedDatabase {
        name: name.to_owned(),
        filestore_removed: had_filestore,
    })
}

/// Removes `path` if it exists: a folder with its contents (links inside
/// are removed, never followed), or a file or link.
fn remove_filestore(path: &Path) -> std::io::Result<()> {
    match path.symlink_metadata() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn confirmation_must_match_exactly() {
        assert!(check_confirmation("shop_main", "shop_main").is_ok());
        for typed in [
            "",
            "shop",
            "Shop_main",
            " shop_main",
            "shop_main ",
            "shop_main\n",
        ] {
            assert!(
                matches!(
                    check_confirmation("shop_main", typed),
                    Err(DeleteError::NotConfirmed(_))
                ),
                "{typed:?} accepted"
            );
        }
    }

    #[test]
    fn filestore_removal_does_not_follow_links() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).expect("mkdir");
        std::fs::write(outside.join("keep"), "x").expect("write");

        let fs = dir.path().join("filestore").join("shop");
        std::fs::create_dir_all(fs.join("ab")).expect("mkdir");
        std::fs::write(fs.join("ab/file"), "x").expect("write");
        std::os::unix::fs::symlink(&outside, fs.join("link")).expect("symlink");
        remove_filestore(&fs).expect("remove");
        assert!(!fs.exists());
        assert!(outside.join("keep").exists());

        // The filestore itself a link: only the link goes.
        std::os::unix::fs::symlink(&outside, &fs).expect("symlink");
        remove_filestore(&fs).expect("remove");
        assert!(fs.symlink_metadata().is_err());
        assert!(outside.join("keep").exists());

        remove_filestore(&fs).expect("missing is fine");
    }
}
