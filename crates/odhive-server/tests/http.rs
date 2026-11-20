//! HTTP-level tests for the security layer and embedded UI.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use odhive_server::security::CONTENT_SECURITY_POLICY;
use odhive_server::{AppConfig, app};
use tower::ServiceExt;

const PORT: u16 = 9001;

async fn get(host: Option<&str>, path: &str) -> axum::response::Response {
    let mut req = Request::builder().uri(path);
    if let Some(h) = host {
        req = req.header(header::HOST, h);
    }
    app(AppConfig { port: PORT })
        .oneshot(req.body(Body::empty()).expect("request"))
        .await
        .expect("response")
}

fn assert_security_headers(resp: &axum::response::Response) {
    let h = resp.headers();
    assert_eq!(h["content-security-policy"], CONTENT_SECURITY_POLICY);
    assert_eq!(h["x-content-type-options"], "nosniff");
    assert_eq!(h["referrer-policy"], "no-referrer");
    assert_eq!(h["x-frame-options"], "DENY");
    let csp = h["content-security-policy"].to_str().expect("ascii");
    assert!(csp.contains("frame-ancestors 'none'"));
    assert!(csp.contains("default-src 'self'"));
    assert!(!csp.contains("unsafe-inline"));
    assert!(!csp.contains("unsafe-eval"));
}

#[tokio::test]
async fn foreign_host_is_forbidden() {
    let resp = get(Some("evil.com"), "/").await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_security_headers(&resp);
}

#[tokio::test]
async fn rebinding_host_with_matching_port_is_forbidden() {
    let resp = get(Some("evil.com:9001"), "/").await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn missing_host_is_forbidden() {
    let resp = get(None, "/").await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn localhost_is_allowed_and_carries_security_headers() {
    let resp = get(Some("localhost:9001"), "/").await;
    // 200 with a built UI; 503 "not built" page when web/dist is absent.
    assert!(
        resp.status() == StatusCode::OK || resp.status() == StatusCode::SERVICE_UNAVAILABLE,
        "unexpected status {}",
        resp.status()
    );
    assert_security_headers(&resp);
}

#[tokio::test]
async fn ipv4_and_ipv6_loopback_hosts_are_allowed() {
    for host in ["127.0.0.1:9001", "[::1]:9001"] {
        let resp = get(Some(host), "/").await;
        assert_ne!(resp.status(), StatusCode::FORBIDDEN, "{host}");
    }
}

#[tokio::test]
async fn unknown_asset_is_not_found() {
    let resp = get(Some("localhost:9001"), "/assets/does-not-exist.js").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_security_headers(&resp);
}

#[tokio::test]
async fn path_traversal_does_not_escape_embedded_files() {
    let resp = get(Some("localhost:9001"), "/../../Cargo.toml").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let resp = get(Some("localhost:9001"), "/%2e%2e/%2e%2e/Cargo.toml").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn non_get_methods_are_rejected() {
    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header(header::HOST, "localhost:9001")
        .body(Body::empty())
        .expect("request");
    let resp = app(AppConfig { port: PORT })
        .oneshot(req)
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn serves_index_html_when_ui_is_built() {
    let resp = get(Some("localhost:9001"), "/").await;
    if resp.status() == StatusCode::SERVICE_UNAVAILABLE {
        return; // web/dist not built in this environment
    }
    assert!(
        resp.headers()[header::CONTENT_TYPE]
            .to_str()
            .expect("ascii")
            .starts_with("text/html")
    );
    let body = resp.into_body().collect().await.expect("body").to_bytes();
    let html = String::from_utf8_lossy(&body);
    assert!(html.contains("<title>OdHive</title>"));
    assert!(!html.contains("<script>"), "no inline scripts allowed");
}

#[tokio::test]
async fn real_listener_falls_back_and_serves() {
    use odhive_core::ports::bind_loopback;
    let first = bind_loopback(45_000, 5_000).await.expect("bind");
    let busy = first.port;
    let second = bind_loopback(busy, 10).await.expect("fallback");
    assert!(second.port > busy);
    let port = second.port;
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(odhive_server::serve(second, async {
        let _ = rx.await;
    }));

    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let req = format!("GET / HTTP/1.1\r\nHost: evil.com:{port}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.expect("write");
    let mut buf = String::new();
    stream.read_to_string(&mut buf).await.expect("read");
    assert!(buf.starts_with("HTTP/1.1 403"), "got: {buf}");

    let _ = tx.send(());
    server.await.expect("join").expect("serve");
    drop(first);
}
