//! `/environment`: whether this machine has what Odoo development needs.

use axum::Json;
use axum::extract::State;
use odhive_core::config::Edition;
use odhive_core::envcheck::{self, Report, Requirement};

use super::error::{ApiResult, ErrorBody};
use crate::service::Service;

/// Checks PostgreSQL (reachable over the default connection, server
/// version, role may create databases), Python for each registered
/// Community source (Odoo 19 and 20 when none is registered), wkhtmltopdf,
/// git and uv. Each check carries a fix when something is off; install
/// commands are text for the user and are never run.
#[utoipa::path(operation_id = "check_environment", get, path = "/environment", tag = "service",
    responses(
        (status = OK, description = "Success", body = envcheck::Report),
        (status = INTERNAL_SERVER_ERROR, description = "Config unreadable", body = ErrorBody),
    ))]
pub(super) async fn check(State(svc): State<Service>) -> ApiResult<Json<Report>> {
    let cfg = svc.config()?;
    let mut opts = svc.env_options().clone();
    opts.discover = svc.discover_options().clone();
    opts.requirements = cfg
        .sources
        .iter()
        .filter(|s| s.edition == Edition::Community)
        .map(|s| Requirement::for_source(&s.id, &s.path, s.odoo_version))
        .collect();
    Ok(Json(envcheck::run(&opts).await))
}
