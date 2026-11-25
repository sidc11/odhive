//! API errors: a status code plus a JSON body `{ "error": <code>, "message": <text> }`.

use axum::Json;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use odhive_core::config::ConfigError;
use odhive_core::db::archive::ArchiveError;
use odhive_core::db::{CreateError, DeleteError, DuplicateError, RestoreError};
use odhive_core::launch::LaunchError;
use odhive_core::manager::ManagerError;
use odhive_core::pidfile::PidFileError;
use odhive_core::project::ProjectError;
use odhive_core::schema::SchemaError;
use odhive_core::setup::SetupError;
use odhive_core::sources::SourceError;
use serde::Serialize;
use utoipa::ToSchema;

/// Body of every error response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize, ToSchema)]
pub struct ErrorBody {
    /// Stable machine-readable code, e.g. `unknown_project`.
    #[schema(example = "unknown_project")]
    pub error: String,
    /// Human-readable explanation.
    pub message: String,
}

/// An error returned by an API handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    /// HTTP status.
    pub status: StatusCode,
    /// Stable code, see [`ErrorBody::error`].
    pub code: &'static str,
    /// Explanation.
    pub message: String,
}

impl ApiError {
    /// An error with `status`, `code` and `message`.
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    /// `400 Bad Request`.
    pub fn bad_request(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, message)
    }

    /// `404 Not Found`.
    pub fn not_found(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, code, message)
    }

    /// `409 Conflict`.
    pub fn conflict(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, code, message)
    }

    /// `422 Unprocessable Entity`.
    pub fn invalid(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, code, message)
    }

    /// `500 Internal Server Error`.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", message)
    }

    /// `413 Payload Too Large` for a body over `limit` bytes.
    pub fn body_too_large(limit: u64) -> Self {
        Self::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "body_too_large",
            format!("request bodies are limited to {limit} bytes"),
        )
    }

    /// The JSON body, without the status.
    #[must_use]
    pub fn body(&self) -> ErrorBody {
        ErrorBody {
            error: self.code.to_owned(),
            message: self.message.clone(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if self.status.is_server_error() {
            tracing::error!(code = self.code, "{}", self.message);
        }
        (self.status, Json(self.body())).into_response()
    }
}

impl From<JsonRejection> for ApiError {
    fn from(e: JsonRejection) -> Self {
        // A chunked body cut at the `DefaultBodyLimit`.
        if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
            return Self::new(e.status(), "body_too_large", e.body_text());
        }
        Self::new(e.status(), "bad_body", e.body_text())
    }
}

impl From<QueryRejection> for ApiError {
    fn from(e: QueryRejection) -> Self {
        Self::bad_request("bad_query", e.body_text())
    }
}

impl From<PathRejection> for ApiError {
    fn from(e: PathRejection) -> Self {
        Self::bad_request("bad_path", e.body_text())
    }
}

impl From<ConfigError> for ApiError {
    fn from(e: ConfigError) -> Self {
        Self::internal(e.to_string())
    }
}

impl From<PidFileError> for ApiError {
    fn from(e: PidFileError) -> Self {
        Self::internal(e.to_string())
    }
}

impl From<SchemaError> for ApiError {
    fn from(e: SchemaError) -> Self {
        Self::invalid("bad_source", e.to_string())
    }
}

impl From<ProjectError> for ApiError {
    fn from(e: ProjectError) -> Self {
        let msg = e.to_string();
        match e {
            ProjectError::Io { .. } | ProjectError::Serialize(_) => Self::internal(msg),
            _ => Self::invalid("invalid_project", msg),
        }
    }
}

impl From<SourceError> for ApiError {
    fn from(e: SourceError) -> Self {
        let msg = e.to_string();
        match e {
            SourceError::NotADirectory(_)
            | SourceError::NotCommunity(_)
            | SourceError::NotEnterprise(_)
            | SourceError::Version(_)
            | SourceError::VersionMismatch { .. } => Self::invalid("bad_source", msg),
            SourceError::UnknownSource(_) => Self::not_found("unknown_source", msg),
            SourceError::AlreadyExists(_) => Self::conflict("exists", msg),
            SourceError::GitNotFound(_) => Self::invalid("git_missing", msg),
            SourceError::GitFailed { .. } => Self::new(StatusCode::BAD_GATEWAY, "git_failed", msg),
            _ => Self::internal(msg),
        }
    }
}

