//! `/projects`: registered projects, their instances and option lists.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use odhive_core::config::{GlobalConfig, ProjectEntry};
use odhive_core::git::{self, GitHead};
use odhive_core::launch::{self, LoadedProject, SCHEMA_CACHE_DIR};
use odhive_core::manager::InstanceInfo;
use odhive_core::ports::PortOwner;
use odhive_core::project::{AutofixMode, DEFAULT_INSTANCE, Instance, ReloadMode};
use odhive_core::schema;
use odhive_core::setup::{self, EnvOptions, NewProject};
use odhive_core::venv::{VENV_DIR, venv_python};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::error::{ApiError, ApiResult, ErrorBody};
use super::sources::{SCHEMA_TIMEOUT, SchemaInfo, absolute};
use super::{Body, Params};
use crate::jobs::{Job, JobKind, JobResult, JobStep};
use crate::service::Service;

/// A project to create.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateProject {
    /// Project name (`^[a-z0-9][a-z0-9_-]{0,63}$`).
    #[schema(example = "shop")]
    pub name: String,
    /// Registered Community source id.
    #[schema(example = "odoo-19.0")]
    pub community: String,
    /// Registered Enterprise source id of the same series.
    #[serde(default)]
    pub enterprise: Option<String>,
    /// Existing addons folders (absolute paths), used after the project's
    /// own `addons/`.
    #[serde(default)]
    pub addons_paths: Vec<String>,
    /// Python interpreter (absolute path) for the project's venv; omitted =
    /// detected at setup.
    #[serde(default)]
    pub python: Option<String>,
    /// The first instance; omitted = `main` with automatic ports and the
    /// database `<project>_main`.
    #[serde(default)]
    pub instance: Option<NewInstance>,
}

/// Settings of a project's first instance.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewInstance {
    /// Instance id (same rules as a project name); omitted = `main`.
    #[serde(default)]
    #[schema(example = "main")]
    pub id: Option<String>,
    /// Database; omitted = `<project>_<instance>`.
    #[serde(default)]
    pub db: Option<String>,
    /// Pinned HTTP port (never moved; the start fails while it is taken);
    /// omitted = the first free port from 8069 at each start.
    #[serde(default)]
    pub http_port: Option<u16>,
}

impl NewInstance {
    fn into_instance(self) -> Instance {
        let mut instance = Instance::new(self.id.unwrap_or_else(|| DEFAULT_INSTANCE.to_owned()));
        instance.db = self.db;
        instance.http_port = self.http_port;
        instance
    }
}

/// An instance that runs outside this OdHive service, e.g. one started by
/// `odhive start` in a terminal. It can be stopped, not restarted, here.
#[derive(Debug, Clone, Serialize, serde::Deserialize, ToSchema)]
pub struct ExternalRun {
    /// Process id.
    pub pid: u32,
    /// The OdHive process supervising it; `None` = nobody captures its
    /// output.
    pub supervisor: Option<u32>,
    /// HTTP port.
    pub http_port: u16,
}

/// One instance of a project: its settings and, while it runs, its state.
#[derive(Debug, Clone, Serialize, serde::Deserialize, ToSchema)]
pub struct InstanceView {
    /// Instance id.
    pub id: String,
    /// Database.
    pub db: String,
    /// Pinned HTTP port; `None` = allocated at start.
    pub http_port: Option<u16>,
    /// Pinned gevent port; `None` = allocated at start.
    pub gevent_port: Option<u16>,
    /// Odoo `--workers`.
    pub workers: u32,
    /// Smart Reload mode.
    pub reload_mode: ReloadMode,
    /// Dependency auto-fix mode.
    pub autofix_mode: AutofixMode,
    /// State while this service runs it (also after it ended, until it is
    /// stopped or started again).
    pub live: Option<InstanceInfo>,
    /// Set when it runs outside this service.
    pub external: Option<ExternalRun>,
}

