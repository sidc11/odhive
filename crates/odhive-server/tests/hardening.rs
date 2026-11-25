//! Request hardening: Origin check on mutations, body size limit, no CORS,
//! connection limit, header read deadline and the WebSocket cap.

use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use axum::response::Response;
use odhive_core::manager::ManagerOptions;
use odhive_core::ports::LoopbackListeners;
use odhive_server::{ConnectionLimits, Limits, ServiceOptions, WsLimits};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest};

mod common;
use common::{Api, Env, HOST, ORIGIN, split};

/// A signed-in mutation of `path` with `origin` (none if `None`) and
/// `extra` headers.
fn mutation(
    method: Method,
    path: &str,
    origin: Option<&str>,
    extra: &[(header::HeaderName, &str)],
) -> Request<Body> {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, HOST)
        .header("x-odhive", "1");
    if let Some(o) = origin {
        req = req.header(header::ORIGIN, o);
    }
    for (name, value) in extra {
        req = req.header(name, *value);
    }
    req.body(Body::empty()).expect("request")
}

fn assert_no_cors(headers: &HeaderMap, what: &str) {
    for name in headers.keys() {
        assert!(
            !name.as_str().starts_with("access-control-"),
            "{what}: CORS header {name}"
        );
    }
}

#[tokio::test]
async fn mutations_from_foreign_or_missing_origins_are_refused() {
    let env = Env::new();
    let api = env.app();
    let cookie = [(header::COOKIE, api.cookie.as_str())];
    let foreign = [
        Some("http://evil.com"),
        Some("http://evil.com:7069"),
        Some("https://localhost:7069"),
        Some("http://localhost:7070"),
        Some("http://localhost:7069/"),
        Some("null"),
        Some(""),
        None,
    ];
    for (method, path) in [
        (Method::POST, "/api/v1/auth/codes"),
        (Method::DELETE, "/api/v1/sources/nope"),
        (Method::POST, "/api/v1/projects/shop/options/reveal"),
        (Method::PUT, "/api/v1/sources"),
        (Method::PATCH, "/api/v1/nope"),
        (Method::OPTIONS, "/api/v1/sources"),
        (Method::POST, "/"),
        (Method::POST, "/login"),
    ] {
        for origin in foreign {
            let resp = api
                .send(mutation(method.clone(), path, origin, &cookie))
                .await;
            assert_no_cors(resp.headers(), path);
            let (status, body) = split(resp).await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "{method} {path} {origin:?}: {body}"
            );
            assert_eq!(body["error"], "forbidden_origin");
        }
    }

    // Two Origin headers are never sent by a browser.
    let mut req = mutation(Method::POST, "/api/v1/auth/codes", Some(ORIGIN), &cookie);
    req.headers_mut()
        .append(header::ORIGIN, "http://evil.com".parse().expect("origin"));
    let (status, _) = split(api.send(req).await).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // The own origins pass; so does the CLI (bearer, no Origin).
    for origin in [ORIGIN, "http://127.0.0.1:7069", "http://[::1]:7069"] {
        let req = mutation(Method::POST, "/api/v1/auth/codes", Some(origin), &cookie);
        let (status, body) = split(api.send(req).await).await;
        assert_eq!(status, StatusCode::CREATED, "{origin}: {body}");
    }
    let bearer = [(header::AUTHORIZATION, api.bearer.as_str())];
    let req = mutation(Method::POST, "/api/v1/auth/codes", None, &bearer);
    let (status, body) = split(api.send(req).await).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // Reads need no Origin.
    let req = Request::get("/api/v1/health")
        .header(header::HOST, HOST)
        .header(header::COOKIE, &api.cookie)
        .body(Body::empty())
        .expect("request");
    assert_eq!(api.send(req).await.status(), StatusCode::OK);
}

