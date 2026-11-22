//! Authentication: every API route and the WebSocket need the session
//! cookie or the bearer token; `/login` trades a one-time code for the
//! cookie.

use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use axum::response::Response;
use futures_util::StreamExt;
use odhive_core::manager::ManagerOptions;
use odhive_core::token::Token;
use odhive_server::ServiceOptions;
use odhive_server::auth::SESSION_COOKIE;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest};

mod common;
use common::{Api, Env, HOST, split};

/// A request with the right Host and mutation header, plus `extra` headers.
fn request(method: Method, path: &str, extra: &[(header::HeaderName, &str)]) -> Request<Body> {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, HOST)
        .header("x-odhive", "1");
    for (name, value) in extra {
        req = req.header(name, *value);
    }
    req.body(Body::empty()).expect("request")
}

async fn login(api: &Api, path: &str, cookie: Option<&str>) -> Response {
    let extra: Vec<_> = cookie.map(|c| (header::COOKIE, c)).into_iter().collect();
    api.send(request(Method::GET, path, &extra)).await
}

async fn new_code(api: &Api) -> Value {
    let (status, code) = api.post("/api/v1/auth/codes", None).await;
    assert_eq!(status, StatusCode::CREATED, "{code}");
    code
}

#[tokio::test]
async fn api_routes_need_a_signed_in_caller() {
    let env = Env::new();
    let api = env.app();
    let other = Token::generate().expect("token").to_hex();
    let wrong_cookie = format!("{SESSION_COOKIE}={other}");
    let wrong_bearer = format!("Bearer {other}");
    for (method, path) in [
        (Method::GET, "/api/v1/health"),
        (Method::GET, "/api/v1/sources"),
        (Method::GET, "/api/v1/projects/shop"),
        (Method::POST, "/api/v1/sources"),
        (Method::POST, "/api/v1/auth/codes"),
        (Method::DELETE, "/api/v1/sources/odoo-19.0"),
        (Method::GET, "/api/v1/ws"),
        (Method::GET, "/api/v1/nope"),
        (Method::GET, "/api"),
    ] {
        for extra in [
            vec![],
            vec![(header::COOKIE, wrong_cookie.as_str())],
            vec![(header::AUTHORIZATION, wrong_bearer.as_str())],
        ] {
            let resp = api.send(request(method.clone(), path, &extra)).await;
            assert_eq!(
                resp.headers()[header::WWW_AUTHENTICATE],
                "Bearer realm=\"odhive\""
            );
            let (status, body) = split(resp).await;
            assert_eq!(
                status,
                StatusCode::UNAUTHORIZED,
                "{method} {path} {extra:?}"
            );
            assert_eq!(body["error"], "unauthorized", "{method} {path}");
        }
    }
    // Nothing was registered by the refused POST.
    let (status, sources) = api.get("/api/v1/sources").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(sources, json!([]));
}

#[tokio::test]
async fn the_bearer_token_works_like_the_cookie() {
    let env = Env::new();
    let token = Token::generate().expect("token");
    let bearer = format!("Bearer {}", token.to_hex());
    let api = env.app_with_auth(
        ManagerOptions::default(),
        ServiceOptions::default(),
        common::PORT,
        token,
        Duration::from_secs(60),
    );
    let resp = api
        .send(request(
            Method::GET,
            "/api/v1/health",
            &[(header::AUTHORIZATION, &bearer)],
        ))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    // Signed in or not, mutations still need X-OdHive.
    let req = Request::post("/api/v1/auth/codes")
        .header(header::HOST, HOST)
        .header(header::AUTHORIZATION, &bearer)
        .body(Body::empty())
        .expect("request");
    let (status, body) = split(api.send(req).await).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"], "missing_header");
}

