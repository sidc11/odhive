//! Security middleware: Host allowlist (DNS-rebinding protection), Origin
//! check of mutations and WebSocket upgrades (CSRF, cross-site WebSocket
//! hijacking), request body and time limits, and response headers.
//!
//! OdHive never sends CORS headers: no other origin may read its responses.

use axum::extract::{Request, State};
use std::time::Duration;

use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// Content Security Policy applied to every response. No inline scripts or styles.
pub const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; script-src 'self'; \
style-src 'self'; img-src 'self' data:; font-src 'self'; connect-src 'self'; \
object-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

/// Accepts only loopback host names on the port OdHive is serving.
#[derive(Debug, Clone, Copy)]
pub struct HostGuard {
    port: u16,
}

impl HostGuard {
    /// Creates a guard for the given serving port.
    #[must_use]
    pub fn new(port: u16) -> Self {
        Self { port }
    }

    /// Returns true if `host` (a `Host` header value) is an allowed loopback authority.
    ///
    /// Allowed: `localhost`, `127.0.0.1` and `[::1]`, each with the serving port.
    #[must_use]
    pub fn is_allowed(&self, host: &str) -> bool {
        let Some((name, port)) = split_authority(host) else {
            return false;
        };
        let Ok(port) = port.parse::<u16>() else {
            return false;
        };
        if port != self.port {
            return false;
        }
        name.eq_ignore_ascii_case("localhost") || name == "127.0.0.1" || name == "[::1]"
    }

    /// Returns true if `origin` (an `Origin` header value) is OdHive's own
    /// web UI: `http://` plus an allowed authority, nothing else.
    #[must_use]
    pub fn is_allowed_origin(&self, origin: &str) -> bool {
        origin
            .strip_prefix("http://")
            .is_some_and(|authority| !authority.contains('/') && self.is_allowed(authority))
    }
}

/// Splits `name:port`, keeping IPv6 brackets on the name. Returns `None` without a port.
fn split_authority(host: &str) -> Option<(&str, &str)> {
    if host.starts_with('[') {
        let end = host.find(']')?;
        let (name, rest) = host.split_at(end + 1);
        return rest.strip_prefix(':').map(|port| (name, port));
    }
    let (name, port) = host.rsplit_once(':')?;
    if name.contains(':') {
        return None;
    }
    Some((name, port))
}

/// Rejects requests whose `Host` is not an allowed loopback authority with `403`.
pub async fn require_allowed_host(
    State(guard): State<HostGuard>,
    req: Request,
    next: Next,
) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .or_else(|| req.uri().authority().map(|a| a.as_str()));
    match host {
        Some(h) if guard.is_allowed(h) => next.run(req).await,
        _ => {
            let mut resp = (StatusCode::FORBIDDEN, "forbidden host\n").into_response();
            apply_security_headers(resp.headers_mut());
            resp
        }
    }
}

/// Rejects cross-site requests with `403 forbidden_origin` (CSRF and
/// cross-site WebSocket hijacking).
///
/// Checked: every request except `GET`/`HEAD`, and every WebSocket upgrade
/// (a `GET` browsers send cross-site without asking). Such a request must
/// carry the `Origin` of OdHive's own web UI. Browsers always send `Origin`
/// on these requests, so a request without one is not from a page; it is
/// let through only with an `Authorization` header (the CLI's bearer
/// token), which a page cannot add cross-site without a CORS preflight that
/// OdHive never answers.
pub async fn require_same_origin(
    State(guard): State<HostGuard>,
    req: Request,
    next: Next,
) -> Response {
    if is_same_origin(&guard, &req) {
        next.run(req).await
    } else {
        crate::api::error::ApiError::new(
            StatusCode::FORBIDDEN,
            "forbidden_origin",
            "this request is only accepted from OdHive's own pages",
        )
        .into_response()
    }
}