#[tokio::test]
async fn a_foreign_host_is_refused_before_anything_else() {
    let env = Env::new();
    let api = env.app();
    let req = Request::post("/api/v1/auth/codes")
        .header(header::HOST, "evil.com:7069")
        .header(header::ORIGIN, "http://evil.com:7069")
        .header(header::COOKIE, &api.cookie)
        .header("x-odhive", "1")
        .body(Body::empty())
        .expect("request");
    let resp = api.send(req).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_no_cors(resp.headers(), "foreign host");
}

#[tokio::test]
async fn no_cors_headers_are_ever_sent() {
    let env = Env::new();
    let api = env.app();
    // A preflight from another site: refused, and nothing allows it.
    let req = Request::builder()
        .method(Method::OPTIONS)
        .uri("/api/v1/sources")
        .header(header::HOST, HOST)
        .header(header::ORIGIN, "http://evil.com")
        .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
        .header(header::ACCESS_CONTROL_REQUEST_HEADERS, "x-odhive")
        .body(Body::empty())
        .expect("request");
    let resp = api.send(req).await;
    assert_ne!(resp.status(), StatusCode::OK);
    assert_ne!(resp.status(), StatusCode::NO_CONTENT);
    assert_no_cors(resp.headers(), "preflight");

    // Reads from any origin, signed in or not, on API and UI routes.
    for path in [
        "/api/v1/health",
        "/api/v1/sources",
        "/api/v1/nope",
        "/",
        "/login",
    ] {
        for origin in [ORIGIN, "http://evil.com"] {
            for cookie in [Some(api.cookie.as_str()), None] {
                let mut req = Request::get(path)
                    .header(header::HOST, HOST)
                    .header(header::ORIGIN, origin);
                if let Some(c) = cookie {
                    req = req.header(header::COOKIE, c);
                }
                let resp = api.send(req.body(Body::empty()).expect("request")).await;
                assert_no_cors(resp.headers(), path);
            }
        }
    }
}