/// A registered project.
#[derive(Debug, Clone, Serialize, serde::Deserialize, ToSchema)]
pub struct Project {
    /// Name.
    pub name: String,
    /// Project folder.
    pub path: String,
    /// Why the project could not be read; the fields below are empty then.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Odoo series of its Community source, when that is registered.
    pub odoo_version: Option<String>,
    /// Community source id.
    pub community: Option<String>,
    /// Enterprise source id.
    pub enterprise: Option<String>,
    /// Custom addons folders, as configured.
    pub addons_paths: Vec<String>,
    /// Git checkouts holding the custom addons folders, in addons-path order
    /// (each repository once).
    pub git: Vec<GitHead>,
    /// The project's Python environment exists.
    pub has_venv: bool,
    /// Instances, in project order.
    pub instances: Vec<InstanceView>,
}

fn broken(entry: &ProjectEntry, error: String) -> Project {
    Project {
        name: entry.name.clone(),
        path: entry.path.display().to_string(),
        error: Some(error),
        odoo_version: None,
        community: None,
        enterprise: None,
        addons_paths: Vec::new(),
        git: Vec::new(),
        has_venv: false,
        instances: Vec::new(),
    }
}

fn view(svc: &Service, cfg: &GlobalConfig, p: &LoadedProject) -> ApiResult<Project> {
    let c = &p.config;
    let mut instances = Vec::with_capacity(c.instances.len());
    for i in &c.instances {
        let owner = PortOwner::new(&c.name, &i.id);
        let external = svc.external(&owner)?.map(|e| ExternalRun {
            pid: e.pid,
            supervisor: e.supervisor,
            http_port: e.record.http_port,
        });
        instances.push(InstanceView {
            id: i.id.clone(),
            db: i.db_name(&c.name),
            http_port: i.http_port,
            gevent_port: i.gevent_port,
            workers: i.workers,
            reload_mode: i.reload_mode,
            autofix_mode: i.autofix_mode,
            live: svc.manager().get(&owner),
            external,
        });
    }
    Ok(Project {
        name: c.name.clone(),
        path: p.dir().display().to_string(),
        error: None,
        odoo_version: c
            .resolve_sources(cfg)
            .ok()
            .map(|(community, _)| community.odoo_version.to_string()),
        community: Some(c.sources.community.clone()),
        enterprise: c.sources.enterprise.clone(),
        addons_paths: c
            .addons_paths
            .iter()
            .map(|a| a.display().to_string())
            .collect(),
        git: git_heads(&c.resolved_addons_paths(p.dir())),
        has_venv: venv_python(&p.dir().join(VENV_DIR)).is_file(),
        instances,
    })
}

/// The repositories `dirs` are in, each once.
fn git_heads(dirs: &[std::path::PathBuf]) -> Vec<GitHead> {
    let mut out: Vec<GitHead> = Vec::new();
    for head in dirs.iter().filter_map(|d| git::head(d)) {
        if !out.iter().any(|h| h.repo == head.repo) {
            out.push(head);
        }
    }
    out
}

/// Registered projects. A project whose files cannot be read is listed with
/// `error` set.
#[utoipa::path(operation_id = "list_projects", get, path = "/projects", tag = "projects",
    responses((status = OK, description = "Success", body = Vec<Project>)))]
pub(super) async fn list(State(svc): State<Service>) -> ApiResult<Json<Vec<Project>>> {
    let cfg = svc.config()?;
    let mut out = Vec::with_capacity(cfg.projects.len());
    for entry in &cfg.projects {
        out.push(match launch::load_project(&cfg, &entry.name) {
            Ok(p) => view(&svc, &cfg, &p)?,
            Err(e) => broken(entry, e.to_string()),
        });
    }
    Ok(Json(out))
}

/// One project.
#[utoipa::path(operation_id = "get_project", get, path = "/projects/{name}", tag = "projects",
    params(("name" = String, Path, description = "Project name")),
    responses(
        (status = OK, description = "Success", body = Project),
        (status = NOT_FOUND, description = "Not found", body = ErrorBody),
    ))]
