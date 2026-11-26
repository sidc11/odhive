//! Odoo databases on a project's PostgreSQL server.
//!
//! [`health`] checks the server the project's Odoo connects to (with the
//! project's `db_*` options): reachable, server version, whether the role
//! may create databases. [`list()`] lists the databases Odoo would offer.
//! [`create()`] creates one with the project's `odoo-bin`. [`restore()`]
//! restores a dump (Odoo backup zip, `pg_dump` archive or SQL script).
//! [`delete()`] drops one with its filestore. [`duplicate()`] copies one
//! with its filestore. [`backup()`] backs one up into the project's
//! `backups/` folder; [`list_backups`] lists that folder.

pub mod archive;
mod backup;
mod create;
mod delete;
mod duplicate;
mod list;
mod restore;
mod target;

use std::time::Duration;

use serde::Serialize;

pub use backup::{
    Backup, BackupError, BackupEvent, BackupFormat, BackupRequest, BackupStep, MAX_BACKUP_NAME_LEN,
    backup, backup_file_name, delete_backup, list_backups, open_backup, validate_backup_name,
};
pub use create::{
    CreateError, CreateEvent, CreateStep, CreatedDatabase, DEFAULT_LANGUAGE, DEFAULT_LOGIN,
    FINALIZE_SCRIPT, MAX_MODULES, NewDatabase, OdooCommand, create, exists, missing_modules,
    validate_new_db_name,
};
pub use delete::{
    DeleteError, DeleteEvent, DeleteStep, DeletedDatabase, check_confirmation, delete,
};
pub use duplicate::{
    DuplicateError, DuplicateEvent, DuplicateRequest, DuplicateStep, DuplicatedDatabase, UUID_KEY,
    duplicate,
};
pub use list::{Database, DatabaseList, list};
pub use restore::{
    FINALIZE_RESTORE_SCRIPT, NEUTRALIZED_KEY, PgTools, RestoreError, RestoreEvent, RestoreOptions,
    RestoreRequest, RestoreStep, RestoredDatabase, pg_path_option, restore, template_option,
};
pub use target::{ConnectionSettings, PgTarget, SOCKET_DIRS, SslMode};

use crate::envcheck::{Check, Requirement, postgres};
use crate::syslib::PackageManager;

/// What the server says about itself and the role once logged in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct ServerFacts {
    /// `server_version_num`, e.g. `170002`.
    pub version_num: i32,
    /// e.g. `17.2`.
    pub version: String,
    /// Role logged in as.
    pub role: String,
    /// Whether the role may create databases (`CREATEDB` or superuser).
    pub can_create_db: bool,
    /// Whether the role is a superuser.
    pub superuser: bool,
}

/// [`health`]'s answer.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct DbHealth {
    /// Whether the server can be used, and the fix when it cannot.
    pub check: Check,
    /// Set when the login worked.
    pub server: Option<ServerFacts>,
    /// The connection settings used (never the password).
    pub settings: ConnectionSettings,
}

impl DbHealth {
    /// Whether databases can be created and restored.
    #[must_use]
    pub fn can_create_db(&self) -> bool {
        self.server.as_ref().is_some_and(|s| s.can_create_db)
    }
}

/// Connects to `target` (sockets first, then TCP) within `timeout` per
/// attempt and reports on it. `requirement` adds the project's minimum
/// server version. Never fails: problems are findings.
pub async fn health(
    target: &PgTarget,
    requirement: Option<&Requirement>,
    pm: Option<PackageManager>,
    timeout: Duration,
) -> DbHealth {
    let requirements: Vec<Requirement> = requirement.cloned().into_iter().collect();
    let (check, server) = postgres::probe(target, None, &requirements, pm, timeout).await;
    DbHealth {
        check,
        server,
        settings: target.settings(),
    }
}

#[cfg(test)]
mod tests;
