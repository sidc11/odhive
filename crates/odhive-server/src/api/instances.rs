//! Instance actions and logs: `/instances` and
//! `/projects/{name}/instances/{id}/…`.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use odhive_core::autofix::Candidate;
use odhive_core::launch::Target;
use odhive_core::logs::store::Record;
use odhive_core::manager::{InstanceInfo, StopOutcome};
use odhive_core::ports::PortOwner;
use odhive_core::supervisor::Status;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use super::error::{ApiError, ApiResult, ErrorBody};
use super::{Params, QueryArgs};
use crate::service::{Service, Stopped};

/// Signal number of `SIGKILL` (the same on Linux and macOS).
const SIGKILL: i32 = 9;

/// Default number of log entries per page.
pub const DEFAULT_LOG_LIMIT: usize = 500;
/// Largest number of log entries per page.
pub const MAX_LOG_LIMIT: usize = 5000;

/// How a stop request ended.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct StopResult {
    /// A process was running.
    pub was_running: bool,
    /// It did not stop in time and was killed.
    pub killed: bool,
    /// Final status, for an instance this service supervised.
    pub status: Option<Status>,
}

/// Which log entries to read.
#[derive(Debug, Clone, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct LogQuery {
    /// First sequence number wanted (`next` of the previous page); omitted =
    /// the newest `limit` entries.
    pub from: Option<u64>,
    /// Entries per page, 1–5000 (default 500).
    pub limit: Option<usize>,
}

/// Log entries of one instance.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct LogPage {
    /// Entries in log order.
    pub records: Vec<Arc<Record>>,
    /// Entries asked for that were already dropped from memory (the log
    /// file still has them).
    pub skipped: u64,
    /// `from` for the next page.
    pub next: u64,
}

/// A project's instance, checked to exist.
pub(super) fn instance(svc: &Service, name: &str, id: &str) -> ApiResult<PortOwner> {
    let (_, project) = svc.project(name)?;
    project.select(&Target {
        project: name.to_owned(),
        instance: Some(id.to_owned()),
    })?;
    Ok(PortOwner::new(name, id))
}

/// Instances this service runs (and ended ones not yet stopped), across
/// projects.
#[utoipa::path(operation_id = "list_instances", get, path = "/instances", tag = "instances",
    responses((status = OK, description = "Success", body = Vec<InstanceInfo>)))]
pub(super) async fn list(State(svc): State<Service>) -> Json<Vec<InstanceInfo>> {
    Json(svc.manager().list())
}

/// Starts an instance with the project's current settings. Returns once the
/// process is spawned (`starting`); status changes follow as events.
#[utoipa::path(operation_id = "start_instance", post, path = "/projects/{name}/instances/{id}/start", tag = "instances",
    params(
        ("name" = String, Path, description = "Project name"),
        ("id" = String, Path, description = "Instance id"),
    ),
    responses(
        (status = OK, description = "Success", body = InstanceInfo),
        (status = NOT_FOUND, description = "Not found", body = ErrorBody),
        (status = CONFLICT, description = "Already running, running elsewhere, no venv, database or ports taken", body = ErrorBody),
    ))]
pub(super) async fn start(
    State(svc): State<Service>,
    Params((name, id)): Params<(String, String)>,
) -> ApiResult<Json<InstanceInfo>> {
    Ok(Json(svc.start(&name, &id, false).await?))
}

/// Stops an instance gracefully (SIGINT, then SIGKILL after 15 s). Works for
/// instances run outside this service too.
#[utoipa::path(operation_id = "stop_instance", post, path = "/projects/{name}/instances/{id}/stop", tag = "instances",
    params(
        ("name" = String, Path, description = "Project name"),
        ("id" = String, Path, description = "Instance id"),
    ),
    responses(
        (status = OK, description = "Success", body = StopResult),
        (status = NOT_FOUND, description = "Not found", body = ErrorBody),
        (status = CONFLICT, description = "A stop is under way", body = ErrorBody),
    ))]
pub(super) async fn stop(
    State(svc): State<Service>,
    Params((name, id)): Params<(String, String)>,
) -> ApiResult<Json<StopResult>> {
    let owner = instance(&svc, &name, &id)?;
    let result = match svc.stop(&owner).await? {
        Stopped::Managed {
            was_running,
            outcome: StopOutcome::Supervised { status },
        } => StopResult {
            was_running,
            killed: was_running
                && matches!(&status, Status::Stopped { exit } | Status::Crashed { exit }
                    if exit.signal == Some(SIGKILL)),
            status: Some(status),
        },
        Stopped::Managed {
            outcome: StopOutcome::Attached { stop },
            ..
        }
        | Stopped::External(stop) => StopResult {
            was_running: !stop.already_gone,
            killed: stop.killed,
            status: None,
        },
        Stopped::NotRunning => StopResult {
            was_running: false,
            killed: false,
            status: None,
        },
    };
    Ok(Json(result))
}