impl From<SetupError> for ApiError {
    fn from(e: SetupError) -> Self {
        let msg = e.to_string();
        match e {
            SetupError::Name(_) => Self::bad_request("invalid_name", msg),
            SetupError::AlreadyRegistered(_) | SetupError::Exists(_) => {
                Self::conflict("exists", msg)
            }
            SetupError::UnknownSource { .. } => Self::not_found("unknown_source", msg),
            SetupError::BadAddonsPath(_) => Self::invalid("bad_addons_path", msg),
            SetupError::Source(e) => e.into(),
            SetupError::Project(e) => e.into(),
            SetupError::Python(_) | SetupError::PythonTooOld { .. } | SetupError::Pick(_) => {
                Self::invalid("python", msg)
            }
            SetupError::BrokenVenv(_) => Self::conflict("broken_venv", msg),
            SetupError::Venv(_) => Self::new(StatusCode::INTERNAL_SERVER_ERROR, "venv_failed", msg),
            SetupError::Requirements(_) => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "requirements_failed",
                msg,
            ),
            SetupError::Io { .. } => Self::internal(msg),
        }
    }
}

impl From<ManagerError> for ApiError {
    fn from(e: ManagerError) -> Self {
        let msg = e.to_string();
        match e {
            ManagerError::NotRunning(_) => Self::conflict("not_running", msg),
            ManagerError::AlreadyRunning(_) => Self::conflict("already_running", msg),
            ManagerError::Busy(_) => Self::conflict("busy", msg),
            ManagerError::UnknownInstance { .. } => Self::not_found("unknown_instance", msg),
            ManagerError::DatabaseInUse { .. } => Self::conflict("database_in_use", msg),
            ManagerError::NoPlan(_) => Self::conflict("no_plan", msg),
            ManagerError::Project(e) => e.into(),
            ManagerError::Ports(_) => Self::conflict("ports_unavailable", msg),
            ManagerError::NoPendingFix(_) => Self::conflict("no_pending_fix", msg),
            ManagerError::FixRefused { .. } => Self::conflict("fix_refused", msg),
            ManagerError::Fixing(_) => Self::conflict("fixing", msg),
            ManagerError::Supervisor(_) | ManagerError::PidFile(_) => Self::internal(msg),
        }
    }
}

impl From<LaunchError> for ApiError {
    fn from(e: LaunchError) -> Self {
        let msg = e.to_string();
        match e {
            LaunchError::BadTarget(_) | LaunchError::Name(_) => {
                Self::bad_request("invalid_name", msg)
            }
            LaunchError::UnknownProject(_) => Self::not_found("unknown_project", msg),
            LaunchError::UnknownInstance { .. } => Self::not_found("unknown_instance", msg),
            LaunchError::NameMismatch { .. } => Self::conflict("project_mismatch", msg),
            LaunchError::NoVenv(..) => Self::conflict("no_venv", msg),
            LaunchError::NoOdooBin(_) => Self::conflict("source_missing", msg),
            LaunchError::Project(e) => e.into(),
            LaunchError::Manager(e) => e.into(),
            LaunchError::Schema(e) => e.into(),
            LaunchError::Conf(_) | LaunchError::PidFile(_) | LaunchError::Io { .. } => {
                Self::internal(msg)
            }
        }
    }
}

impl From<CreateError> for ApiError {
    fn from(e: CreateError) -> Self {
        let msg = e.to_string();
        match e {
            CreateError::Name(_) => Self::bad_request("invalid_name", msg),
            CreateError::Invalid { .. } => Self::bad_request("invalid_request", msg),
            CreateError::Exists(_) => Self::conflict("database_exists", msg),
            CreateError::Server(_) => {
                Self::new(StatusCode::SERVICE_UNAVAILABLE, "postgres_unavailable", msg)
            }
            CreateError::Spawn { .. } => Self::internal(msg),
            CreateError::Failed { .. } => {
                Self::new(StatusCode::INTERNAL_SERVER_ERROR, "create_failed", msg)
            }
        }
    }
}

