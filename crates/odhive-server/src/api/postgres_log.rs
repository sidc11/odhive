//! `/projects/{name}/postgres/log`: the PostgreSQL server log, read-only.
//!
//! The file is the project's `postgres_log`, else the most recently written
//! one OdHive finds ([`odhive_core::pglog::detect`]). Clients poll: each
//! request reads what was written since the last one.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError};

use axum::Json;
use axum::extract::State;
use odhive_core::logs::store::Record;
use odhive_core::pglog::{self, LogPathError};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use super::error::{ApiError, ApiResult, ErrorBody};
use super::instances::MAX_LOG_LIMIT;
use super::{Body, Params, QueryArgs};
use crate::service::Service;

/// Entries sent by default when `from` is not given.
pub const DEFAULT_TAIL: usize = 1000;

/// Which entries to read.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct PostgresLogQuery {
    /// First sequence number wanted (`next` of the previous answer, same
    /// `stream`); omitted = the newest `limit` entries.
    pub from: Option<u64>,
    /// Most entries sent, 0–5000 (default 1000).
    pub limit: Option<usize>,
}

/// Whether the log could be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PostgresLogStatus {
    /// Read; `records` has the entries.
    Ok,
    /// No log file was found, or the configured one is gone.
    NotFound,
    /// The file (or its folder) may not be read by this user; see `hint`.
    PermissionDenied,
    /// Reading failed otherwise; see `message`.
    Unreadable,
}

/// The PostgreSQL server log.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PostgresLog {
    /// The file shown (links resolved); `null` when none was found.
    pub path: Option<String>,
    /// The file set for the project (`postgres_log`), as written there;
    /// `null` = the first one found is shown.
    pub configured: Option<String>,
    /// PostgreSQL log files found on this machine, the most recently
    /// written first.
    pub found: Vec<String>,
    /// Whether the log could be read.
    pub status: PostgresLogStatus,
    /// What went wrong, when `status` is not `ok`.
    pub message: Option<String>,
    /// What to do about it.
    pub hint: Option<String>,
    /// Changes when the log is read anew: its sequence numbers start over,
    /// so `from` of an older stream must not be used.
    pub stream: u64,
    /// Entries in log order.
    pub records: Vec<Arc<Record>>,
    /// Entries asked for that were already dropped from memory.
    pub skipped: u64,
    /// `from` of the next request.
    pub next: u64,
}

/// The file to show for a project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PostgresLogSource {
    /// Absolute path of a `*.log` file; `null` = the first one found.
    #[schema(example = "/var/log/postgresql/postgresql-16-main.log")]
    pub path: Option<String>,
}

fn text(p: &Path) -> String {
    p.display().to_string()
}

fn failed(status: PostgresLogStatus, message: String, hint: Option<String>) -> PostgresLog {
    PostgresLog {
        path: None,
        configured: None,
        found: Vec::new(),
        status,
        message: Some(message),
        hint,
        stream: 0,
        records: Vec::new(),
        skipped: 0,
        next: 1,
    }
}

fn read(svc: &Service, configured: Option<&Path>, q: &PostgresLogQuery) -> PostgresLog {
    let found = pglog::detect(svc.pg_log_dirs());
    let path = match configured {
        Some(p) => match pglog::check_log_path(p) {
            Ok(real) => real,
            Err(LogPathError::Missing(_)) => {
                return failed(
                    PostgresLogStatus::NotFound,
                    format!("the PostgreSQL log {} does not exist", p.display()),
                    Some("Choose another log file, or let OdHive find one.".to_owned()),
                );
            }
            Err(LogPathError::PermissionDenied(_)) => {
                return failed(
                    PostgresLogStatus::PermissionDenied,
                    format!("cannot read {}: permission denied", p.display()),
                    Some(pglog::permission_hint(p)),
                );
            }
            Err(e) => return failed(PostgresLogStatus::Unreadable, e.to_string(), None),
        },
        None => match found.first() {
            Some(c) => c.path.clone(),
            None => {
                return failed(
                    PostgresLogStatus::NotFound,
                    "no PostgreSQL log file was found".to_owned(),
                    Some(not_found_hint()),
                );
            }
        },
    };
    let mut out = match open(svc, &path, q) {
        Ok(log) => log,
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => failed(
            PostgresLogStatus::PermissionDenied,
            format!("cannot read {}: permission denied", path.display()),
            Some(pglog::permission_hint(&path)),
        ),
        Err(e) if e.kind() == io::ErrorKind::NotFound => failed(
            PostgresLogStatus::NotFound,
            format!("the PostgreSQL log {} does not exist", path.display()),
            None,
        ),
        Err(e) => failed(
            PostgresLogStatus::Unreadable,
            format!("cannot read {}: {e}", path.display()),
            None,
        ),
    };
    out.path = Some(text(&path));
    out.configured = configured.map(text);
    out.found = found.iter().map(|c| text(&c.path)).collect();
    out
}