fn is_same_origin(guard: &HostGuard, req: &Request) -> bool {
    let headers = req.headers();
    let upgrade = headers
        .get(header::UPGRADE)
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"));
    let safe = matches!(*req.method(), Method::GET | Method::HEAD);
    if safe && !upgrade {
        return true;
    }
    let mut origins = headers.get_all(header::ORIGIN).iter();
    match (origins.next(), origins.next()) {
        (None, _) => headers.contains_key(header::AUTHORIZATION),
        (Some(o), None) => o.to_str().is_ok_and(|o| guard.is_allowed_origin(o)),
        // More than one Origin is never sent by a browser.
        (Some(_), Some(_)) => false,
    }
}

/// Request limits applied by [`limit_requests`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestLimits {
    /// Largest accepted request body in bytes; larger → `413 body_too_large`.
    pub max_body: usize,
    /// Longest time a request may take to answer → else `503 timeout`.
    /// A WebSocket only counts until its upgrade response.
    pub timeout: Duration,
    /// Limits of the upload routes ([`is_upload`]), which take the place of
    /// `max_body` and `timeout` there.
    pub upload: UploadLimits,
}

/// Limits of a file upload (a database dump to restore).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UploadLimits {
    /// Largest file in bytes; larger → `413 body_too_large`.
    pub max_bytes: u64,
    /// Longest wait for the next chunk → else `408 upload_timeout`.
    pub idle: Duration,
}

impl UploadLimits {
    /// Default upload limit: 32 GiB.
    pub const MAX_BYTES: u64 = 32 * 1024 * 1024 * 1024;
    /// Default idle limit: 60 s without a byte.
    pub const IDLE: Duration = Duration::from_secs(60);
}

impl Default for UploadLimits {
    fn default() -> Self {
        Self {
            max_bytes: Self::MAX_BYTES,
            idle: Self::IDLE,
        }
    }
}

/// Whether `req` goes to an upload route: `POST` to
/// `/api/v1/projects/{name}/databases/restore`.
#[must_use]
pub fn is_upload(req: &Request) -> bool {
    if req.method() != Method::POST {
        return false;
    }
    let path = req.uri().path();
    path.strip_prefix("/api/v1/projects/")
        .and_then(|rest| rest.strip_suffix("/databases/restore"))
        .is_some_and(|name| !name.is_empty() && !name.contains('/'))
}

impl RequestLimits {
    /// Default body limit: 1 MiB (OdHive's API bodies are small JSON).
    pub const MAX_BODY: usize = 1024 * 1024;
    /// Default request timeout: above the longest handler (an instance
    /// restart stops gracefully for up to 15 s before it starts again).
    pub const TIMEOUT: Duration = Duration::from_secs(60);
}

impl Default for RequestLimits {
    fn default() -> Self {
        Self {
            max_body: Self::MAX_BODY,
            timeout: Self::TIMEOUT,
            upload: UploadLimits::default(),
        }
    }
}

/// Rejects a request whose declared `Content-Length` is over the body limit
/// with `413 body_too_large`, and answers `503 timeout` when the handler takes
/// longer than the limit. A body without a length (chunked) is cut at the
/// same limit when a handler reads it (see the `DefaultBodyLimit` on the API).
///
/// An upload ([`is_upload`]) is held to [`UploadLimits`] instead: its size
/// limit here, and its handler (which gets the limits as a request
/// extension) cuts the stream at the same size and after `idle` without
/// data, rather than after a fixed time.
pub async fn limit_requests(
    State(limits): State<RequestLimits>,
    mut req: Request,
    next: Next,
) -> Response {
    let upload = is_upload(&req);
    let declared = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .map(|v| v.to_str().ok().and_then(|v| v.parse::<u64>().ok()));
    let too_large = match declared {
        None => false,
        Some(None) => {
            return crate::api::error::ApiError::bad_request(
                "bad_request",
                "invalid Content-Length",
            )
            .into_response();
        }
        Some(Some(n)) if upload => n > limits.upload.max_bytes,
        Some(Some(n)) => usize::try_from(n).map_or(true, |n| n > limits.max_body),
    };
    if too_large {
        let max = if upload {
            limits.upload.max_bytes
        } else {
            limits.max_body as u64
        };
        return crate::api::error::ApiError::body_too_large(max).into_response();
    }
    if upload {
        req.extensions_mut().insert(limits.upload);
        return next.run(req).await;
    }
    match tokio::time::timeout(limits.timeout, next.run(req)).await {
        Ok(resp) => resp,
        Err(_) => crate::api::error::ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "timeout",
            format!(
                "the request took longer than {} s",
                limits.timeout.as_secs()
            ),
        )
        .into_response(),
    }
}