impl From<DeleteError> for ApiError {
    fn from(e: DeleteError) -> Self {
        let msg = e.to_string();
        match e {
            DeleteError::Name(_) => Self::bad_request("invalid_name", msg),
            DeleteError::NotFound(_) => Self::not_found("database_not_found", msg),
            DeleteError::NotConfirmed(_) => Self::bad_request("not_confirmed", msg),
            DeleteError::InUse { .. } => Self::conflict("database_in_use", msg),
            DeleteError::Server(_) => {
                Self::new(StatusCode::SERVICE_UNAVAILABLE, "postgres_unavailable", msg)
            }
            DeleteError::Failed { .. } | DeleteError::Filestore { .. } => {
                Self::new(StatusCode::INTERNAL_SERVER_ERROR, "delete_failed", msg)
            }
        }
    }
}

impl From<DuplicateError> for ApiError {
    fn from(e: DuplicateError) -> Self {
        let msg = e.to_string();
        match e {
            DuplicateError::Name(_) => Self::bad_request("invalid_name", msg),
            DuplicateError::Request(e) => e.into(),
            DuplicateError::SameName(_) => Self::bad_request("invalid_request", msg),
            DuplicateError::NotFound(_) => Self::not_found("database_not_found", msg),
            DuplicateError::Exists(_) => Self::conflict("database_exists", msg),
            DuplicateError::FilestoreExists(_) => Self::conflict("filestore_exists", msg),
            DuplicateError::InUse { .. } => Self::conflict("database_in_use", msg),
            DuplicateError::Server(_) => {
                Self::new(StatusCode::SERVICE_UNAVAILABLE, "postgres_unavailable", msg)
            }
            DuplicateError::Spawn { .. } | DuplicateError::Failed { .. } => {
                Self::new(StatusCode::INTERNAL_SERVER_ERROR, "duplicate_failed", msg)
            }
        }
    }
}

impl From<RestoreError> for ApiError {
    fn from(e: RestoreError) -> Self {
        let msg = e.to_string();
        match e {
            RestoreError::Request(e) => e.into(),
            RestoreError::Exists(_) => Self::conflict("database_exists", msg),
            RestoreError::FilestoreExists(_) => Self::conflict("filestore_exists", msg),
            RestoreError::Server(_) => {
                Self::new(StatusCode::SERVICE_UNAVAILABLE, "postgres_unavailable", msg)
            }
            RestoreError::MissingTool { .. } => Self::conflict("pg_tools_missing", msg),
            RestoreError::PsqlTooOld { .. } => Self::conflict("psql_too_old", msg),
            RestoreError::VersionMismatch { .. } => Self::conflict("version_mismatch", msg),
            RestoreError::Archive(e) => e.into(),
            RestoreError::NotOdoo => Self::bad_request("invalid_dump", msg),
            RestoreError::NotNeutralized
            | RestoreError::Spawn { .. }
            | RestoreError::Failed { .. }
            | RestoreError::Io { .. } => {
                Self::new(StatusCode::INTERNAL_SERVER_ERROR, "restore_failed", msg)
            }
        }
    }
}

impl From<ArchiveError> for ApiError {
    fn from(e: ArchiveError) -> Self {
        let msg = e.to_string();
        match e {
            ArchiveError::Empty
            | ArchiveError::Compressed(_)
            | ArchiveError::Unknown
            | ArchiveError::NoDump
            | ArchiveError::Zip(_) => Self::bad_request("invalid_dump", msg),
            ArchiveError::Unsafe { .. }
            | ArchiveError::TooManyEntries(_)
            | ArchiveError::TooLarge(_)
            | ArchiveError::Bomb { .. } => Self::bad_request("unsafe_archive", msg),
            ArchiveError::Io { .. } | ArchiveError::Cancelled => Self::internal(msg),
        }
    }
}

/// What API handlers return.
pub(crate) type ApiResult<T> = Result<T, ApiError>;
