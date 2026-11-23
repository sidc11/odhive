//! `GET /pythons`: interpreters found on this machine, judged against a
//! source's Python range.

use std::time::Duration;

use axum::http::StatusCode;
use odhive_core::python::DiscoverOptions;
use odhive_server::service::ServiceOptions;
use serde_json::Value;

mod common;
use common::{Env, python3};

/// Finds only the test's `python3` (no `PATH` scan, no uv).
fn only_python3() -> ServiceOptions {
    ServiceOptions {
        discover: DiscoverOptions {
            path_var: None,
            uv: None,
            explicit: vec![python3()],
            timeout: Duration::from_secs(30),
        },
        ..ServiceOptions::default()
    }
}

#[tokio::test]
async fn interpreters_are_listed_and_judged_against_a_source() {
    let env = Env::new();
    let api = env.app_with(Default::default(), only_python3(), common::PORT);
    let source = api.add_source(&env.source_dir()).await;

    let (status, plain) = api.get("/api/v1/pythons").await;
    assert_eq!(status, StatusCode::OK, "{plain}");
    assert_eq!(plain["support"], Value::Null);
    assert_eq!(plain["recommended"], Value::Null);
    assert_eq!(plain["problem"], Value::Null);
    let found = plain["interpreters"].as_array().expect("interpreters");
    assert_eq!(found.len(), 1, "{plain}");
    assert_eq!(found[0]["compat"], Value::Null);
    assert!(
        found[0]["version"]
            .as_str()
            .is_some_and(|v| v.starts_with("3."))
    );

    let (status, judged) = api.get(&format!("/api/v1/pythons?source={source}")).await;
    assert_eq!(status, StatusCode::OK, "{judged}");
    assert_eq!(judged["support"]["min"].as_array().map(Vec::len), Some(2));
    let python = &judged["interpreters"][0];
    if python["compat"] == "supported" && python["standard"] == true {
        assert_eq!(judged["recommended"], python["path"], "{judged}");
        assert_eq!(judged["problem"], Value::Null);
    } else {
        assert_eq!(judged["recommended"], Value::Null, "{judged}");
        assert!(judged["problem"].is_string(), "{judged}");
    }
}

#[tokio::test]
async fn unknown_or_enterprise_sources_are_refused() {
    let env = Env::new();
    let api = env.app_with(Default::default(), only_python3(), common::PORT);

    let (status, body) = api.get("/api/v1/pythons?source=nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"], "unknown_source");

    let (status, body) = api.get("/api/v1/pythons?other=1").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "bad_query");

    let enterprise = env.home.join("enterprise");
    std::fs::create_dir_all(enterprise.join("web_enterprise")).expect("mkdir");
    std::fs::write(
        enterprise.join("web_enterprise/__manifest__.py"),
        "{'name': 'Web Enterprise'}",
    )
    .expect("write");
    let (status, entry) = api
        .post(
            "/api/v1/sources",
            Some(serde_json::json!({
                "path": enterprise.to_str().expect("utf8"),
                "enterprise_version": "19.0",
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{entry}");
    let id = entry["id"].as_str().expect("id");
    let (status, body) = api.get(&format!("/api/v1/pythons?source={id}")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"], "bad_source");
}
