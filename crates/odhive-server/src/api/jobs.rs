//! `/jobs`: background work started by other routes (source clones, project
//! setups). Their progress also arrives over the WebSocket
//! (`subscribe_jobs`).

use std::time::Duration;

use axum::Json;
use axum::extract::State;

use super::Params;
use super::error::{ApiResult, ErrorBody};
use crate::jobs::Job;
use crate::service::Service;

/// How long a cancel request waits for the job to clean up.
const CANCEL_WAIT: Duration = Duration::from_secs(20);

/// Running and recently ended jobs, oldest first.
#[utoipa::path(operation_id = "list_jobs", get, path = "/jobs", tag = "jobs",
    responses((status = OK, description = "Success", body = Vec<Job>)))]
pub(super) async fn list(State(svc): State<Service>) -> Json<Vec<Job>> {
    Json(svc.jobs().list())
}

/// One job.
#[utoipa::path(operation_id = "get_job", get, path = "/jobs/{id}", tag = "jobs",
    params(("id" = u64, Path, description = "Job id")),
    responses(
        (status = OK, description = "Success", body = Job),
        (status = BAD_REQUEST, description = "Invalid id", body = ErrorBody),
        (status = NOT_FOUND, description = "Not found", body = ErrorBody),
    ))]
pub(super) async fn get(
    State(svc): State<Service>,
    Params(id): Params<u64>,
) -> ApiResult<Json<Job>> {
    svc.jobs()
        .get(id)
        .map(Json)
        .ok_or_else(|| super::error::ApiError::not_found("unknown_job", format!("no job {id}")))
}

/// Cancels a running job: its tool is stopped and what it did is undone.
/// Answers once the job ended (or after 20 s); an ended job is returned as
/// is.
#[utoipa::path(operation_id = "cancel_job", post, path = "/jobs/{id}/cancel", tag = "jobs",
    params(("id" = u64, Path, description = "Job id")),
    responses(
        (status = OK, description = "The job after the cancel", body = Job),
        (status = BAD_REQUEST, description = "Invalid id", body = ErrorBody),
        (status = NOT_FOUND, description = "Not found", body = ErrorBody),
    ))]
pub(super) async fn cancel(
    State(svc): State<Service>,
    Params(id): Params<u64>,
) -> ApiResult<Json<Job>> {
    Ok(Json(svc.jobs().cancel(id, CANCEL_WAIT).await?))
}
