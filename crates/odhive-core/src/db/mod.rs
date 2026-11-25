//! Odoo databases on a project's PostgreSQL server.
//!
//! [`health`] checks the server the project's Odoo connects to (with the
//! project's `db_*` options): reachable, server version, whether the role
//! may create databases.

mod target;

use std::time::Duration;

use serde::Serialize;

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
