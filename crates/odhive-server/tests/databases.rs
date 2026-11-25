//! `GET /projects/{name}/postgres`: the project's PostgreSQL server.

use axum::http::StatusCode;
use serde_json::json;

mod common;
use common::Env;

#[tokio::test]
async fn project_db_settings_are_used_and_the_password_never_shown() {
    let env = Env::new();
    let api = env.app();
    let source = api.add_source(&env.source_dir()).await;
    api.create_project("shop", &source).await;

    // Bound then dropped: nothing listens there.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .expect("bind")
        .port();
    let (status, body) = api
        .put(
            "/api/v1/projects/shop/options",
            json!({
                "options": {
                    "db_host": "127.0.0.1",
                    "db_port": port.to_string(),
                    "db_user": "odoo_dev",
                },
                "secrets": { "db_password": "pg-s3cret-value" },
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = api.get("/api/v1/projects/shop/postgres").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body.to_string().contains("pg-s3cret-value"), "{body}");
    assert_eq!(
        body["settings"],
        json!({
            "socket_dirs": [],
            "host": "127.0.0.1",
            "port": port,
            "user": "odoo_dev",
            "password_set": true,
            "ssl_mode": "prefer",
        })
    );
    assert_eq!(body["check"]["status"], "error", "{body}");
    assert_eq!(body["check"]["id"], "postgresql");
    assert!(body["server"].is_null());
    let tried = format!("127.0.0.1:{port}");
    assert!(
        body["check"]["details"]
            .as_array()
            .expect("details")
            .iter()
            .any(|d| d.as_str().is_some_and(|d| d.starts_with(&tried))),
        "{body}"
    );

    let (status, body) = api.get("/api/v1/projects/nope/postgres").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

#[tokio::test]
async fn tls_only_sslmode_is_reported() {
    let env = Env::new();
    let api = env.app();
    let source = api.add_source(&env.source_dir()).await;
    api.create_project("shop", &source).await;
    let (status, body) = api
        .put(
            "/api/v1/projects/shop/options",
            json!({ "options": { "db_host": "127.0.0.1", "db_sslmode": "verify-full" } }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, body) = api.get("/api/v1/projects/shop/postgres").await;
    assert_eq!(body["settings"]["ssl_mode"], "verify-full");
    assert_eq!(body["check"]["status"], "error");
    assert!(
        body["check"]["summary"]
            .as_str()
            .is_some_and(|s| s.contains("needs TLS")),
        "{body}"
    );
}
