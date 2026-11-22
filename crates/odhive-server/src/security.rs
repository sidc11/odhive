//! Security middleware: Host allowlist (DNS-rebinding protection), Origin
//! check of WebSocket upgrades (cross-site WebSocket hijacking) and response
//! headers.

use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
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

/// Rejects WebSocket upgrades whose `Origin` is not OdHive's own web UI
/// with `403`. Browsers always send `Origin` on a WebSocket handshake and
/// never apply the same-origin policy to it, so this is what keeps other
/// sites out. Without `Origin` (not a browser) the request passes.
pub async fn require_same_origin_upgrade(
    State(guard): State<HostGuard>,
    req: Request,
    next: Next,
) -> Response {
    let upgrade = req
        .headers()
        .get(header::UPGRADE)
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"));
    if !upgrade {
        return next.run(req).await;
    }
    match req.headers().get(header::ORIGIN) {
        None => next.run(req).await,
        Some(o) if o.to_str().is_ok_and(|o| guard.is_allowed_origin(o)) => next.run(req).await,
        Some(_) => crate::api::error::ApiError::new(
            StatusCode::FORBIDDEN,
            "forbidden_origin",
            "WebSocket connections are only accepted from OdHive's own pages",
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
