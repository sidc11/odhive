//! Embedded frontend (`web/dist`) served with a single-page-app fallback.

use axum::body::Body;
use axum::http::{Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "../../web/dist"]
#[allow_missing = true]
struct WebDist;

const NOT_BUILT: &str = "OdHive web UI is not built. Run `pnpm --dir web install && pnpm --dir web build`, then rebuild OdHive.\n";

pub(crate) async fn serve(method: Method, uri: Uri) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let path = uri.path().trim_start_matches('/');

    if !path.is_empty()
        && let Some(resp) = file_response(path)
    {
        return resp;
    }
    // The root and client-side routes (no file extension) get the app shell.
    let last = path.rsplit('/').next().unwrap_or(path);
    if !last.contains('.') {
        if let Some(resp) = file_response("index.html") {
            return resp;
        }
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            NOT_BUILT,
        )
            .into_response();
    }
    StatusCode::NOT_FOUND.into_response()
}

fn file_response(path: &str) -> Option<Response> {
    let file = WebDist::get(path)?;
    let mime = file.metadata.mimetype().to_owned();
    // Vite fingerprints everything under assets/, so it can be cached forever.
    let cache = if path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime)
        .header(header::CACHE_CONTROL, cache)
        .body(Body::from(file.data.into_owned()))
        .ok()
}
