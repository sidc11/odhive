//! `/projects/{name}/open-file`: opens a `file:line` from a log in the
//! user's editor.

use axum::extract::State;
use axum::http::StatusCode;
use odhive_core::editor::{self, Editor, EditorError};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::error::{ApiError, ApiResult, ErrorBody};
use super::{Body, Params};
use crate::service::Service;

/// A source location to open.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct OpenFile {
    /// Absolute path, as logged. It must be a file inside the project
    /// folder, its addons folders or its Odoo sources (after resolving
    /// symlinks).
    #[schema(example = "/home/me/OdHive/projects/shop/addons/sale_x/models/order.py")]
    pub path: String,
    /// Line number, from 1.
    #[schema(minimum = 1, example = 42)]
    pub line: u32,
}

impl From<EditorError> for ApiError {
    fn from(e: EditorError) -> Self {
        let msg = e.to_string();
        match e {
            EditorError::NotAbsolute(_) | EditorError::BadLine(_) => {
                Self::invalid("bad_location", msg)
            }
            EditorError::NotAllowed(_) => Self::new(StatusCode::FORBIDDEN, "path_not_allowed", msg),
            EditorError::NoFile(_) => Self::not_found("file_not_found", msg),
            EditorError::NotFound | EditorError::EmptyTemplate => Self::conflict("no_editor", msg),
            EditorError::Spawn { .. } => Self::internal(msg),
        }
    }
}

/// Opens a file of the project at a line in the configured editor
/// (`editor` in config.toml, else `$VISUAL`/`$EDITOR` when they name a
/// graphical editor, else `code`). Returns once the editor has started.
#[utoipa::path(operation_id = "open_file", post, path = "/projects/{name}/open-file", tag = "projects",
    request_body = OpenFile,
    params(("name" = String, Path, description = "Project name")),
    responses(
        (status = NO_CONTENT, description = "The editor was started"),
        (status = FORBIDDEN, description = "The path is outside the project and its sources", body = ErrorBody),
        (status = NOT_FOUND, description = "Unknown project, or no such file", body = ErrorBody),
        (status = CONFLICT, description = "No editor is configured or found", body = ErrorBody),
        (status = UNPROCESSABLE_ENTITY, description = "Not an absolute path, or a bad line number", body = ErrorBody),
    ))]
pub(super) async fn open(
    State(svc): State<Service>,
    Params(name): Params<String>,
    Body(req): Body<OpenFile>,
) -> ApiResult<StatusCode> {
    let (cfg, project) = svc.project(&name)?;
    let roots = editor::project_roots(&cfg, &project);
    let file = editor::allowed_file(&roots, &req.path, req.line)?;
    Editor::from_env(cfg.editor.as_deref())?.open(&file, req.line)?;
    Ok(StatusCode::NO_CONTENT)
}
