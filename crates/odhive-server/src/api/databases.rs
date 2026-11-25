//! `/projects/{name}/postgres` and `/projects/{name}/databases`: the
//! PostgreSQL server a project's Odoo connects to (with the project's
//! `db_*` options), and the databases on it.

use std::ffi::OsString;
use std::fmt;
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;

use axum::Json;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use odhive_core::conf::{DATA_DIR, MASK};
use odhive_core::db::archive::{self, ExtractLimits};
use odhive_core::db::{
    self, DEFAULT_LANGUAGE, DEFAULT_LOGIN, DatabaseList, DbHealth, DeleteError, DuplicateError,
    DuplicateEvent, DuplicateRequest, DuplicateStep, NewDatabase, OdooCommand, PgTarget, PgTools,
    RestoreError, RestoreOptions, RestoreRequest,
};
use odhive_core::envcheck::Requirement;
use odhive_core::launch;
use odhive_core::ports::PortOwner;
use odhive_core::project::ProjectConfig;
use odhive_core::venv::{VENV_DIR, venv_python};
use serde::Deserialize;
use utoipa::{IntoParams, ToSchema};

use super::error::{ApiError, ApiResult, ErrorBody};
use super::{Body, Params, QueryArgs};
use crate::jobs::{Job, JobKind, JobResult, JobStep};
use crate::security::UploadLimits;
use crate::service::Service;

/// Folder of the Odoo filestores inside the data folder.
const FILESTORE_DIR: &str = "filestore";

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

/// How to restore an uploaded dump.
#[derive(Debug, Clone, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct RestoreQuery {
    /// Name of the new database: letters, digits, `_`, `.`, `-`, 2-63
    /// characters, starting with a letter or digit.
    pub name: String,
    /// Run Odoo's neutralization (mail servers off, crons off, …). Default
    /// `true`; `false` restores the database as it was.
    pub neutralize: Option<bool>,
    /// Restore as a copy, with a new database UUID (default `true`); `false`
    /// keeps the UUID, for a database that was moved.
    pub copy: Option<bool>,
}

/// Restores a database dump sent as the request body
/// (`application/octet-stream`): an Odoo backup zip (`dump.sql`,
/// `filestore/`, `manifest.json`), a `pg_dump` custom (`.dump`) or tar
/// archive, or a plain SQL script. The upload is streamed to disk inside the
/// project (at most 32 GiB, cut after 60 s without data) and checked before
/// the job starts. The job unpacks a zip safely (no paths leaving the
/// folder, no links, no zip bombs), creates the database, loads it with
/// `psql` (meta-commands locked) or `pg_restore`, checks that it is Odoo of
/// the project's series, neutralizes it unless `neutralize=false`, moves the
/// filestore to `data/filestore/<name>`, and loads it once in Odoo. A
/// failed or cancelled restore removes the database again. A dump is SQL run
/// as the project's role: restore only dumps you trust.
#[utoipa::path(operation_id = "restore_project_database", post, path = "/projects/{name}/databases/restore", tag = "databases",
    request_body(content = Vec<u8>, content_type = "application/octet-stream", description = "The dump file"),
    params(("name" = String, Path, description = "Project name"), RestoreQuery),
    responses(
        (status = ACCEPTED, description = "Restore started", body = Job),
        (status = BAD_REQUEST, description = "Invalid name, or the file is not a dump OdHive restores", body = ErrorBody),
        (status = NOT_FOUND, description = "No such project", body = ErrorBody),
        (status = REQUEST_TIMEOUT, description = "The upload stalled", body = ErrorBody),
        (status = CONFLICT, description = "The database or its filestore exists, the project has no venv or is being set up, the PostgreSQL client tools are missing or too old", body = ErrorBody),
        (status = PAYLOAD_TOO_LARGE, description = "The file is over the upload limit", body = ErrorBody),
        (status = SERVICE_UNAVAILABLE, description = "PostgreSQL cannot be used", body = ErrorBody),
    ))]
