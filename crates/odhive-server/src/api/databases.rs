//! `/projects/{name}/postgres` and `/projects/{name}/databases`: the
//! PostgreSQL server a project's Odoo connects to (with the project's
//! `db_*` options), and the databases on it.

use std::fmt;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use odhive_core::conf::MASK;
use odhive_core::db::{
    self, DEFAULT_LANGUAGE, DEFAULT_LOGIN, DatabaseList, DbHealth, NewDatabase, OdooCommand,
    PgTarget,
};
use odhive_core::envcheck::Requirement;
use odhive_core::launch;
use odhive_core::venv::{VENV_DIR, venv_python};
use serde::Deserialize;
use utoipa::ToSchema;

use super::error::{ApiError, ApiResult, ErrorBody};
use super::{Body, Params};
use crate::jobs::{Job, JobKind, JobResult, JobStep};
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

/// A database to create.
#[derive(Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateDatabase {
    /// Database name: letters, digits, `_`, `.`, `-`, 2-63 characters,
    /// starting with a letter or digit.
    #[schema(example = "shop_main")]
    pub name: String,
    /// Language, e.g. `fr_BE`; also the admin's. Default `en_US`.
    #[serde(default)]
    pub language: Option<String>,
    /// Two-letter country code of the main company (sets its currency too).
    #[serde(default)]
    pub country: Option<String>,
    /// Install demo data.
    #[serde(default)]
    pub demo: bool,
    /// Admin login. Default `admin`.
    #[serde(default)]
    pub login: Option<String>,
    /// Admin password. Never part of a command line, a log or an answer.
    #[schema(format = Password)]
    pub password: String,
    /// Modules to install besides `base` (technical names).
    #[serde(default)]
    pub modules: Vec<String>,
}

impl fmt::Debug for CreateDatabase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CreateDatabase")
            .field("name", &self.name)
            .field("language", &self.language)
            .field("country", &self.country)
            .field("demo", &self.demo)
            .field("login", &self.login)
            .field("password", &MASK)
            .field("modules", &self.modules)
            .finish()
    }
}

/// Starts creating a database with the project's `odoo-bin`, as Odoo's
/// database manager does: `db init` (language, country, admin login, demo
/// data), then `-i <modules> --stop-after-init`, then a check that it all
/// took and the admin password. Follow it with `/jobs/{id}` or the
/// WebSocket. A failed or cancelled creation removes the database again.
#[utoipa::path(operation_id = "create_project_database", post, path = "/projects/{name}/databases", tag = "databases",
    request_body = CreateDatabase,
    params(("name" = String, Path, description = "Project name")),
    responses(
        (status = ACCEPTED, description = "Creation started", body = Job),
        (status = BAD_REQUEST, description = "Invalid request or unknown module", body = ErrorBody),
        (status = NOT_FOUND, description = "No such project", body = ErrorBody),
        (status = CONFLICT, description = "The database exists, the project has no venv, or it is being set up", body = ErrorBody),
        (status = SERVICE_UNAVAILABLE, description = "PostgreSQL cannot be used", body = ErrorBody),
    ))]
pub(super) async fn create(
    State(svc): State<Service>,
    Params(name): Params<String>,
    Body(req): Body<CreateDatabase>,
) -> ApiResult<(StatusCode, Json<Job>)> {
    let new = NewDatabase {
        name: req.name,
        language: req.language.unwrap_or_else(|| DEFAULT_LANGUAGE.into()),
        country: req.country,
        demo: req.demo,
        login: req.login.unwrap_or_else(|| DEFAULT_LOGIN.into()),
        password: req.password,
        modules: req.modules,
    }
    .validated()?;
    let (cfg, project) = svc.project(&name)?;
    svc.ensure_not_setting_up(&name)?;
    let (community, enterprise) = project.config.resolve_sources(&cfg)?;
    let dir = project.dir();
    let python = venv_python(&dir.join(VENV_DIR));
    if !python.is_file() {
        return Err(launch::LaunchError::NoVenv(dir.join(VENV_DIR), name.clone()).into());
    }
    let mut addons = launch::start_conf(&project.config, dir, community, enterprise)
        .ctx
        .addons_paths;
    addons.push(community.path.join("odoo").join("addons"));
    let missing = db::missing_modules(&new.modules, &addons);
    if !missing.is_empty() {
        return Err(ApiError::bad_request(
            "unknown_modules",
            format!(
                "no module named {} in the project's addons paths",
                missing.join(", ")
            ),
        ));
    }
    let target = PgTarget::for_project(&project.config.odoo_options, &project.config.env);
    let timeout = svc.env_options().timeout;
    if db::exists(&target, &new.name, timeout).await? {
        return Err(db::CreateError::Exists(new.name).into());
    }
    let prepared = launch::prepare(&cfg, svc.paths(), &project, &[]).await?;
    let odoo = OdooCommand {
        python,
        odoo_bin: community.path.join("odoo-bin"),
        conf: prepared.conf,
        cwd: dir.to_path_buf(),
        env: project.config.env.clone(),
    };
    let kind = JobKind::CreateDatabase {
        project: name,
        database: new.name.clone(),
    };
    let job = svc.jobs().spawn(kind, move |report| {
        Box::pin(async move {
            let database = db::create(&target, &odoo, &new, timeout, |e| {
                report.step(JobStep::CreateDatabase(e));
            })
            .await?;
            Ok(JobResult::CreateDatabase { database })
        })
    })?;
    Ok((StatusCode::ACCEPTED, Json(job)))
}