/// Restarts an instance with the project's current settings (starts it if
/// it is stopped). Instances run outside this service must be stopped
/// first.
#[utoipa::path(operation_id = "restart_instance", post, path = "/projects/{name}/instances/{id}/restart", tag = "instances",
    params(
        ("name" = String, Path, description = "Project name"),
        ("id" = String, Path, description = "Instance id"),
    ),
    responses(
        (status = OK, description = "Success", body = InstanceInfo),
        (status = NOT_FOUND, description = "Not found", body = ErrorBody),
        (status = CONFLICT, description = "Conflict with the current state", body = ErrorBody),
    ))]
pub(super) async fn restart(
    State(svc): State<Service>,
    Params((name, id)): Params<(String, String)>,
) -> ApiResult<Json<InstanceInfo>> {
    Ok(Json(svc.start(&name, &id, true).await?))
}

/// Installs the missing Python package awaiting confirmation, then restarts
/// the instance. Returns when the restart is issued.
#[utoipa::path(operation_id = "confirm_fix", post, path = "/projects/{name}/instances/{id}/fix/confirm", tag = "instances",
    params(
        ("name" = String, Path, description = "Project name"),
        ("id" = String, Path, description = "Instance id"),
    ),
    responses(
        (status = OK, description = "What was installed", body = Candidate),
        (status = NOT_FOUND, description = "Not found", body = ErrorBody),
        (status = CONFLICT, description = "Nothing awaits confirmation, or the attempt limit is reached", body = ErrorBody),
    ))]
pub(super) async fn confirm_fix(
    State(svc): State<Service>,
    Params((name, id)): Params<(String, String)>,
) -> ApiResult<Json<Candidate>> {
    let owner = instance(&svc, &name, &id)?;
    Ok(Json(svc.manager().confirm_fix(&owner).await?))
}

/// Drops the missing Python package awaiting confirmation.
#[utoipa::path(operation_id = "decline_fix", post, path = "/projects/{name}/instances/{id}/fix/decline", tag = "instances",
    params(
        ("name" = String, Path, description = "Project name"),
        ("id" = String, Path, description = "Instance id"),
    ),
    responses(
        (status = OK, description = "What was declined", body = Candidate),
        (status = NOT_FOUND, description = "Not found", body = ErrorBody),
        (status = CONFLICT, description = "Nothing awaits confirmation", body = ErrorBody),
    ))]
pub(super) async fn decline_fix(
    State(svc): State<Service>,
    Params((name, id)): Params<(String, String)>,
) -> ApiResult<Json<Candidate>> {
    let owner = instance(&svc, &name, &id)?;
    Ok(Json(svc.manager().decline_fix(&owner)?))
}

/// Log entries an instance run by this service holds in memory.
#[utoipa::path(operation_id = "instance_logs", get, path = "/projects/{name}/instances/{id}/logs", tag = "instances",
    params(
        ("name" = String, Path, description = "Project name"),
        ("id" = String, Path, description = "Instance id"),
        LogQuery,
    ),
    responses(
        (status = OK, description = "Success", body = LogPage),
        (status = BAD_REQUEST, description = "Invalid request", body = ErrorBody),
        (status = NOT_FOUND, description = "Not found", body = ErrorBody),
        (status = CONFLICT, description = "This service does not run the instance", body = ErrorBody),
    ))]
pub(super) async fn logs(
    State(svc): State<Service>,
    Params((name, id)): Params<(String, String)>,
    QueryArgs(q): QueryArgs<LogQuery>,
) -> ApiResult<Json<LogPage>> {
    let limit = q.limit.unwrap_or(DEFAULT_LOG_LIMIT);
    if !(1..=MAX_LOG_LIMIT).contains(&limit) {
        return Err(ApiError::bad_request(
            "bad_query",
            format!("limit must be 1–{MAX_LOG_LIMIT}"),
        ));
    }
    let owner = instance(&svc, &name, &id)?;
    let m = svc.manager();
    let not_running = || {
        ApiError::conflict(
            "not_running",
            format!(
                "{owner} is not run by this OdHive; its log file is in the project's logs/ folder"
            ),
        )
    };
    let page = match q.from {
        Some(from) => {
            let page = m.read_log(&owner, from, limit).ok_or_else(not_running)?;
            LogPage {
                records: page.records,
                skipped: page.skipped,
                next: page.next,
            }
        }
        None => {
            let next = m.log_next_seq(&owner).ok_or_else(not_running)?;
            let records = m.tail_log(&owner, limit).ok_or_else(not_running)?;
            LogPage {
                records,
                skipped: 0,
                next,
            }
        }
    };
    Ok(Json(page))
}
