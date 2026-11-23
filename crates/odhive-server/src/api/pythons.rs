//! `/pythons`: Python interpreters found on this machine.

use axum::Json;
use axum::extract::State;
use odhive_core::config::Edition;
use odhive_core::python::{self, Compat, Interpreter, PySupport};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use super::QueryArgs;
use super::error::{ApiError, ApiResult, ErrorBody};
use crate::service::Service;

/// Which Odoo source to judge the interpreters against.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub struct PythonQuery {
    /// Registered Community source id; omitted = no `compat` judgement.
    #[serde(default)]
    pub source: Option<String>,
}

/// A working Python interpreter.
#[derive(Debug, Clone, Serialize, serde::Deserialize, ToSchema)]
pub struct PythonInfo {
    /// Interpreter executable (symlinks resolved).
    pub path: String,
    /// Version, e.g. `3.12.4`.
    pub version: String,
    /// `sys.implementation.name`, e.g. `cpython`.
    pub implementation: String,
    /// Built without the GIL.
    pub free_threaded: bool,
    /// A standard CPython build, the kind Odoo's dependencies are built for.
    pub standard: bool,
    /// How its version fits the source's range; `None` without `source`.
    pub compat: Option<Compat>,
}

impl PythonInfo {
    fn new(i: &Interpreter, support: Option<PySupport>) -> Self {
        Self {
            path: i.path.display().to_string(),
            version: i.version.to_string(),
            implementation: i.implementation.clone(),
            free_threaded: i.free_threaded,
            standard: i.is_standard_cpython(),
            compat: support.map(|s| s.check(i.version)),
        }
    }
}

/// The interpreters found, and the one a project setup would pick.
#[derive(Debug, Clone, Serialize, serde::Deserialize, ToSchema)]
pub struct PythonList {
    /// Python range the source supports; `None` without `source`.
    pub support: Option<PySupport>,
    /// Path of the interpreter a setup picks by default (newest supported
    /// standard CPython); `None` when none fits or without `source`.
    pub recommended: Option<String>,
    /// Why nothing fits, when `source` was given and nothing does.
    pub problem: Option<String>,
    /// Working interpreters, in discovery order (`PATH`, then uv).
    pub interpreters: Vec<PythonInfo>,
    /// Why uv's interpreters could not be listed.
    pub uv_error: Option<String>,
}

/// Finds and probes the Python 3 interpreters on `PATH` and those uv
/// installed. With `source`, each is judged against that Odoo's range.
#[utoipa::path(operation_id = "list_pythons", get, path = "/pythons", tag = "service",
    params(PythonQuery),
    responses(
        (status = OK, description = "Success", body = PythonList),
        (status = BAD_REQUEST, description = "Invalid query", body = ErrorBody),
        (status = NOT_FOUND, description = "Unknown source", body = ErrorBody),
        (status = UNPROCESSABLE_ENTITY, description = "Not a Community source", body = ErrorBody),
    ))]
pub(super) async fn list(
    State(svc): State<Service>,
    QueryArgs(q): QueryArgs<PythonQuery>,
) -> ApiResult<Json<PythonList>> {
    let support = match q.source.as_deref() {
        None => None,
        Some(id) => {
            let cfg = svc.config()?;
            let source = cfg.sources.iter().find(|s| s.id == id).ok_or_else(|| {
                ApiError::not_found("unknown_source", format!("no source with id {id:?}"))
            })?;
            if source.edition != Edition::Community {
                return Err(ApiError::invalid(
                    "bad_source",
                    format!("{id} is an Enterprise source; Python comes from Community"),
                ));
            }
            Some(PySupport::for_source(&source.path, source.odoo_version))
        }
    };
    let found = python::discover(svc.discover_options()).await;
    let (recommended, problem) = match support {
        None => (None, None),
        Some(s) => match python::pick(&found.interpreters, s) {
            Ok(i) => (Some(i.path.display().to_string()), None),
            Err(e) => (None, Some(e.to_string())),
        },
    };
    Ok(Json(PythonList {
        support,
        recommended,
        problem,
        interpreters: found
            .interpreters
            .iter()
            .map(|i| PythonInfo::new(i, support))
            .collect(),
        uv_error: found.uv_error,
    }))
}
