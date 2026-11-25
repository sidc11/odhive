//! `/projects/{name}/postgres`: the PostgreSQL server a project's Odoo
//! connects to, with the project's `db_*` options.

use axum::Json;
use axum::extract::State;
use odhive_core::db::{self, DbHealth, PgTarget};
use odhive_core::envcheck::Requirement;

use super::Params;
use super::error::{ApiResult, ErrorBody};
use crate::service::Service;

/// Connects with the project's `db_host`, `db_port`, `db_user`,
/// `db_password` and `db_sslmode` (unset ones come from the project's
/// `[env]` `PG*` variables, then the service's, then libpq's defaults) and
/// reports whether the server is reachable, its version against the
/// project's Odoo series, and whether the role may create databases. The
/// password is used to log in and is never part of the answer.
#[utoipa::path(operation_id = "check_project_postgres", get, path = "/projects/{name}/postgres", tag = "databases",
    params(("name" = String, Path, description = "Project name")),
    responses(
        (status = OK, description = "Success (problems are findings in `check`)", body = DbHealth),
        (status = NOT_FOUND, description = "No such project", body = ErrorBody),
    ))]
pub(super) async fn health(
    State(svc): State<Service>,
    Params(name): Params<String>,
) -> ApiResult<Json<DbHealth>> {
    let (cfg, project) = svc.project(&name)?;
    let config = &project.config;
    let target = PgTarget::for_project(&config.odoo_options, &config.env);
    let requirement = config
        .resolve_sources(&cfg)
        .ok()
        .map(|(c, _)| Requirement::for_source(&c.id, &c.path, c.odoo_version));
    let opts = svc.env_options();
    Ok(Json(
        db::health(
            &target,
            requirement.as_ref(),
            opts.package_manager,
            opts.timeout,
        )
        .await,
    ))
}
