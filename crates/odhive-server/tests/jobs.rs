//! Background jobs over the REST API: cloning a source and setting up a
//! project, their conflicts and cancelling.
//!
//! Instances use pinned ports from 19300–19309.

use std::fs;
use std::os::unix::fs::PermissionsExt;

use axum::http::StatusCode;
use serde_json::{Value, json};

mod common;
use common::{Api, Env, job_options, python3};

fn steps<'a>(job: &'a Value, name: &str) -> Vec<&'a Value> {
    job["steps"]
        .as_array()
        .expect("steps")
        .iter()
        .filter(|s| s["step"] == name || s["event"] == name)
        .collect()
}

/// Skips a setup test where the stdlib venv module is missing and uv is
/// not installed (Debian/Ubuntu without python3-venv).
fn venv_unavailable(job: &Value) -> bool {
    job["error"]["error"] == "venv_failed"
        && job["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("python3-venv"))
}

async fn served(env: &Env, url: &str) -> Api {
    env.app_with(Default::default(), job_options(url), common::PORT)
}

#[tokio::test]
async fn a_source_is_cloned_verified_and_registered() {
    let env = Env::new();
    let url = env.upstream();
    let api = served(&env, &url).await;

    let (status, job) = api
        .post("/api/v1/sources/clone", Some(json!({ "version": "19.0" })))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    assert_eq!(job["kind"], "clone");
    assert_eq!(job["version"], "19.0");
    assert_eq!(job["state"], "running");

    let job = api.wait_job(job["id"].as_u64().expect("id")).await;
    assert_eq!(job["state"], "succeeded", "{job}");
    let target = env.home.join("OdHive/sources/odoo-19.0");
    assert_eq!(job["result"]["kind"], "clone");
    assert_eq!(job["result"]["source"]["id"], "odoo-19.0");
    assert_eq!(
        job["result"]["source"]["path"],
        target.to_str().expect("utf8")
    );
    assert_eq!(steps(&job, "started").len(), 1, "{job}");
    assert_eq!(steps(&job, "finished").len(), 1, "{job}");

    let (_, sources) = api.get("/api/v1/sources").await;
    assert_eq!(sources[0]["id"], "odoo-19.0", "{sources}");

    // The folder is there now.
    let (status, body) = api
        .post("/api/v1/sources/clone", Some(json!({ "version": "19.0" })))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "exists");

    // Only 19 and 20; no URL can be given.
    for bad in [json!({ "version": "18.0" }), json!({ "version": "x" })] {
        let (status, body) = api.post("/api/v1/sources/clone", Some(bad)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"], "bad_version");
    }
    let (status, body) = api
        .post(
            "/api/v1/sources/clone",
            Some(json!({ "version": "20.0", "url": "https://example.invalid/x.git" })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}

#[tokio::test]
async fn a_failed_clone_leaves_nothing_behind() {
    let env = Env::new();
    let url = env.upstream();
    let api = served(&env, &url).await;

    // The upstream has no 20.0 branch.
    let (status, job) = api
        .post("/api/v1/sources/clone", Some(json!({ "version": "20.0" })))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    let job = api.wait_job(job["id"].as_u64().expect("id")).await;
    assert_eq!(job["state"], "failed", "{job}");
    assert_eq!(job["error"]["error"], "git_failed", "{job}");
    assert!(job["result"].is_null());

    let left: Vec<_> = fs::read_dir(env.home.join("OdHive/sources"))
        .expect("read_dir")
        .collect();
    assert!(left.is_empty(), "{left:?}");
    let (_, sources) = api.get("/api/v1/sources").await;
    assert_eq!(sources, json!([]));
}

#[tokio::test]
async fn a_project_is_set_up_with_its_python() {
    let env = Env::new();
    let api = served(&env, "file:///nonexistent").await;
    let source = api.add_source(&env.source_dir()).await;
    api.create_project("shop", &source).await;

    let python = python3();
    let (status, job) = api
        .post(
            "/api/v1/projects/shop/setup",
            Some(json!({ "python": python.to_str().expect("utf8") })),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    assert_eq!(job["kind"], "setup");
    assert_eq!(job["project"], "shop");

    let id = job["id"].as_u64().expect("id");
    let job = api.wait_job(id).await;
    if venv_unavailable(&job) {
        return;
    }
    assert_eq!(job["state"], "succeeded", "{job}");
    assert_eq!(job["result"]["kind"], "setup");
    assert_eq!(job["result"]["report"]["installed"], 0);
    let venv_python = env.project_dir("shop").join(".venv/bin/python");
    assert_eq!(
        job["result"]["report"]["python"],
        venv_python.to_str().expect("utf8")
    );
    assert_eq!(steps(&job, "python").len(), 1, "{job}");
    assert_eq!(steps(&job, "collected").len(), 1, "{job}");

    let (_, project) = api.get("/api/v1/projects/shop").await;
    assert_eq!(project["has_venv"], true);

    // Listed, and set up again: the venv is kept.
    let (_, jobs) = api.get("/api/v1/jobs").await;
    assert_eq!(jobs.as_array().map(Vec::len), Some(1));
    let (status, again) = api
        .post("/api/v1/projects/shop/setup", Some(json!({})))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{again}");
    let again = api.wait_job(again["id"].as_u64().expect("id")).await;
    assert_eq!(again["state"], "succeeded", "{again}");
    assert_eq!(steps(&again, "venv_kept").len(), 1, "{again}");
}

#[tokio::test]
async fn setup_requests_are_checked() {
    let env = Env::new();
    let api = served(&env, "file:///nonexistent").await;
    let source = api.add_source(&env.source_dir()).await;
    api.create_project("shop", &source).await;

    let (status, body) = api
        .post("/api/v1/projects/nope/setup", Some(json!({})))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, body) = api
        .post(
            "/api/v1/projects/shop/setup",
            Some(json!({ "python": "bin/python3" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "bad_path");
    let (status, body) = api
        .post("/api/v1/projects/shop/setup", Some(json!({ "pip": true })))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // A Python that is not one fails the job, not the request.
    let fake = env.home.join("not-python");
    fs::write(&fake, "#!/bin/sh\nexit 3\n").expect("write");
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).expect("chmod");
    let (status, job) = api
        .post(
            "/api/v1/projects/shop/setup",
            Some(json!({ "python": fake.to_str().expect("utf8") })),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    let job = api.wait_job(job["id"].as_u64().expect("id")).await;
    assert_eq!(job["state"], "failed", "{job}");
    assert_eq!(job["error"]["error"], "python", "{job}");
    assert!(!env.project_dir("shop").join(".venv").exists());

    // Not while an instance runs.
    env.make_runnable("shop", 19300, 19301);
    let (status, body) = api
        .post("/api/v1/projects/shop/instances/main/start", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = api
        .post("/api/v1/projects/shop/setup", Some(json!({})))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "running");
    let (status, _) = api
        .post("/api/v1/projects/shop/instances/main/stop", None)
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_running_setup_blocks_the_project_until_cancelled() {
    let env = Env::new();
    let api = served(&env, "file:///nonexistent").await;
    let source = api.add_source(&env.source_dir()).await;
    api.create_project("shop", &source).await;
    env.make_runnable("shop", 19302, 19303);
    // Make the venv missing again so that setup probes the slow Python.
    fs::remove_dir_all(env.project_dir("shop").join(".venv")).expect("rm venv");

    // A "Python" that never answers its probe.
    let slow = env.home.join("slow-python");
    fs::write(&slow, "#!/bin/sh\nexec sleep 60\n").expect("write");
    fs::set_permissions(&slow, fs::Permissions::from_mode(0o755)).expect("chmod");
    let (status, job) = api
        .post(
            "/api/v1/projects/shop/setup",
            Some(json!({ "python": slow.to_str().expect("utf8") })),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    let id = job["id"].as_u64().expect("id");

    let (status, body) = api
        .post("/api/v1/projects/shop/setup", Some(json!({})))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "job_running");
    let (status, body) = api
        .post("/api/v1/projects/shop/instances/main/start", None)
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "setup_running");
    let (status, body) = api.delete("/api/v1/projects/shop").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "setup_running");

    let started = std::time::Instant::now();
    let (status, job) = api.post(&format!("/api/v1/jobs/{id}/cancel"), None).await;
    assert_eq!(status, StatusCode::OK, "{job}");
    assert_eq!(job["state"], "cancelled", "{job}");
    assert!(job["finished_at"].is_u64());
    assert!(started.elapsed() < std::time::Duration::from_secs(10));

    // The project is free again.
    let (status, body) = api.delete("/api/v1/projects/shop").await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    for (path, code) in [
        ("/api/v1/jobs/999", StatusCode::NOT_FOUND),
        ("/api/v1/jobs/abc", StatusCode::BAD_REQUEST),
    ] {
        let (status, body) = api.get(path).await;
        assert_eq!(status, code, "{body}");
    }
    let (status, _) = api.post("/api/v1/jobs/999/cancel", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
