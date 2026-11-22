//! `/projects`: registered projects, their instances and option lists.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use odhive_core::config::{GlobalConfig, ProjectEntry};
use odhive_core::launch::{self, LoadedProject, SCHEMA_CACHE_DIR};
use odhive_core::manager::InstanceInfo;
use odhive_core::ports::PortOwner;
use odhive_core::project::{AutofixMode, ReloadMode};
use odhive_core::schema;
use odhive_core::setup::{self, NewProject};
use odhive_core::venv::{VENV_DIR, venv_python};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::error::{ApiError, ApiResult, ErrorBody};
use super::sources::{SCHEMA_TIMEOUT, SchemaInfo, absolute};
use super::{Body, Params};
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
}

/// An instance that runs outside this OdHive service, e.g. one started by
/// `odhive start` in a terminal. It can be stopped, not restarted, here.
#[derive(Debug, Clone, Serialize, ToSchema)]
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
#[derive(Debug, Clone, Serialize, ToSchema)]
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
#[derive(Debug, Clone, Serialize, ToSchema)]
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
        has_venv: venv_python(&p.dir().join(VENV_DIR)).is_file(),
        instances,
    })
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

/// Creates a project folder in the workspace with one `main` instance. Its
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
    let mut running: Vec<String> = svc
        .manager()
        .list_project(&name)
        .into_iter()
        .filter(|i| !i.is_done())
        .map(|i| i.owner.to_string())
        .collect();
    running.extend(
        launch::running_records(&svc.run_dir())?
            .iter()
            .filter(|r| r.project == name)
            .map(|r| r.owner().to_string()),
    );
    running.sort();
    running.dedup();
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
    let (community, _) = project.config.resolve_sources(&cfg)?;
    let python = venv_python(&project.dir().join(VENV_DIR));
    let loaded = schema::load_schema(
        community,
        python.is_file().then_some(python.as_path()),
        &svc.paths().data_dir.join(SCHEMA_CACHE_DIR),
        SCHEMA_TIMEOUT,
    )
    .await?;
    Ok(Json(loaded.into()))
}
