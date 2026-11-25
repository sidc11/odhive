//! The databases Odoo would offer: owned by the role, not templates, not
//! the maintenance ones, and passing the project's `dbfilter`.

use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio_postgres::Client;

use super::{ConnectionSettings, PgTarget};
use crate::envcheck::{Check, postgres};
use crate::names::validate_db_name;
use crate::syslib::PackageManager;

/// Databases read at the same time.
const PARALLEL: usize = 4;
/// Longest `dbfilter` accepted, in bytes.
const MAX_FILTER_LEN: usize = 1024;

/// One database on the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct Database {
    /// Database name.
    pub name: String,
    /// Size on disk in bytes; `None` when the role may not connect.
    pub size_bytes: Option<i64>,
    /// Whether the name passes OdHive's rule (`^[A-Za-z0-9_.-]+$`, not
    /// starting with `-`). Other databases are listed but cannot be acted on.
    pub valid_name: bool,
    /// Whether Odoo's `base` module is installed.
    pub odoo: bool,
    /// `base`'s installed version, e.g. `19.0.1.3`.
    pub odoo_version: Option<String>,
    /// Odoo series from [`Self::odoo_version`], e.g. `19.0`.
    pub odoo_series: Option<String>,
    /// Latest change seen (module install or upgrade, user change, login),
    /// UTC, `YYYY-MM-DDTHH:MM:SSZ`.
    pub last_modified: Option<String>,
    /// Ids of the project's instances using this database.
    pub instances: Vec<String>,
    /// Why the database could not be read, when it could not.
    pub error: Option<String>,
}

/// [`list`]'s answer.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct DatabaseList {
    /// The databases, by name.
    pub databases: Vec<Database>,
    /// Set when the server could not be used; `databases` is then empty.
    pub error: Option<Check>,
    /// The project's `dbfilter`, when set.
    pub filter: Option<String>,
    /// Why the filter was not applied, when it was set and not applied.
    pub filter_note: Option<String>,
    /// Databases the filter hid.
    pub hidden: usize,
    /// The connection settings used (never the password).
    pub settings: ConnectionSettings,
}

/// A `dbfilter` as Odoo applies it: a Python regular expression matched at
/// the start of the name (`re.match`).
#[derive(Debug)]
pub(crate) enum DbFilter {
    /// No filter set.
    None,
    /// Applied.
    Regex(regex::Regex),
    /// Set, but not applied, for this reason.
    Unusable(String),
}

impl DbFilter {
    pub(crate) fn parse(filter: Option<&str>) -> Self {
        let Some(f) = filter.map(str::trim).filter(|f| !f.is_empty()) else {
            return Self::None;
        };
        if f.contains("%h") || f.contains("%d") {
            return Self::Unusable(
                "it depends on the host name each request uses (%h, %d), so every database is shown"
                    .into(),
            );
        }
        if f.len() > MAX_FILTER_LEN {
            return Self::Unusable(format!(
                "it is longer than {MAX_FILTER_LEN} bytes, so every database is shown"
            ));
        }
        match regex::RegexBuilder::new(&format!("^(?:{f})"))
            .size_limit(1 << 20)
            .build()
        {
            Ok(r) => Self::Regex(r),
            Err(_) => Self::Unusable(
                "it is not a regular expression OdHive can read, so every database is shown".into(),
            ),
        }
    }

    pub(crate) fn allows(&self, name: &str) -> bool {
        match self {
            Self::Regex(r) => r.is_match(name),
            Self::None | Self::Unusable(_) => true,
        }
    }

    fn note(&self) -> Option<String> {
        match self {
            Self::Unusable(why) => Some(why.clone()),
            Self::None | Self::Regex(_) => None,
        }
    }
}