pub(super) async fn restore(
    State(svc): State<Service>,
    Params(name): Params<String>,
    QueryArgs(q): QueryArgs<RestoreQuery>,
    limits: Option<Extension<UploadLimits>>,
    body: axum::body::Body,
) -> ApiResult<(StatusCode, Json<Job>)> {
    let limits = limits.map(|Extension(l)| l).unwrap_or_default();
    db::validate_new_db_name(&q.name)?;
    let (cfg, project) = svc.project(&name)?;
    svc.ensure_not_setting_up(&name)?;
    let (community, _) = project.config.resolve_sources(&cfg)?;
    let dir = project.dir();
    let python = venv_python(&dir.join(VENV_DIR));
    if !python.is_file() {
        return Err(launch::LaunchError::NoVenv(dir.join(VENV_DIR), name.clone()).into());
    }
    let config = &project.config;
    let options = &config.odoo_options;
    let target = PgTarget::for_project(options, &config.env);
    let timeout = svc.env_options().timeout;
    let path_var = config
        .env
        .0
        .get("PATH")
        .map(OsString::from)
        .or_else(|| std::env::var_os("PATH"));
    let tools = PgTools::find(
        db::pg_path_option(options).as_deref(),
        path_var.as_deref(),
        timeout,
    )
    .await?;
    if db::exists(&target, &q.name, timeout).await? {
        return Err(db::CreateError::Exists(q.name).into());
    }
    let data_dir = dir.join(DATA_DIR);
    let filestore_root = data_dir.join(FILESTORE_DIR);
    if filestore_root.join(&q.name).symlink_metadata().is_ok() {
        return Err(RestoreError::FilestoreExists(filestore_root.join(&q.name)).into());
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&data_dir)
        .map_err(|e| ApiError::internal(format!("cannot create {}: {e}", data_dir.display())))?;
    // Removed when the job ends, however it ends (or now, on an error).
    let staging = tempfile::Builder::new()
        .prefix(".odhive-restore-")
        .tempdir_in(&data_dir)
        .map_err(|e| ApiError::internal(format!("cannot create a staging folder: {e}")))?;
    let upload = staging.path().join("upload");
    receive(body, &upload, limits).await?;
    let format = archive::detect(&upload)?;
    if format == archive::DumpFormat::Zip {
        // Names, links and sizes from the zip's directory: an unsafe archive
        // is refused before any job starts.
        let zip = upload.clone();
        tokio::task::spawn_blocking(move || {
            archive::check_odoo_zip(&zip, ExtractLimits::default())
        })
        .await
        .map_err(|e| ApiError::internal(e.to_string()))??;
    }
    if format.uses_psql() && !tools.psql_can_restrict() {
        return Err(RestoreError::PsqlTooOld {
            version: tools
                .psql_version
                .map_or_else(|| "unknown".into(), |(a, b)| format!("{a}.{b}")),
        }
        .into());
    }
    let prepared = launch::prepare(&cfg, svc.paths(), &project, &[]).await?;
    let odoo = OdooCommand {
        python,
        odoo_bin: community.path.join("odoo-bin"),
        conf: prepared.conf,
        cwd: dir.to_path_buf(),
        env: config.env.clone(),
    };
    let opts = RestoreOptions {
        filestore_root,
        staging: staging.path().to_path_buf(),
        series: Some(community.odoo_version.to_string()),
        template: db::template_option(options),
        limits: ExtractLimits::default(),
        timeout,
    };
    let req = RestoreRequest {
        name: q.name.clone(),
        neutralize: q.neutralize.unwrap_or(true),
        copy: q.copy.unwrap_or(true),
    };
    let kind = JobKind::RestoreDatabase {
        project: name,
        database: q.name,
    };
    let job = svc.jobs().spawn(kind, move |report| {
        Box::pin(async move {
            let _staging = staging;
            let database = db::restore(&target, &odoo, &tools, &upload, &req, &opts, |e| {
                report.step(JobStep::RestoreDatabase(e));
            })
            .await?;
            Ok(JobResult::RestoreDatabase { database })
        })
    })?;
    Ok((StatusCode::ACCEPTED, Json(job)))
}

