//! `/sources`: registered Odoo source trees.

use std::path::{Path, PathBuf};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use odhive_core::config::{Edition, SourceEntry};
use odhive_core::launch::SCHEMA_CACHE_DIR;
use odhive_core::project::ProjectConfig;
use odhive_core::schema::{self, LoadedSchema, OptionSchema, SchemaOrigin};
use odhive_core::sources;
use odhive_core::version::OdooVersion;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::error::{ApiError, ApiResult, ErrorBody};
use super::{Body, Params};
use crate::jobs::{Job, JobKind, JobResult, JobStep};
use crate::service::Service;

/// Time allowed for reading Odoo's option list.
pub(crate) const SCHEMA_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// A source tree to register.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewSource {
    /// Absolute path of an Odoo 19/20 Community tree, or of an Enterprise
    /// addons tree when `enterprise_version` is given.
    pub path: String,
    /// Registers an Enterprise tree for this Odoo series (Enterprise trees
    /// carry no version), e.g. `19.0`.
    #[serde(default)]
    pub enterprise_version: Option<String>,
}

/// Where an option list came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SchemaSource {
    /// Read from OdHive's cache of this source tree.
    Cached,
    /// Read from Odoo just now.
    Introspected,
    /// The list bundled with OdHive (see `reason`).
    Bundled,
}

/// The `odoo.conf` options of an Odoo source.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SchemaInfo {
    /// Where the list came from.
    pub source: SchemaSource,
    /// Why the bundled list is used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The options.
    pub schema: OptionSchema,
}

impl From<LoadedSchema> for SchemaInfo {
    fn from(l: LoadedSchema) -> Self {
        let (source, reason) = match l.origin {
            SchemaOrigin::Cached => (SchemaSource::Cached, None),
            SchemaOrigin::Introspected => (SchemaSource::Introspected, None),
            SchemaOrigin::Bundled { reason } => (SchemaSource::Bundled, Some(reason)),
        };
        Self {
            source,
            reason,
            schema: l.schema,
        }
    }
}

/// An absolute path from a request; relative paths have no meaning for the
/// service.
pub(crate) fn absolute(field: &str, text: &str) -> ApiResult<PathBuf> {
    let path = Path::new(text);
    if text.is_empty() || !path.is_absolute() {
        return Err(ApiError::bad_request(
            "bad_path",
            format!("{field} must be an absolute path"),
        ));
    }
    Ok(path.to_path_buf())
}

/// Registered source trees.
#[utoipa::path(operation_id = "list_sources", get, path = "/sources", tag = "sources",
    responses((status = OK, description = "Success", body = Vec<SourceEntry>)))]
pub(super) async fn list(State(svc): State<Service>) -> ApiResult<Json<Vec<SourceEntry>>> {
    Ok(Json(svc.config()?.sources))
}

/// Registers an existing source tree. Registering a registered path again
/// returns its entry.
#[utoipa::path(operation_id = "add_source", post, path = "/sources", tag = "sources", request_body = NewSource,
    responses(
        (status = CREATED, description = "Created", body = SourceEntry),
        (status = BAD_REQUEST, description = "Invalid request", body = ErrorBody),
        (status = UNPROCESSABLE_ENTITY, description = "Not an Odoo 19/20 tree", body = ErrorBody),
    ))]
pub(super) async fn add(
    State(svc): State<Service>,
    Body(new): Body<NewSource>,
) -> ApiResult<(StatusCode, Json<SourceEntry>)> {
    let path = absolute("path", &new.path)?;
    let version = new
        .enterprise_version
        .as_deref()
        .map(|v| {
            v.parse::<OdooVersion>()
                .map_err(|e| ApiError::bad_request("bad_version", e.to_string()))
        })
        .transpose()?;
    let entry = svc
        .edit_config(|cfg| {
            Ok(match version {
                Some(v) => sources::register_enterprise(cfg, &path, v)?,
                None => sources::register_community(cfg, &path)?,
            })
        })
        .await?;
    Ok((StatusCode::CREATED, Json(entry)))
}

/// A Community series to clone.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CloneSource {
    /// Odoo series, e.g. `19.0` or `saas~19.2`.
    #[schema(example = "19.0")]
    pub version: String,
}

/// Starts a shallow clone of Odoo Community `version` from the official
/// repository into `<workspace>/sources/odoo-<branch>`, registered once it
/// was verified. Follow it with `/jobs/{id}` or the WebSocket.
#[utoipa::path(operation_id = "clone_source", post, path = "/sources/clone", tag = "sources", request_body = CloneSource,
    responses(
        (status = ACCEPTED, description = "Clone started", body = Job),
        (status = BAD_REQUEST, description = "Invalid version", body = ErrorBody),
        (status = CONFLICT, description = "The folder exists or a clone of it runs", body = ErrorBody),
    ))]