/// Lists the databases on `target`'s server that Odoo would list for this
/// role, filtered by `dbfilter` (the project's Odoo option), with each one's
/// size, Odoo version and last change. `uses` pairs an instance id with
/// the database it uses, to fill [`Database::instances`]. Never fails: a server that cannot be
/// used is reported in [`DatabaseList::error`], a database that cannot be
/// read in its [`Database::error`].
pub async fn list(
    target: &PgTarget,
    dbfilter: Option<&str>,
    uses: &[(String, String)],
    pm: Option<PackageManager>,
    timeout: Duration,
) -> DatabaseList {
    let filter = DbFilter::parse(dbfilter);
    let mut out = DatabaseList {
        databases: Vec::new(),
        error: None,
        filter: dbfilter
            .map(str::trim)
            .filter(|f| !f.is_empty())
            .map(str::to_owned),
        filter_note: filter.note(),
        hidden: 0,
        settings: target.settings(),
    };
    let conn = match postgres::connect(target, timeout).await {
        Ok(c) => c,
        Err(e) => {
            out.error = Some(postgres::failure_check(target, e, None, pm));
            return out;
        }
    };
    let rows = match owned_databases(&conn.client).await {
        Ok(r) => r,
        Err(e) => {
            let mut c = Check::new(
                crate::envcheck::CheckId::Postgresql,
                crate::envcheck::Status::Error,
                "Connected, but the databases could not be listed",
            );
            c.details.push(pg_message(&e));
            c.location = Some(conn.label());
            out.error = Some(c);
            return out;
        }
    };
    drop(conn.client);

    let place = Arc::new(conn.place);
    let target = Arc::new(target.clone());
    let permits = Arc::new(Semaphore::new(PARALLEL));
    let mut tasks = JoinSet::new();
    for (name, size_bytes) in rows {
        if !filter.allows(&name) {
            out.hidden += 1;
            continue;
        }
        let mut db = Database {
            valid_name: validate_db_name(&name).is_ok(),
            instances: uses
                .iter()
                .filter(|(_, db)| *db == name)
                .map(|(id, _)| id.clone())
                .collect(),
            name,
            size_bytes,
            odoo: false,
            odoo_version: None,
            odoo_series: None,
            last_modified: None,
            error: None,
        };
        if size_bytes.is_none() {
            db.error = Some("the role may not connect to it".into());
            out.databases.push(db);
            continue;
        }
        let (target, place, permits) = (target.clone(), place.clone(), permits.clone());
        tasks.spawn(async move {
            let _permit = permits.acquire_owned().await;
            let read = async {
                let client = postgres::connect_db(&target, &place, &db.name, timeout).await?;
                odoo_facts(&client).await.map_err(|e| pg_message(&e))
            };
            match tokio::time::timeout(timeout * 2, read).await {
                Ok(Ok(facts)) => {
                    db.odoo = facts.version.is_some();
                    db.odoo_series = facts.version.as_deref().map(series);
                    db.odoo_version = facts.version;
                    db.last_modified = facts.last_modified;
                }
                Ok(Err(e)) => db.error = Some(e),
                Err(_) => db.error = Some(format!("no answer within {:?}", timeout * 2)),
            }
            db
        });
    }
    while let Some(done) = tasks.join_next().await {
        if let Ok(db) = done {
            out.databases.push(db);
        }
    }
    out.databases.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Odoo's `list_dbs`: databases the role owns that accept connections,
/// without templates and the maintenance databases. Size is `None` where
/// the role may not connect.
async fn owned_databases(
    client: &Client,
) -> Result<Vec<(String, Option<i64>)>, tokio_postgres::Error> {
    let rows = client
        .query(
            "SELECT d.datname::text, \
                    CASE WHEN pg_catalog.has_database_privilege(d.oid, 'CONNECT') \
                         THEN pg_catalog.pg_database_size(d.oid) END \
             FROM pg_catalog.pg_database d \
             WHERE d.datdba = (SELECT r.oid FROM pg_catalog.pg_roles r WHERE r.rolname = current_user) \
               AND NOT d.datistemplate AND d.datallowconn \
               AND d.datname NOT IN ('postgres', 'template0', 'template1') \
             ORDER BY d.datname",
            &[],
        )
        .await?;
    rows.iter()
        .map(|r| Ok((r.try_get(0)?, r.try_get(1)?)))
        .collect()
}

/// The server's own message (`Display` only says `db error`).
fn pg_message(e: &tokio_postgres::Error) -> String {
    e.as_db_error()
        .map_or_else(|| e.to_string(), |d| d.message().to_owned())
}

struct OdooFacts {
    version: Option<String>,
    last_modified: Option<String>,
}

/// `base`'s version and the latest change, when the database is Odoo's.
async fn odoo_facts(client: &Client) -> Result<OdooFacts, tokio_postgres::Error> {
    let row = client
        .query_one(
            "SELECT pg_catalog.to_regclass('public.ir_module_module') IS NOT NULL, \
                    pg_catalog.to_regclass('public.res_users') IS NOT NULL, \
                    pg_catalog.to_regclass('public.res_users_log') IS NOT NULL",
            &[],
        )
        .await?;
    let (modules, users, logins): (bool, bool, bool) =
        (row.try_get(0)?, row.try_get(1)?, row.try_get(2)?);
    if !modules {
        return Ok(OdooFacts {
            version: None,
            last_modified: None,
        });
    }
    // Only fixed table names, chosen by what exists: nothing user-supplied.
    let mut changes = vec!["(SELECT max(write_date) FROM public.ir_module_module)"];
    if users {
        changes.push("(SELECT max(write_date) FROM public.res_users)");
    }
    if logins {
        changes.push("(SELECT max(create_date) FROM public.res_users_log)");
    }
    let sql = format!(
        "SELECT (SELECT latest_version::text FROM public.ir_module_module \
                 WHERE name = 'base' AND state = 'installed' LIMIT 1), \
                to_char(GREATEST({}), 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')",
        changes.join(", ")
    );
    let row = client.query_one(&sql, &[]).await?;
    Ok(OdooFacts {
        version: row.try_get(0)?,
        last_modified: row.try_get(1)?,
    })
}

/// `19.0.1.3` → `19.0`; `saas~18.3.1.3` → `saas~18.3`: the module's own
/// version is the last two parts.
fn series(version: &str) -> String {
    let parts: Vec<&str> = version.split('.').collect();
    if parts.len() > 2 {
        parts[..parts.len() - 2].join(".")
    } else {
        version.to_owned()
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn series_drops_the_module_version() {
        assert_eq!(series("19.0.1.3"), "19.0");
        assert_eq!(series("saas~18.3.1.3"), "saas~18.3");
        assert_eq!(series("1.3"), "1.3");
    }

    #[test]
    fn filters_match_at_the_start_like_python() {
        let f = DbFilter::parse(Some("acme"));
        assert!(f.allows("acme"));
        assert!(f.allows("acme_main"));
        assert!(!f.allows("old_acme"));
        let f = DbFilter::parse(Some("^acme$"));
        assert!(f.allows("acme") && !f.allows("acme_main"));
        // An alternation stays anchored as a whole.
        let f = DbFilter::parse(Some("a|b"));
        assert!(f.allows("b1") && !f.allows("cb"));
        assert!(DbFilter::parse(None).allows("x"));
        assert!(DbFilter::parse(Some("  ")).note().is_none());
    }

    #[test]
    fn unusable_filters_show_everything_with_a_note() {
        for f in ["^%d$", "%h_db", "(unclosed", "(?=look)ahead"] {
            let parsed = DbFilter::parse(Some(f));
            assert!(parsed.allows("anything"), "{f}");
            assert!(parsed.note().is_some(), "{f}");
        }
        let long = "a".repeat(MAX_FILTER_LEN + 1);
        assert!(DbFilter::parse(Some(&long)).note().is_some());
    }
}