/// Confirms a database deletion.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DeleteDatabase {
    /// The database name, typed again exactly.
    #[schema(example = "shop_main")]
    pub confirm: String,
}

/// Starts deleting a database the project lists, with its filestore, as
/// Odoo's database manager does (`odoo-bin db drop`: other connections are
/// ended, the database dropped, `data/filestore/<name>` removed). `confirm`
/// must be the database name, typed exactly. Refused while an instance of
/// the project runs on it (stop it first) or another job works on it.
/// Follow the job with `/jobs/{id}` or the WebSocket. There is no undo.
#[utoipa::path(operation_id = "delete_project_database", delete, path = "/projects/{name}/databases/{database}", tag = "databases",
    request_body = DeleteDatabase,
    params(
        ("name" = String, Path, description = "Project name"),
        ("database" = String, Path, description = "Database name"),
    ),
    responses(
        (status = ACCEPTED, description = "Deletion started", body = Job),
        (status = BAD_REQUEST, description = "Invalid name, or the confirmation is not the name", body = ErrorBody),
        (status = NOT_FOUND, description = "No such project, or the project does not list this database", body = ErrorBody),
        (status = CONFLICT, description = "An instance runs on the database, a job works on it, the project has no venv or is being set up", body = ErrorBody),
        (status = SERVICE_UNAVAILABLE, description = "PostgreSQL cannot be used", body = ErrorBody),
    ))]
pub(super) async fn delete(
    State(svc): State<Service>,
    Params((name, database)): Params<(String, String)>,
    Body(req): Body<DeleteDatabase>,
) -> ApiResult<(StatusCode, Json<Job>)> {
    odhive_core::names::validate_db_name(&database).map_err(DeleteError::from)?;
    db::check_confirmation(&database, &req.confirm)?;
    let (cfg, project) = svc.project(&name)?;
    svc.ensure_not_setting_up(&name)?;
    svc.ensure_database_idle(&name, &database)?;
    let (community, _) = project.config.resolve_sources(&cfg)?;
    let dir = project.dir();
    let python = venv_python(&dir.join(VENV_DIR));
    if !python.is_file() {
        return Err(launch::LaunchError::NoVenv(dir.join(VENV_DIR), name.clone()).into());
    }
    let running: Vec<String> = svc
        .instances_on(&project.config, &database)?
        .into_iter()
        .map(|i| i.instance)
        .collect();
    if !running.is_empty() {
        return Err(DeleteError::InUse {
            name: database,
            instances: running,
        }
        .into());
    }
    let config = &project.config;
    let target = PgTarget::for_project(&config.odoo_options, &config.env);
    let opts = svc.env_options();
    if !lists(&svc, config, &target, &database).await? {
        return Err(DeleteError::NotFound(database).into());
    }
    let prepared = launch::prepare(&cfg, svc.paths(), &project, &[]).await?;
    let odoo = OdooCommand {
        python,
        odoo_bin: community.path.join("odoo-bin"),
        conf: prepared.conf,
        cwd: dir.to_path_buf(),
        env: config.env.clone(),
    };
    let filestore_root = dir.join(DATA_DIR).join(FILESTORE_DIR);
    let timeout = opts.timeout;
    let kind = JobKind::DeleteDatabase {
        project: name,
        database: database.clone(),
    };
    let job = svc.jobs().spawn(kind, move |report| {
        Box::pin(async move {
            let database = db::delete(&target, &odoo, &database, &filestore_root, timeout, |e| {
                report.step(JobStep::DeleteDatabase(e));
            })
            .await?;
            Ok(JobResult::DeleteDatabase { database })
        })
    })?;
    Ok((StatusCode::ACCEPTED, Json(job)))
}