#[tokio::test]
async fn oversized_bodies_get_413() {
    let env = Env::new();
    let api = env.app();
    let limit = odhive_server::RequestLimits::MAX_BODY;
    let big = format!(r#"{{"path":"{}"}}"#, "a".repeat(limit));

    // Declared by Content-Length: refused before the body is read, on any route.
    for path in ["/api/v1/sources", "/api/v1/nope", "/"] {
        let req = Request::post(path)
            .header(header::HOST, HOST)
            .header(header::ORIGIN, ORIGIN)
            .header(header::COOKIE, &api.cookie)
            .header("x-odhive", "1")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::CONTENT_LENGTH, big.len())
            .body(Body::from(big.clone()))
            .expect("request");
        let (status, body) = split(api.send(req).await).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{path}: {body}");
        assert_eq!(body["error"], "body_too_large");
    }

    // Without a length (chunked): cut while it is read.
    let chunks: Vec<Result<String, std::io::Error>> = big
        .as_bytes()
        .chunks(64 * 1024)
        .map(|c| Ok(String::from_utf8(c.to_vec()).expect("ascii")))
        .collect();
    let req = Request::post("/api/v1/sources")
        .header(header::HOST, HOST)
        .header(header::ORIGIN, ORIGIN)
        .header(header::COOKIE, &api.cookie)
        .header("x-odhive", "1")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from_stream(futures_util::stream::iter(chunks)))
        .expect("request");
    let (status, body) = split(api.send(req).await).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert_eq!(body["error"], "body_too_large");

    // A body just under the limit is read (and fails validation instead).
    let (status, body) = api
        .post("/api/v1/sources", Some(json!({ "path": "a".repeat(1000) })))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // A Content-Length that is not a number.
    let req = Request::post("/api/v1/sources")
        .header(header::HOST, HOST)
        .header(header::ORIGIN, ORIGIN)
        .header(header::COOKIE, &api.cookie)
        .header("x-odhive", "1")
        .header(header::CONTENT_LENGTH, "lots")
        .body(Body::empty())
        .expect("request");
    let resp: Response = api.send(req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// Serves `env`'s app with `limits` on a free loopback port; returns the
/// port and a sender that stops the server.
async fn serve_limited(
    env: &Env,
    limits: Limits,
    ws: WsLimits,
) -> (Api, u16, tokio::sync::oneshot::Sender<()>) {
    let v4 = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = v4.local_addr().expect("addr").port();
    let api = env.app_with(
        ManagerOptions::default(),
        ServiceOptions {
            ws,
            ..ServiceOptions::default()
        },
        port,
    );
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let listeners = LoopbackListeners { port, v4, v6: None };
    tokio::spawn(odhive_server::serve_with(
        listeners,
        api.service.clone(),
        limits,
        async {
            let _ = rx.await;
        },
    ));
    (api, port, tx)
}

/// Sends a one-shot request (`Connection: close`) and reads the whole answer.
async fn http_get(stream: &mut TcpStream, port: u16) -> String {
    let req = format!("GET / HTTP/1.1\r\nHost: evil.com:{port}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.expect("write");
    let mut buf = String::new();
    stream.read_to_string(&mut buf).await.expect("read");
    buf
}

#[tokio::test]
async fn connections_over_the_limit_wait_for_a_free_slot() {
    let env = Env::new();
    let limits = Limits {
        connections: ConnectionLimits {
            max_connections: 1,
            header_read_timeout: Duration::from_secs(30),
            shutdown_grace: Duration::from_secs(1),
        },
        ..Limits::default()
    };
    let (_api, port, stop) = serve_limited(&env, limits, WsLimits::default()).await;

    // The only slot is held by an idle connection.
    let idle = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut second = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect (backlog)");
    let waiting = tokio::time::timeout(Duration::from_millis(500), http_get(&mut second, port));
    assert!(waiting.await.is_err(), "answered over the limit");

    // Once it closes, the waiting connection is served.
    drop(idle);
    let mut third = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    let answer = tokio::time::timeout(Duration::from_secs(10), http_get(&mut third, port)).await;
    // `second` was half-read above; `third` gets in after it.
    let answer = answer.expect("served after the slot freed up");
    assert!(answer.starts_with("HTTP/1.1 403"), "{answer}");
    let _ = stop.send(());
}

#[tokio::test]
async fn slow_headers_close_the_connection() {
    let env = Env::new();
    let limits = Limits {
        connections: ConnectionLimits {
            header_read_timeout: Duration::from_millis(300),
            ..ConnectionLimits::default()
        },
        ..Limits::default()
    };
    let (_api, port, stop) = serve_limited(&env, limits, WsLimits::default()).await;
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost")
        .await
        .expect("write");
    let mut buf = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut buf)).await;
    assert!(read.is_ok(), "connection still open");
    let text = String::from_utf8_lossy(&buf);
    assert!(!text.starts_with("HTTP/1.1 2"), "{text}");
    let _ = stop.send(());
}

#[tokio::test]
async fn websockets_over_the_limit_get_503() {
    let env = Env::new();
    let ws = WsLimits {
        max_connections: 1,
        ..WsLimits::default()
    };
    let (api, port, stop) = serve_limited(&env, Limits::default(), ws).await;
    let connect = || {
        let mut req = format!("ws://127.0.0.1:{port}/api/v1/ws")
            .into_client_request()
            .expect("request");
        req.headers_mut()
            .insert(header::AUTHORIZATION, api.bearer.parse().expect("bearer"));
        tokio_tungstenite::connect_async(req)
    };
    let (first, _) = connect().await.expect("first");
    match connect().await {
        Err(tungstenite::Error::Http(resp)) => {
            assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
            let body: serde_json::Value =
                serde_json::from_slice(resp.body().as_deref().unwrap_or_default()).expect("json");
            assert_eq!(body["error"], "too_many_connections");
        }
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("connected over the limit"),
    }

    // Closing the first frees the slot.
    drop(first);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if connect().await.is_ok() {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "slot never freed");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let _ = stop.send(());
}