pub(super) async fn get(
    State(svc): State<Service>,
    Params(name): Params<String>,
) -> ApiResult<Json<Project>> {
    let (cfg, project) = svc.project(&name)?;
    Ok(Json(view(&svc, &cfg, &project)?))
}

/// Creates a project folder in the workspace with one instance (`main`
/// unless `instance` names it). Its
/// Python environment is not set up yet (`has_venv` is false).
#[utoipa::path(operation_id = "create_project", post, path = "/projects", tag = "projects", request_body = CreateProject,
    responses(
        (status = CREATED, description = "Created", body = Project),
        (status = BAD_REQUEST, description = "Invalid request", body = ErrorBody),
        (status = NOT_FOUND, description = "Unknown source", body = ErrorBody),
        (status = CONFLICT, description = "The name or folder is taken", body = ErrorBody),
        (status = UNPROCESSABLE_ENTITY, description = "Invalid input", body = ErrorBody),
    ))]
pub(super) async fn create(
    State(svc): State<Service>,
    Body(req): Body<CreateProject>,
) -> ApiResult<(StatusCode, Json<Project>)> {
    let addons_paths = req
        .addons_paths
        .iter()
        .map(|p| absolute("addons_paths", p))
        .collect::<ApiResult<Vec<_>>>()?;
    let python = req
        .python
        .as_deref()
        .map(|p| absolute("python", p))
        .transpose()?;
    let ws = svc.workspace().clone();
    let (cfg, project) = svc
        .edit_config(|cfg| {
            let community = registered(cfg, &req.community)?;
            if let Some(e) = &req.enterprise {
                registered(cfg, e)?;
            }
            let new = NewProject {
                name: req.name.clone(),
                community,
                enterprise: req.enterprise.clone(),
                addons_paths,
                python,
                instance: req.instance.clone().unwrap_or_default().into_instance(),
            };
            let project = setup::create_project(cfg, &ws, &new)?;
            Ok((cfg.clone(), project))
        })
        .await?;
    Ok((StatusCode::CREATED, Json(view(&svc, &cfg, &project)?)))
}

/// Only registered source ids are accepted; registering paths is `/sources`'
/// job.
fn registered(cfg: &GlobalConfig, id: &str) -> ApiResult<String> {
    cfg.sources
        .iter()
        .find(|s| s.id == id)
        .map(|s| s.id.clone())
        .ok_or_else(|| ApiError::not_found("unknown_source", format!("no source with id {id:?}")))
}

/// How to set up a project's Python environment.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SetupRequest {
    /// Build the venv with this interpreter (absolute path; remembered in
    /// the project). Omitted = the project's setting, else the newest
    /// suitable one found.
    #[serde(default)]
    pub python: Option<String>,
    /// Replace an existing venv.
    #[serde(default)]
    pub recreate: bool,
    /// Skip installing requirements.
    #[serde(default)]
    pub skip_requirements: bool,
}

/// Starts setting up the project's Python environment: creates `.venv`
/// (kept when it exists, unless `recreate`) and installs the requirements
/// of Odoo and of the project's modules. Follow it with `/jobs/{id}` or the
/// WebSocket. Refused while an instance of the project runs.
#[utoipa::path(operation_id = "setup_project", post, path = "/projects/{name}/setup", tag = "projects", request_body = SetupRequest,
    params(("name" = String, Path, description = "Project name")),
    responses(
        (status = ACCEPTED, description = "Setup started", body = Job),
        (status = BAD_REQUEST, description = "Invalid request", body = ErrorBody),
        (status = NOT_FOUND, description = "Not found", body = ErrorBody),
        (status = CONFLICT, description = "An instance runs or a setup runs", body = ErrorBody),
        (status = UNPROCESSABLE_ENTITY, description = "Invalid project", body = ErrorBody),
    ))]