/// Whether the project lists `database`: owned by its role, not a template
/// nor `postgres`, passing its `dbfilter`.
async fn lists(
    svc: &Service,
    config: &ProjectConfig,
    target: &PgTarget,
    database: &str,
) -> ApiResult<bool> {
    let opts = svc.env_options();
    let listed = db::list(
        target,
        config.odoo_options.get("dbfilter"),
        &[],
        opts.package_manager,
        opts.timeout,
    )
    .await;
    if let Some(check) = listed.error {
        return Err(DuplicateError::Server(check.summary).into());
    }
    Ok(listed.databases.iter().any(|d| d.name == database))
}

/// Copies a database.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DuplicateDatabase {
    /// Name of the copy: letters, digits, `_`, `.`, `-`, 2-63 characters,
    /// starting with a letter or digit.
    #[schema(example = "shop_test")]
    pub name: String,
    /// Run Odoo's neutralization on the copy (mail servers off, crons off,
    /// …). Default `false`.
    #[serde(default)]
    pub neutralize: bool,
    /// Stop the project's instances running on the source first (and start
    /// those run by OdHive again afterwards). Without it, running instances
    /// refuse the copy with `database_in_use`.
    #[serde(default)]
    pub stop_instances: bool,
}

/// Starts copying a database the project lists, with its filestore, as
/// Odoo's database manager does (`odoo-bin db duplicate`: other connections
/// to the source are ended, the copy made from it as a template, given a
/// new database UUID and neutralized when asked, `data/filestore/<source>`
/// copied). PostgreSQL copies only a database nobody is connected to, so
/// instances of the project running on the source are refused with
/// `database_in_use` (it lists them) unless `stop_instances` is set: the job
/// then stops them first and starts the ones OdHive runs again afterwards.
/// Follow the job with `/jobs/{id}` or the WebSocket. A failed or cancelled
/// copy is removed again; the source is never changed.
#[utoipa::path(operation_id = "duplicate_project_database", post, path = "/projects/{name}/databases/{database}/duplicate", tag = "databases",
    request_body = DuplicateDatabase,
    params(
        ("name" = String, Path, description = "Project name"),
        ("database" = String, Path, description = "Database to copy"),
    ),
    responses(
        (status = ACCEPTED, description = "Copy started", body = Job),
        (status = BAD_REQUEST, description = "Invalid name", body = ErrorBody),
        (status = NOT_FOUND, description = "No such project, or the project does not list this database", body = ErrorBody),
        (status = CONFLICT, description = "Instances run on the source, the copy or its filestore exists, a job works on either database, the project has no venv or is being set up", body = ErrorBody),
        (status = SERVICE_UNAVAILABLE, description = "PostgreSQL cannot be used", body = ErrorBody),
    ))]
