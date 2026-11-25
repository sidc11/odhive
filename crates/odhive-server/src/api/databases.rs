//! `/projects/{name}/postgres` and `/projects/{name}/databases`: the
//! PostgreSQL server a project's Odoo connects to (with the project's
//! `db_*` options), and the databases on it.

use axum::Json;
use axum::extract::State;
use odhive_core::db::{self, DatabaseList, DbHealth, PgTarget};
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

/// Lists the databases the project's Odoo would offer: owned by its role,
/// not templates, and passing its `dbfilter` (matched like Odoo's
/// `re.match`; a filter using `%h`/`%d` or one OdHive cannot read is not
/// applied and `filter_note` says why). Each comes with its size, `base`
/// module version, latest change and the project's instances using it. A
/// server that cannot be used is reported in `error`.
#[utoipa::path(operation_id = "list_project_databases", get, path = "/projects/{name}/databases", tag = "databases",
    params(("name" = String, Path, description = "Project name")),
    responses(
        (status = OK, description = "Success (a server that cannot be used is reported in `error`)", body = DatabaseList),
        (status = NOT_FOUND, description = "No such project", body = ErrorBody),
    ))]
pub(super) async fn list(
    State(svc): State<Service>,
    Params(name): Params<String>,
) -> ApiResult<Json<DatabaseList>> {
    let (_, project) = svc.project(&name)?;
    let config = &project.config;
    let target = PgTarget::for_project(&config.odoo_options, &config.env);
    let uses: Vec<(String, String)> = config
        .instances
        .iter()
        .map(|i| (i.id.clone(), i.db_name(&config.name)))
        .collect();
    let opts = svc.env_options();
    Ok(Json(
        db::list(
            &target,
            config.odoo_options.get("dbfilter"),
            &uses,
            opts.package_manager,
            opts.timeout,
        )
        .await,
    ))
}
