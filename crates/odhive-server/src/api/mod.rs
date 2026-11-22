//! The REST API under `/api/v1`, described by an OpenAPI document
//! ([`openapi`]) from which the web UI's TypeScript types are generated.
//!
//! Every mutation (any method but `GET`/`HEAD`) must carry
//! `X-OdHive: 1`: browsers cannot add that header to a cross-site request
//! without a CORS preflight, which OdHive never answers.

pub mod error;
mod instances;
mod projects;
mod sources;
pub mod ws;

use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::{Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde::Serialize;
use utoipa::{OpenApi, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::service::Service;
use error::ApiError;

/// Where the API is mounted.
pub const PREFIX: &str = "/api/v1";

/// Header every mutating request must carry, with value `1`.
pub const MUTATION_HEADER: &str = "x-odhive";

#[derive(OpenApi)]
#[openapi(
    info(
        title = "OdHive API",
        description = "Local API of OdHive, used by its web UI and CLI. Every request other than GET/HEAD must carry the header `X-OdHive: 1`.",
        license(name = "Apache-2.0", identifier = "Apache-2.0")
    ),
    tags(
        (name = "sources", description = "Registered Odoo source trees"),
        (name = "projects", description = "Projects and their instances"),
        (name = "instances", description = "Running Odoo instances"),
        (name = "service", description = "The OdHive service itself"),
    ),
    components(schemas(ws::ClientMessage, ws::ServerMessage))
)]
struct ApiDoc;

fn api_router() -> OpenApiRouter<Service> {
    OpenApiRouter::new()
        .routes(routes!(health))
        .routes(routes!(sources::list, sources::add))
        .routes(routes!(sources::remove))
        .routes(routes!(sources::schema))
        .routes(routes!(projects::list, projects::create))
        .routes(routes!(projects::get, projects::remove))
        .routes(routes!(projects::schema))
        .routes(routes!(instances::list))
        .routes(routes!(instances::start))
        .routes(routes!(instances::stop))
        .routes(routes!(instances::restart))
        .routes(routes!(instances::confirm_fix))
        .routes(routes!(instances::decline_fix))
        .routes(routes!(instances::logs))
        .routes(routes!(ws::upgrade))
}

/// The OpenAPI document of the API.
#[must_use]
pub fn openapi() -> utoipa::openapi::OpenApi {
    let (_, doc) = OpenApiRouter::<Service>::with_openapi(ApiDoc::openapi())
        .nest(PREFIX, api_router())
        .split_for_parts();
    doc
}

/// The API routes, mounted at [`PREFIX`]; unknown paths below `/api` get a
/// JSON 404 instead of the web UI.
pub(crate) fn router(service: Service) -> Router {
    let (api, _) = api_router().split_for_parts();
    let api = api.fallback(not_found).with_state(service);
    Router::new()
        .nest(PREFIX, api)
        .route("/api", axum::routing::any(not_found))
        .route("/api/{*rest}", axum::routing::any(not_found))
        .layer(middleware::from_fn(require_mutation_header))
}

async fn not_found() -> ApiError {
    ApiError::not_found("not_found", "no such API route")
}

/// Rejects mutations without `X-OdHive: 1` with `403`.
async fn require_mutation_header(req: Request, next: Next) -> Response {
    let safe = matches!(*req.method(), Method::GET | Method::HEAD);
    let marked = req
        .headers()
        .get(MUTATION_HEADER)
        .is_some_and(|v| v.as_bytes() == b"1");
    if safe || marked {
        next.run(req).await
    } else {
        ApiError::new(
            StatusCode::FORBIDDEN,
            "missing_header",
            "mutating requests need the header X-OdHive: 1",
        )
        .into_response()
    }
}

/// A JSON body whose rejection is an [`ApiError`].
#[derive(Debug, FromRequest)]
#[from_request(via(axum::Json), rejection(ApiError))]
pub(crate) struct Body<T>(pub T);

/// Path parameters whose rejection is an [`ApiError`].
#[derive(Debug, FromRequestParts)]
#[from_request(via(axum::extract::Path), rejection(ApiError))]
pub(crate) struct Params<T>(pub T);

/// Query parameters whose rejection is an [`ApiError`].
#[derive(Debug, FromRequestParts)]
#[from_request(via(axum::extract::Query), rejection(ApiError))]
pub(crate) struct QueryArgs<T>(pub T);

/// Service status.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Health {
    /// OdHive version.
    #[schema(example = "0.1.0")]
    pub version: String,
}

/// Whether the service is up, and its version.
#[utoipa::path(operation_id = "health", get, path = "/health", tag = "service",
    responses((status = OK, description = "Success", body = Health)))]
async fn health() -> Json<Health> {
    Json(Health {
        version: env!("CARGO_PKG_VERSION").to_owned(),
    })
}