#[tokio::test]
async fn a_login_code_sets_the_cookie_once() {
    let env = Env::new();
    let api = env.app();
    let code = new_code(&api).await;
    let path = code["path"].as_str().expect("path");
    assert_eq!(code["expires_in"], 60);
    assert_eq!(
        path,
        format!("/login?t={}", code["code"].as_str().expect("code"))
    );

    let resp = login(&api, path, None).await;
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert_eq!(resp.headers()[header::LOCATION], "/");
    assert_eq!(resp.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(resp.headers()["referrer-policy"], "no-referrer");
    let set = resp.headers()[header::SET_COOKIE].to_str().expect("ascii");
    assert!(set.contains("; HttpOnly"), "{set}");
    assert!(set.contains("; SameSite=Strict"), "{set}");
    assert!(set.contains("; Path=/"), "{set}");
    let cookie = set.split(';').next().expect("pair").to_owned();
    assert_eq!(cookie, api.cookie);

    // The cookie from /login opens the API.
    let resp = api
        .send(request(
            Method::GET,
            "/api/v1/health",
            &[(header::COOKIE, &cookie)],
        ))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Used once: a second browser gets nothing.
    let resp = login(&api, path, None).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(resp.headers().get(header::SET_COOKIE).is_none());
    assert_eq!(resp.headers()[header::CACHE_CONTROL], "no-store");
    // The browser that is signed in is just sent home.
    let resp = login(&api, path, Some(&cookie)).await;
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert!(resp.headers().get(header::SET_COOKIE).is_none());
}

#[tokio::test]
async fn expired_or_made_up_codes_are_refused() {
    let env = Env::new();
    let api = env.app_with_auth(
        ManagerOptions::default(),
        ServiceOptions::default(),
        common::PORT,
        Token::generate().expect("token"),
        Duration::from_millis(200),
    );
    let code = new_code(&api).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let path = code["path"].as_str().expect("path");
    let (status, body) = split(login(&api, path, None).await).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "expired");
    assert!(body.as_str().expect("text").contains("odhive"), "{body}");

    let fresh = new_code(&api).await;
    let fresh = fresh["code"].as_str().expect("code");
    for bad in [
        "/login".to_owned(),
        "/login?t=".to_owned(),
        "/login?code=x".to_owned(),
        format!("/login?t={}", fresh.to_uppercase()),
        format!("/login?t={}", &fresh[..63]),
        format!("/login?t={}", api.cookie.split('=').nth(1).expect("token")),
        "/login?t=%ff%fe".to_owned(),
    ] {
        let resp = login(&api, &bad, None).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{bad}");
        assert!(resp.headers().get(header::SET_COOKIE).is_none(), "{bad}");
    }
    // The real code survived the guesses.
    let resp = login(&api, &format!("/login?t={fresh}"), None).await;
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn login_keeps_the_host_check() {
    let env = Env::new();
    let api = env.app();
    let code = new_code(&api).await;
    let req = Request::get(code["path"].as_str().expect("path"))
        .header(header::HOST, "evil.com:7069")
        .body(Body::empty())
        .expect("request");
    assert_eq!(api.send(req).await.status(), StatusCode::FORBIDDEN);
    // Refused before it was used.
    let resp = login(&api, code["path"].as_str().expect("path"), None).await;
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn the_web_ui_shell_needs_no_sign_in() {
    let env = Env::new();
    let api = env.app();
    let resp = api.send(request(Method::GET, "/", &[])).await;
    // 200 with a built UI; 503 "not built" page without one. Never 401.
    assert!(
        resp.status() == StatusCode::OK || resp.status() == StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        resp.status()
    );
}

#[tokio::test]
async fn websocket_without_the_cookie_is_refused() {
    let env = Env::new();
    let (api, port) = env
        .serve_with(ManagerOptions::default(), ServiceOptions::default())
        .await;
    let other = Token::generate().expect("token").to_hex();
    for cookie in [None, Some(format!("{SESSION_COOKIE}={other}"))] {
        let mut req = format!("ws://127.0.0.1:{port}/api/v1/ws")
            .into_client_request()
            .expect("request");
        req.headers_mut().insert(
            header::ORIGIN,
            format!("http://localhost:{port}").parse().expect("origin"),
        );
        if let Some(c) = &cookie {
            req.headers_mut()
                .insert(header::COOKIE, c.parse().expect("cookie"));
        }
        match tokio_tungstenite::connect_async(req).await {
            Err(tungstenite::Error::Http(resp)) => {
                assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{cookie:?}");
            }
            Err(e) => panic!("{e}"),
            Ok(_) => panic!("connected with {cookie:?}"),
        }
    }
    // Signed in, it connects.
    let mut req = format!("ws://127.0.0.1:{port}/api/v1/ws")
        .into_client_request()
        .expect("request");
    req.headers_mut()
        .insert(header::COOKIE, api.cookie.parse().expect("cookie"));
    let (mut ws, _) = tokio_tungstenite::connect_async(req)
        .await
        .expect("connect");
    let hello = tokio::time::timeout(Duration::from_secs(30), ws.next())
        .await
        .expect("hello in time")
        .expect("open")
        .expect("message");
    assert!(hello.to_text().expect("text").contains("hello"));
}