pub(super) async fn clone(
    State(svc): State<Service>,
    Body(req): Body<CloneSource>,
) -> ApiResult<(StatusCode, Json<Job>)> {
    let version: OdooVersion =
        req.version
            .parse()
            .map_err(|e: odhive_core::version::VersionError| {
                ApiError::bad_request("bad_version", e.to_string())
            })?;
    let ws = svc.workspace().clone();
    let target = ws.sources_dir().join(format!("odoo-{}", version.branch()));
    if std::fs::symlink_metadata(&target).is_ok() {
        return Err(ApiError::conflict(
            "exists",
            format!(
                "{} already exists; register it with POST /sources",
                target.display()
            ),
        ));
    }
    let kind = JobKind::Clone {
        version: version.to_string(),
    };
    let job_svc = svc.clone();
    let job = svc.jobs().spawn(kind, move |report| {
        Box::pin(async move {
            // The clone registers into a scratch config; the real one is
            // edited only at the end, so other edits are not held up.
            let mut scratch = job_svc.config()?;
            let opts = job_svc.clone_options().clone();
            let cloned = sources::clone_community(&mut scratch, &ws, version, &opts, |e| {
                report.step(JobStep::Clone(e));
            })
            .await?;
            let source = job_svc
                .edit_config(|cfg| Ok(sources::register_community(cfg, &cloned.path)?))
                .await?;
            Ok(JobResult::Clone { source })
        })
    })?;
    Ok((StatusCode::ACCEPTED, Json(job)))
}

/// Unregisters a source tree; its files are kept. Refused while a project
/// uses it.
#[utoipa::path(operation_id = "remove_source", delete, path = "/sources/{id}", tag = "sources",
    params(("id" = String, Path, description = "Source id, e.g. `odoo-19.0`")),
    responses(
        (status = OK, description = "The removed entry", body = SourceEntry),
        (status = NOT_FOUND, description = "Not found", body = ErrorBody),
        (status = CONFLICT, description = "A project uses the source", body = ErrorBody),
    ))]
pub(super) async fn remove(
    State(svc): State<Service>,
    Params(id): Params<String>,
) -> ApiResult<Json<SourceEntry>> {
    let entry = svc
        .edit_config(|cfg| {
            let users: Vec<&str> = cfg
                .projects
                .iter()
                .filter(|p| {
                    ProjectConfig::load(&p.path).is_ok_and(|c| {
                        c.sources.community == id || c.sources.enterprise.as_deref() == Some(&id)
                    })
                })
                .map(|p| p.name.as_str())
                .collect();
            if !users.is_empty() {
                return Err(ApiError::conflict(
                    "in_use",
                    format!("source {id:?} is used by {}", users.join(", ")),
                ));
            }
            Ok(sources::remove_source(cfg, &id)?)
        })
        .await?;
    Ok(Json(entry))
}

/// The `odoo.conf` options of a Community source: from OdHive's cache, else
/// the list bundled with OdHive. Nothing is run; a project's option list
/// (`/projects/{name}/schema`) is read with its own Python.
#[utoipa::path(operation_id = "source_schema", get, path = "/sources/{id}/schema", tag = "sources",
    params(("id" = String, Path, description = "Community source id")),
    responses(
        (status = OK, description = "Success", body = SchemaInfo),
        (status = NOT_FOUND, description = "Not found", body = ErrorBody),
        (status = UNPROCESSABLE_ENTITY, description = "Not a Community source", body = ErrorBody),
    ))]
pub(super) async fn schema(
    State(svc): State<Service>,
    Params(id): Params<String>,
) -> ApiResult<Json<SchemaInfo>> {
    let cfg = svc.config()?;
    let source = cfg.sources.iter().find(|s| s.id == id).ok_or_else(|| {
        ApiError::not_found("unknown_source", format!("no source with id {id:?}"))
    })?;
    if source.edition != Edition::Community {
        return Err(ApiError::invalid(
            "bad_source",
            format!("{id} is an Enterprise source; options come from Community"),
        ));
    }
    let loaded = schema::load_schema(
        source,
        None,
        &svc.paths().data_dir.join(SCHEMA_CACHE_DIR),
        SCHEMA_TIMEOUT,
    )
    .await?;
    Ok(Json(loaded.into()))
}