/// Adds the security headers to every response.
pub async fn security_headers(req: Request, next: Next) -> Response {
    let mut resp = next.run(req).await;
    apply_security_headers(resp.headers_mut());
    resp
}

fn apply_security_headers(headers: &mut axum::http::HeaderMap) {
    let pairs: [(&str, &str); 8] = [
        ("content-security-policy", CONTENT_SECURITY_POLICY),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        ("x-frame-options", "DENY"),
        ("cross-origin-opener-policy", "same-origin"),
        ("cross-origin-resource-policy", "same-origin"),
        (
            "permissions-policy",
            "camera=(), microphone=(), geolocation=(), payment=(), usb=()",
        ),
        ("x-permitted-cross-domain-policies", "none"),
    ];
    for (name, value) in pairs {
        headers.insert(name, HeaderValue::from_static(value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn slow_handlers_time_out_with_503() {
        use axum::body::Body;
        use tower::ServiceExt as _;
        let limits = RequestLimits {
            timeout: Duration::from_millis(50),
            ..RequestLimits::default()
        };
        let app = axum::Router::new()
            .route(
                "/slow",
                axum::routing::get(|| async {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    "late"
                }),
            )
            .route("/fast", axum::routing::get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(limits, limit_requests));
        let call = |path: &'static str| {
            let app = app.clone();
            async move {
                app.oneshot(Request::get(path).body(Body::empty()).expect("request"))
                    .await
                    .expect("response")
            }
        };
        let resp = call("/slow").await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = axum::body::to_bytes(resp.into_body(), 1024)
            .await
            .expect("body");
        assert!(String::from_utf8_lossy(&body).contains("\"timeout\""));
        assert_eq!(call("/fast").await.status(), StatusCode::OK);
    }

    #[test]
    fn allows_loopback_names_on_serving_port() {
        let g = HostGuard::new(7069);
        assert!(g.is_allowed("localhost:7069"));
        assert!(g.is_allowed("LocalHost:7069"));
        assert!(g.is_allowed("127.0.0.1:7069"));
        assert!(g.is_allowed("[::1]:7069"));
    }

    #[test]
    fn allows_only_own_origins() {
        let g = HostGuard::new(7069);
        for ok in [
            "http://localhost:7069",
            "http://127.0.0.1:7069",
            "http://[::1]:7069",
        ] {
            assert!(g.is_allowed_origin(ok), "{ok:?} should be allowed");
        }
        for bad in [
            "https://localhost:7069",
            "http://localhost:7070",
            "http://localhost",
            "http://evil.com:7069",
            "http://localhost:7069/",
            "http://localhost:7069.evil.com",
            "http://user@localhost:7069",
            "null",
            "localhost:7069",
            "",
        ] {
            assert!(!g.is_allowed_origin(bad), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn rejects_foreign_or_malformed_hosts() {
        let g = HostGuard::new(7069);
        for bad in [
            "evil.com",
            "evil.com:7069",
            "localhost",
            "localhost:7070",
            "localhost.:7069",
            "localhost.evil.com:7069",
            "127.0.0.2:7069",
            "0.0.0.0:7069",
            "[::2]:7069",
            "[::1]",
            "::1:7069",
            "localhost:abc",
            "localhost:99999",
            "",
        ] {
            assert!(!g.is_allowed(bad), "{bad:?} should be rejected");
        }
    }
}