pub(super) async fn duplicate(
    State(svc): State<Service>,
    Params((name, database)): Params<(String, String)>,
    Body(req): Body<DuplicateDatabase>,
) -> ApiResult<(StatusCode, Json<Job>)> {
    let dup = DuplicateRequest {
        source: database,
        name: req.name,
        neutralize: req.neutralize,
    };
    dup.check()?;
    let (cfg, project) = svc.project(&name)?;
    svc.ensure_not_setting_up(&name)?;
    svc.ensure_database_idle(&name, &dup.source)?;
    svc.ensure_database_idle(&name, &dup.name)?;
    let (community, _) = project.config.resolve_sources(&cfg)?;
    let dir = project.dir();
    let python = venv_python(&dir.join(VENV_DIR));
    if !python.is_file() {
        return Err(launch::LaunchError::NoVenv(dir.join(VENV_DIR), name.clone()).into());
    }
    let config = &project.config;
    let target = PgTarget::for_project(&config.odoo_options, &config.env);
    let timeout = svc.env_options().timeout;
    if !lists(&svc, config, &target, &dup.source).await? {
        return Err(DuplicateError::NotFound(dup.source).into());
    }
    let users = svc.instances_on(config, &dup.source)?;
    if !users.is_empty() && !req.stop_instances {
        return Err(DuplicateError::InUse {
            name: dup.source,
            instances: users.into_iter().map(|u| u.instance).collect(),
        }
        .into());
    }
    if db::exists(&target, &dup.name, timeout).await? {
        return Err(DuplicateError::Exists(dup.name).into());
    }
    let filestore_root = dir.join(DATA_DIR).join(FILESTORE_DIR);
    if filestore_root.join(&dup.name).symlink_metadata().is_ok() {
        return Err(DuplicateError::FilestoreExists(filestore_root.join(&dup.name)).into());
    }
    let prepared = launch::prepare(&cfg, svc.paths(), &project, &[]).await?;
    let odoo = OdooCommand {
        python,
        odoo_bin: community.path.join("odoo-bin"),
        conf: prepared.conf,
        cwd: dir.to_path_buf(),
        env: config.env.clone(),
    };
    let kind = JobKind::DuplicateDatabase {
        project: name.clone(),
        database: dup.name.clone(),
        source: dup.source.clone(),
    };
    let job_svc = svc.clone();
    let job = svc.jobs().spawn(kind, move |report| {
        Box::pin(async move {
            let svc = job_svc;
            let step = |e: DuplicateEvent| report.step(JobStep::DuplicateDatabase(e));
            let line = |line: String| step(DuplicateEvent::Output { line });
            let mut stopped = Vec::new();
            let mut to_restart = Vec::new();
            if !users.is_empty() {
                step(DuplicateEvent::Step {
                    step: DuplicateStep::StopInstances,
                });
                for user in &users {
                    let owner = PortOwner::new(&name, &user.instance);
                    svc.stop(&owner).await?;
                    line(format!("stopped instance {owner}"));
                    stopped.push(user.instance.clone());
                    if user.managed {
                        to_restart.push(user.instance.clone());
                    }
                }
            }
            let copied = db::duplicate(&target, &odoo, &dup, &filestore_root, timeout, &step).await;
            let mut restarted = Vec::new();
            if !to_restart.is_empty() {
                step(DuplicateEvent::Step {
                    step: DuplicateStep::RestartInstances,
                });
                for id in to_restart {
                    match svc.start(&name, &id, false).await {
                        Ok(_) => {
                            line(format!("started instance {name}/{id} again"));
                            restarted.push(id);
                        }
                        Err(e) => line(format!(
                            "could not start instance {name}/{id} again: {}",
                            e.message
                        )),
                    }
                }
            }
            Ok(JobResult::DuplicateDatabase {
                database: copied?,
                stopped,
                restarted,
            })
        })
    })?;
    Ok((StatusCode::ACCEPTED, Json(job)))
}

/// Streams the request body to a new private file at `path`, at most
/// `limits.max_bytes`, giving up after `limits.idle` without data.
async fn receive(body: axum::body::Body, path: &Path, limits: UploadLimits) -> ApiResult<u64> {
    use http_body_util::BodyExt;
    use tokio::io::AsyncWriteExt;
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .await
        .map_err(|e| ApiError::internal(format!("cannot create {}: {e}", path.display())))?;
    let mut body = body;
    let mut total: u64 = 0;
    loop {
        let frame = match tokio::time::timeout(limits.idle, body.frame()).await {
            Err(_) => {
                return Err(ApiError::new(
                    StatusCode::REQUEST_TIMEOUT,
                    "upload_timeout",
                    format!("no data received for {} s", limits.idle.as_secs()),
                ));
            }
            Ok(None) => break,
            Ok(Some(Err(e))) => {
                return Err(ApiError::bad_request(
                    "upload_failed",
                    format!("the upload broke off: {e}"),
                ));
            }
            Ok(Some(Ok(frame))) => frame,
        };
        let Ok(data) = frame.into_data() else {
            continue;
        };
        total += data.len() as u64;
        if total > limits.max_bytes {
            return Err(ApiError::body_too_large(limits.max_bytes));
        }
        file.write_all(&data)
            .await
            .map_err(|e| ApiError::internal(format!("cannot write the upload: {e}")))?;
    }
    file.flush()
        .await
        .map_err(|e| ApiError::internal(format!("cannot write the upload: {e}")))?;
    Ok(total)
}