fn not_found_hint() -> String {
    if cfg!(target_os = "macos") {
        "Homebrew's PostgreSQL logs to $(brew --prefix)/var/log/postgresql@<version>.log. Choose the log file of your server.".to_owned()
    } else {
        "Debian and Ubuntu log to /var/log/postgresql/. If your server logs elsewhere (its log_directory), choose that file; logs kept only by journald cannot be shown yet.".to_owned()
    }
}

fn open(svc: &Service, path: &Path, q: &PostgresLogQuery) -> io::Result<PostgresLog> {
    let shared = svc.pg_logs().get(path)?;
    let mut tail = shared.tail.lock().unwrap_or_else(PoisonError::into_inner);
    tail.poll()?;
    let limit = q.limit.unwrap_or(DEFAULT_TAIL);
    let (records, skipped, next) = match q.from {
        Some(from) => {
            let page = tail.read_from(from.max(1), limit);
            (page.records, page.skipped, page.next)
        }
        None => (tail.tail(limit), 0, tail.next_seq()),
    };
    Ok(PostgresLog {
        path: None,
        configured: None,
        found: Vec::new(),
        status: PostgresLogStatus::Ok,
        message: None,
        hint: None,
        stream: shared.id,
        records,
        skipped,
        next,
    })
}

async fn answer(
    svc: Service,
    configured: Option<PathBuf>,
    q: PostgresLogQuery,
) -> ApiResult<PostgresLog> {
    tokio::task::spawn_blocking(move || read(&svc, configured.as_deref(), &q))
        .await
        .map_err(|e| ApiError::internal(format!("reading the PostgreSQL log failed: {e}")))
}

/// The PostgreSQL server log of a project's server: the newest entries, or
/// those from `from` on. A log that cannot be read is answered with its
/// `status` and a `hint`, not an error.
#[utoipa::path(operation_id = "postgres_log", get, path = "/projects/{name}/postgres/log", tag = "databases",
    params(
        ("name" = String, Path, description = "Project name"),
        PostgresLogQuery,
    ),
    responses(
        (status = OK, description = "Success", body = PostgresLog),
        (status = BAD_REQUEST, description = "Invalid request", body = ErrorBody),
        (status = NOT_FOUND, description = "No such project", body = ErrorBody),
    ))]
pub(super) async fn get(
    State(svc): State<Service>,
    Params(name): Params<String>,
    QueryArgs(q): QueryArgs<PostgresLogQuery>,
) -> ApiResult<Json<PostgresLog>> {
    if q.limit.is_some_and(|l| l > MAX_LOG_LIMIT) {
        return Err(ApiError::bad_request(
            "bad_query",
            format!("limit must be at most {MAX_LOG_LIMIT}"),
        ));
    }
    let (_, project) = svc.project(&name)?;
    let configured = project.config.postgres_log.clone();
    Ok(Json(answer(svc, configured, q).await?))
}

/// Chooses the log file shown for a project, saved as `postgres_log` in
/// its `odhive.toml`; `null` goes back to the first one found. The path
/// must be absolute, without `.`/`..` parts, and lead (links resolved) to a
/// regular `*.log` file. Answers like `GET` without entries.
#[utoipa::path(operation_id = "put_postgres_log", put, path = "/projects/{name}/postgres/log", tag = "databases",
    request_body = PostgresLogSource,
    params(("name" = String, Path, description = "Project name")),
    responses(
        (status = OK, description = "Saved", body = PostgresLog),
        (status = BAD_REQUEST, description = "Invalid request", body = ErrorBody),
        (status = NOT_FOUND, description = "No such project", body = ErrorBody),
        (status = CONFLICT, description = "A setup of the project runs", body = ErrorBody),
        (status = UNPROCESSABLE_ENTITY, description = "Not a readable *.log file", body = ErrorBody),
    ))]
pub(super) async fn put(
    State(svc): State<Service>,
    Params(name): Params<String>,
    Body(req): Body<PostgresLogSource>,
) -> ApiResult<Json<PostgresLog>> {
    let path = req.path.map(PathBuf::from);
    if let Some(p) = &path {
        let checked = p.clone();
        tokio::task::spawn_blocking(move || pglog::check_log_path(&checked))
            .await
            .map_err(|e| ApiError::internal(format!("checking the log path failed: {e}")))?
            .map_err(|e| ApiError::invalid("bad_log_path", e.to_string()))?;
    }
    let saved = path.clone();
    svc.edit_project(&name, move |c| {
        c.postgres_log = saved;
        Ok(())
    })
    .await?;
    let q = PostgresLogQuery {
        from: None,
        limit: Some(0),
    };
    Ok(Json(answer(svc, path, q).await?))
}