pub(super) async fn setup(
    State(svc): State<Service>,
    Params(name): Params<String>,
    Body(req): Body<SetupRequest>,
) -> ApiResult<(StatusCode, Json<Job>)> {
    let python = req
        .python
        .as_deref()
        .map(|p| absolute("python", p))
        .transpose()?;
    let (cfg, mut project) = svc.project(&name)?;
    project.config.resolve_sources(&cfg)?;
    let running = svc.running_instances(&name)?;
    if !running.is_empty() {
        return Err(ApiError::conflict(
            "running",
            format!("{} still running; stop it first", running.join(", ")),
        ));
    }
    let opts = EnvOptions {
        python,
        discover: svc.discover_options().clone(),
        recreate: req.recreate,
        install: !req.skip_requirements,
    };
    let kind = JobKind::Setup {
        project: name.clone(),
    };
    let job = svc.jobs().spawn(kind, move |report| {
        Box::pin(async move {
            let report = setup::setup_env(&cfg, &mut project, &opts, |e| {
                report.step(JobStep::Setup(e));
            })
            .await?;
            Ok(JobResult::Setup { report })
        })
    })?;
    Ok((StatusCode::ACCEPTED, Json(job)))
}

/// Unregisters a project; its folder is kept. Refused while an instance
/// runs.
#[utoipa::path(operation_id = "remove_project", delete, path = "/projects/{name}", tag = "projects",
    params(("name" = String, Path, description = "Project name")),
    responses(
        (status = NO_CONTENT, description = "Removed"),
        (status = NOT_FOUND, description = "Not found", body = ErrorBody),
        (status = CONFLICT, description = "An instance runs", body = ErrorBody),
    ))]
pub(super) async fn remove(
    State(svc): State<Service>,
    Params(name): Params<String>,
) -> ApiResult<StatusCode> {
    svc.ensure_not_setting_up(&name)?;
    let running = svc.running_instances(&name)?;
    if !running.is_empty() {
        return Err(ApiError::conflict(
            "running",
            format!("{} still running; stop it first", running.join(", ")),
        ));
    }
    svc.edit_config(|cfg| {
        setup::unregister_project(cfg, &name)
            .map(|_| ())
            .ok_or_else(|| {
                ApiError::not_found("unknown_project", format!("no project named {name:?}"))
            })
    })
    .await?;
    // Ended instances are forgotten with the project.
    for i in svc.manager().list_project(&name) {
        let _ = svc.manager().stop(&i.owner).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// The `odoo.conf` options of the project's Odoo, read with the project's
/// Python (cached per source tree); the bundled list while it has no venv.
#[utoipa::path(operation_id = "project_schema", get, path = "/projects/{name}/schema", tag = "projects",
    params(("name" = String, Path, description = "Project name")),
    responses(
        (status = OK, description = "Success", body = SchemaInfo),
        (status = NOT_FOUND, description = "Not found", body = ErrorBody),
        (status = UNPROCESSABLE_ENTITY, description = "Invalid input", body = ErrorBody),
    ))]
pub(super) async fn schema(
    State(svc): State<Service>,
    Params(name): Params<String>,
) -> ApiResult<Json<SchemaInfo>> {
    let (cfg, project) = svc.project(&name)?;
    Ok(Json(project_schema(&svc, &cfg, &project).await?.into()))
}

/// The option schema of a loaded project (see [`schema`]).
pub(super) async fn project_schema(
    svc: &Service,
    cfg: &GlobalConfig,
    project: &LoadedProject,
) -> ApiResult<schema::LoadedSchema> {
    let (community, _) = project.config.resolve_sources(cfg)?;
    let python = venv_python(&project.dir().join(VENV_DIR));
    Ok(schema::load_schema(
        community,
        python.is_file().then_some(python.as_path()),
        &svc.paths().data_dir.join(SCHEMA_CACHE_DIR),
        SCHEMA_TIMEOUT,
    )
    .await?)
}
