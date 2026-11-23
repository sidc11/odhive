//! REST API tests: routes, errors, the mutation header, and instances run
//! end to end with the fake `odoo-bin`.
//!
//! Instances use pinned ports from 19100–19199 (below the ephemeral range,
//! away from the other test files' ranges); each test has its own block.

use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use serde_json::{Value, json};

mod common;
use common::{Api, Env, HOST, split};

const WAIT: Duration = Duration::from_secs(30);

/// Polls project `name` until its `main` instance satisfies `done`.
async fn wait_main(api: &Api, name: &str, what: &str, done: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + WAIT;
    loop {
        let (status, body) = api.get(&format!("/api/v1/projects/{name}")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let main = body["instances"][0].clone();
        if done(&main) {
            return main;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {main}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn state(main: &Value) -> &str {
    main["live"]["status"]["state"].as_str().unwrap_or("")
}

#[tokio::test]
async fn health_and_unknown_api_routes() {
    let env = Env::new();
    let api = env.app();
    let (status, body) = api.get("/api/v1/health").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));

    // Unknown API paths are JSON 404s, never the web UI's index.html.
    for path in ["/api/v1/nope", "/api/v2/health", "/api", "/api/x/y"] {
        let (status, body) = api.get(path).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert_eq!(body["error"], "not_found", "{path}: {body}");
    }
}

#[tokio::test]
async fn quit_asks_the_service_to_end() {
    let env = Env::new();
    let api = env.app();
    let waiter = {
        let svc = api.service.clone();
        tokio::spawn(async move { svc.quit_requested().await })
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!waiter.is_finished(), "not asked yet");

    // A mutation like any other: signed in and marked.
    let req = Request::post("/api/v1/service/quit")
        .header(header::HOST, HOST)
        .header(header::ORIGIN, common::ORIGIN)
        .header("x-odhive", "1")
        .body(Body::empty())
        .expect("request");
    assert_eq!(api.send(req).await.status(), StatusCode::UNAUTHORIZED);
    let req = Request::post("/api/v1/service/quit")
        .header(header::HOST, HOST)
        .header(header::COOKIE, &api.cookie)
        .header(header::ORIGIN, common::ORIGIN)
        .body(Body::empty())
        .expect("request");
    assert_eq!(api.send(req).await.status(), StatusCode::FORBIDDEN);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!waiter.is_finished(), "refused requests change nothing");

    // The CLI: bearer token, no Origin.
    let req = Request::post("/api/v1/service/quit")
        .header(header::HOST, HOST)
        .header(header::AUTHORIZATION, &api.bearer)
        .header("x-odhive", "1")
        .body(Body::empty())
        .expect("request");
    assert_eq!(api.send(req).await.status(), StatusCode::ACCEPTED);
    tokio::time::timeout(WAIT, waiter)
        .await
        .expect("quit seen")
        .expect("join");
    // Also seen by whoever starts waiting later.
    tokio::time::timeout(WAIT, api.service.quit_requested())
        .await
        .expect("late waiter");
}

#[tokio::test]
async fn api_keeps_the_host_check() {
    let env = Env::new();
    let api = env.app();
    let req = Request::get("/api/v1/sources")
        .header(header::HOST, "evil.com:7069")
        .body(Body::empty())
        .expect("request");
    assert_eq!(api.send(req).await.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn mutations_need_the_odhive_header() {
    let env = Env::new();
    let api = env.app();
    let body = json!({ "path": env.source_dir().to_str().expect("utf8") }).to_string();
    for (method, path, value) in [
        (Method::POST, "/api/v1/sources", None),
        (Method::POST, "/api/v1/sources", Some("0")),
        (Method::POST, "/api/v1/sources", Some("true")),
        (Method::DELETE, "/api/v1/sources/odoo-19.0", None),
        (Method::PUT, "/api/v1/sources", None),
        (
            Method::POST,
            "/api/v1/projects/p/instances/main/start",
            None,
        ),
        (Method::POST, "/api/v1/nope", None),
    ] {
        let mut req = Request::builder()
            .method(method.clone())
            .uri(path)
            .header(header::HOST, HOST)
            .header(header::ORIGIN, common::ORIGIN)
            .header(header::COOKIE, &api.cookie)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(v) = value {
            req = req.header("x-odhive", v);
        }
        let (status, resp) = split(
            api.send(req.body(Body::from(body.clone())).expect("r"))
                .await,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{method} {path} {value:?}: {resp}"
        );
        assert_eq!(resp["error"], "missing_header");
    }
    // Nothing was registered.
    let (_, sources) = api.get("/api/v1/sources").await;
    assert_eq!(sources, json!([]));
}

#[tokio::test]
async fn sources_are_registered_listed_and_removed() {
    let env = Env::new();
    let api = env.app();
    let (status, body) = api
        .post("/api/v1/sources", Some(json!({ "path": "odoo-src" })))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "bad_path");

    let not_odoo = env.home.join("empty");
    std::fs::create_dir(&not_odoo).expect("mkdir");
    let (status, body) = api
        .post(
            "/api/v1/sources",
            Some(json!({ "path": not_odoo.to_str() })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"], "bad_source");

    let (status, body) = api
        .post(
            "/api/v1/sources",
            Some(json!({ "path": env.source_dir().to_str(), "enterprise_version": "18.x" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "bad_version");

    let id = api.add_source(&env.source_dir()).await;
    assert_eq!(id, "odoo-19.0");
    // Registering the same tree again returns the same entry.
    assert_eq!(api.add_source(&env.source_dir()).await, id);
    let (status, list) = api.get("/api/v1/sources").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().map(Vec::len), Some(1), "{list}");
    assert_eq!(list[0]["edition"], "community");
    assert_eq!(list[0]["odoo_version"], "19.0");
    assert_eq!(list[0]["path"], env.source_dir().to_str().expect("utf8"));

    // No cache yet: the bundled option list, nothing run.
    let (status, schema) = api.get("/api/v1/sources/odoo-19.0/schema").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(schema["source"], "bundled");
    assert_eq!(schema["schema"]["series"], "19.0");
    let http_port = schema["schema"]["options"]
        .as_array()
        .expect("options")
        .iter()
        .find(|o| o["key"] == "http_port")
        .expect("http_port")
        .clone();
    assert_eq!(http_port["type"], "int");
    let (status, _) = api.get("/api/v1/sources/nope/schema").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, body) = api.delete("/api/v1/sources/nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"], "unknown_source");
    let (status, body) = api.delete("/api/v1/sources/odoo-19.0").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], "odoo-19.0");
    let (_, list) = api.get("/api/v1/sources").await;
    assert_eq!(list, json!([]));
    assert!(
        env.source_dir().join("odoo-bin").is_file(),
        "files are kept"
    );
}

#[tokio::test]
async fn bodies_are_validated() {
    let env = Env::new();
    let api = env.app();
    let path = env.source_dir();
    let path = path.to_str().expect("utf8");

    let req = Request::post("/api/v1/sources")
        .header(header::HOST, HOST)
        .header(header::ORIGIN, common::ORIGIN)
        .header(header::COOKIE, &api.cookie)
        .header("x-odhive", "1")
        .header(header::CONTENT_TYPE, "text/plain")
        .body(Body::from(json!({ "path": path }).to_string()))
        .expect("request");
    let (status, body) = split(api.send(req).await).await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{body}");
    assert_eq!(body["error"], "bad_body");

    let (status, body) = api
        .post("/api/v1/sources", Some(json!({ "path": path, "extra": 1 })))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"], "bad_body");

    let req = Request::post("/api/v1/sources")
        .header(header::HOST, HOST)
        .header(header::ORIGIN, common::ORIGIN)
        .header(header::COOKIE, &api.cookie)
        .header("x-odhive", "1")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{not json"))
        .expect("request");
    let (status, body) = split(api.send(req).await).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "bad_body");
}

#[tokio::test]
async fn projects_are_created_listed_and_removed() {
    let env = Env::new();
    let api = env.app();
    let source = api.add_source(&env.source_dir()).await;

    for (req, code, error) in [
        (
            json!({ "name": "Shop", "community": source }),
            StatusCode::BAD_REQUEST,
            "invalid_name",
        ),
        (
            json!({ "name": "../x", "community": source }),
            StatusCode::BAD_REQUEST,
            "invalid_name",
        ),
        (
            json!({ "name": "shop", "community": "odoo-18.0" }),
            StatusCode::NOT_FOUND,
            "unknown_source",
        ),
        (
            json!({ "name": "shop", "community": source, "enterprise": "nope" }),
            StatusCode::NOT_FOUND,
            "unknown_source",
        ),
        (
            json!({ "name": "shop", "community": source, "addons_paths": ["rel/addons"] }),
            StatusCode::BAD_REQUEST,
            "bad_path",
        ),
        (
            json!({ "name": "shop", "community": source, "addons_paths": ["/does/not/exist"] }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_addons_path",
        ),
        (
            json!({ "name": "shop", "community": source, "python": "python3" }),
            StatusCode::BAD_REQUEST,
            "bad_path",
        ),
        (
            json!({ "name": "shop", "community": source, "instance": { "id": "Main" } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_project",
        ),
        (
            json!({ "name": "shop", "community": source, "instance": { "db": "a b" } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_project",
        ),
        (
            json!({ "name": "shop", "community": source, "instance": { "http_port": 80 } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_project",
        ),
        (
            json!({ "name": "shop", "community": source, "instance": { "gevent_port": 8072 } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_body",
        ),
    ] {
        let (status, body) = api.post("/api/v1/projects", Some(req.clone())).await;
        assert_eq!(status, code, "{req}: {body}");
        assert_eq!(body["error"], error, "{req}: {body}");
    }
    assert!(
        !env.project_dir("shop").exists(),
        "failed creates leave nothing"
    );

    let created = api.create_project("shop", &source).await;
    assert_eq!(created["name"], "shop");
    assert_eq!(created["odoo_version"], "19.0");
    assert_eq!(created["community"], source);
    assert_eq!(created["has_venv"], false);
    assert_eq!(
        created["path"],
        env.project_dir("shop").to_str().expect("utf8")
    );
    assert_eq!(created["instances"][0]["id"], "main");
    assert_eq!(created["instances"][0]["db"], "shop_main");
    assert_eq!(created["instances"][0]["reload_mode"], "auto");
    assert_eq!(created["instances"][0]["live"], Value::Null);
    assert_eq!(created["instances"][0]["external"], Value::Null);
    assert!(created.get("error").is_none());

    // The first instance can be named, given a database and a pinned port.
    let (status, custom) = api
        .post(
            "/api/v1/projects",
            Some(json!({
                "name": "custom",
                "community": source,
                "instance": { "id": "dev", "db": "custom_db", "http_port": 8070 },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{custom}");
    assert_eq!(custom["instances"].as_array().map(Vec::len), Some(1));
    assert_eq!(custom["instances"][0]["id"], "dev");
    assert_eq!(custom["instances"][0]["db"], "custom_db");
    assert_eq!(custom["instances"][0]["http_port"], 8070);
    assert_eq!(custom["instances"][0]["gevent_port"], Value::Null);
    assert_eq!(
        api.delete("/api/v1/projects/custom").await.0,
        StatusCode::NO_CONTENT
    );

    let (status, body) = api
        .post(
            "/api/v1/projects",
            Some(json!({ "name": "shop", "community": source })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "exists");

    let (status, list) = api.get("/api/v1/projects").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().map(Vec::len), Some(1));
    assert_eq!(list[0], created);
    let (status, body) = api.get("/api/v1/projects/nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "unknown_project");

    // A project whose file is broken is still listed, with the reason.
    let (status, _) = api
        .post(
            "/api/v1/projects",
            Some(json!({ "name": "broken", "community": source })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    std::fs::write(
        env.project_dir("broken").join("odhive.toml"),
        "nonsense = [",
    )
    .expect("w");
    let (_, list) = api.get("/api/v1/projects").await;
    let broken = list
        .as_array()
        .expect("list")
        .iter()
        .find(|p| p["name"] == "broken")
        .expect("listed");
    assert!(
        broken["error"]
            .as_str()
            .is_some_and(|e| e.contains("invalid project file")),
        "{broken}"
    );
    assert_eq!(
        api.get("/api/v1/projects/broken").await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );

    let (status, body) = api.delete(&format!("/api/v1/sources/{source}")).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "in_use");

    let (status, body) = api
        .post("/api/v1/projects/shop/instances/main/start", None)
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "no_venv");
    let (status, body) = api
        .post("/api/v1/projects/shop/instances/nope/start", None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"], "unknown_instance");
    let (status, body) = api.get("/api/v1/projects/shop/instances/main/logs").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "not_running");
    for q in ["limit=0", "limit=5001", "limit=x", "from=-1", "other=1"] {
        let (status, body) = api
            .get(&format!("/api/v1/projects/shop/instances/main/logs?{q}"))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{q}: {body}");
        assert_eq!(body["error"], "bad_query", "{q}");
    }
    let (status, body) = api
        .post("/api/v1/projects/shop/instances/main/fix/confirm", None)
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "not_running");
    let (status, body) = api
        .post("/api/v1/projects/shop/instances/main/stop", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({ "was_running": false, "killed": false, "status": null })
    );

    // The schema of a project without a venv is the bundled one.
    let (status, schema) = api.get("/api/v1/projects/shop/schema").await;
    assert_eq!(status, StatusCode::OK, "{schema}");
    assert_eq!(schema["source"], "bundled");

    for name in ["shop", "broken"] {
        let (status, body) = api.delete(&format!("/api/v1/projects/{name}")).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    }
    assert_eq!(
        api.get("/api/v1/projects/shop").await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        api.delete("/api/v1/projects/shop").await.0,
        StatusCode::NOT_FOUND
    );
    assert!(
        env.project_dir("shop").join("odhive.toml").is_file(),
        "folder kept"
    );
    let (status, _) = api.delete(&format!("/api/v1/sources/{source}")).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread")]
async fn instance_starts_logs_restarts_and_stops() {
    let env = Env::new();
    let api = env.app();
    let source = api.add_source(&env.source_dir()).await;
    api.create_project("shop", &source).await;
    env.make_runnable("shop", 19_100, 19_101);

    let (status, info) = api
        .post("/api/v1/projects/shop/instances/main/start", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{info}");
    assert_eq!(info["project"], "shop");
    assert_eq!(info["instance"], "main");
    assert_eq!(info["http_port"], 19_100);
    assert_eq!(info["db"], "shop_main");
    assert_eq!(info["supervised"], true);
    assert_eq!(info["port_changes"], json!([]));
    let main = wait_main(&api, "shop", "running", |m| state(m) == "running").await;
    assert_eq!(main["external"], Value::Null);

    // CPU and memory of the running process.
    let (status, usage) = api.get("/api/v1/instances/usage").await;
    assert_eq!(status, StatusCode::OK, "{usage}");
    let usage = usage.as_array().expect("array");
    assert_eq!(usage.len(), 1, "{usage:?}");
    assert_eq!(usage[0]["project"], "shop");
    assert_eq!(usage[0]["instance"], "main");
    assert_eq!(usage[0]["pid"], main["live"]["status"]["pid"]);
    assert!(usage[0]["memory_bytes"].as_u64().is_some_and(|m| m > 0));
    assert!(usage[0]["processes"].as_u64().is_some_and(|n| n >= 1));
    assert!(usage[0]["started_at"].as_u64().is_some_and(|t| t > 0));
    assert!(usage[0]["cpu_percent"].as_f64().is_some());

    let (status, body) = api
        .post("/api/v1/projects/shop/instances/main/start", None)
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "already_running");
    let (_, list) = api.get("/api/v1/instances").await;
    assert_eq!(list.as_array().map(Vec::len), Some(1), "{list}");
    let (status, body) = api.delete("/api/v1/projects/shop").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "running");

    // The newest entries, then paging on from `next`.
    let deadline = Instant::now() + WAIT;
    let page = loop {
        let (status, page) = api.get("/api/v1/projects/shop/instances/main/logs").await;
        assert_eq!(status, StatusCode::OK, "{page}");
        let records = page["records"].as_array().expect("records");
        if records.iter().any(|r| r["logger"] == "odoo.service.server") {
            break page;
        }
        assert!(Instant::now() < deadline, "no Odoo log entries: {page}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let records = page["records"].as_array().expect("records");
    assert_eq!(records[0]["seq"], 1);
    let next = page["next"].as_u64().expect("next");
    assert_eq!(
        next,
        records.last().expect("one")["seq"].as_u64().expect("seq") + 1
    );
    let server = records
        .iter()
        .find(|r| r["logger"] == "odoo.service.server")
        .expect("entry");
    assert_eq!(server["level"], "info");
    assert!(
        server["timestamp"]
            .as_str()
            .is_some_and(|t| t.ends_with('Z')),
        "{server}"
    );
    let (_, first) = api
        .get("/api/v1/projects/shop/instances/main/logs?from=1&limit=1")
        .await;
    assert_eq!(first["records"].as_array().map(Vec::len), Some(1));
    assert_eq!(first["next"], 2);

    let (status, info) = api
        .post("/api/v1/projects/shop/instances/main/restart", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{info}");
    wait_main(&api, "shop", "running after restart", |m| {
        state(m) == "running"
    })
    .await;

    let (status, body) = api
        .post("/api/v1/projects/shop/instances/main/stop", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["was_running"], true);
    assert_eq!(body["killed"], false);
    assert_eq!(body["status"]["state"], "stopped");
    let main = wait_main(&api, "shop", "forgotten", |m| m["live"].is_null()).await;
    assert_eq!(main["external"], Value::Null);
    let (_, list) = api.get("/api/v1/instances").await;
    assert_eq!(list, json!([]));
    let (_, usage) = api.get("/api/v1/instances/usage").await;
    assert_eq!(usage, json!([]));
}

#[tokio::test]
async fn projects_show_the_git_branch_of_their_addons() {
    let env = Env::new();
    let api = env.app();
    let source = api.add_source(&env.source_dir()).await;
    let project = api.create_project("shop", &source).await;
    assert_eq!(project["git"], json!([]));

    // The project's own addons/ folder sits in a checkout of the project.
    let dir = env.project_dir("shop");
    std::fs::create_dir_all(dir.join(".git")).expect("mkdir");
    std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/feature/pos\n").expect("write");
    let (status, project) = api.get("/api/v1/projects/shop").await;
    assert_eq!(status, StatusCode::OK, "{project}");
    assert_eq!(
        project["git"],
        json!([{ "repo": dir.display().to_string(), "branch": "feature/pos", "commit": null }])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn instance_run_elsewhere_is_shown_and_can_be_stopped() {
    let env = Env::new();
    let ours = env.app();
    // A second service on the same home stands in for a foreground
    // `odhive start` in a terminal.
    let other = env.app();
    let source = ours.add_source(&env.source_dir()).await;
    ours.create_project("shop", &source).await;
    env.make_runnable("shop", 19_110, 19_111);

    let (status, body) = other
        .post("/api/v1/projects/shop/instances/main/start", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    wait_main(&other, "shop", "running", |m| state(m) == "running").await;

    let main = wait_main(&ours, "shop", "external", |m| !m["external"].is_null()).await;
    assert_eq!(main["live"], Value::Null);
    assert_eq!(main["external"]["http_port"], 19_110);
    let (_, usage) = ours.get("/api/v1/instances/usage").await;
    assert_eq!(usage[0]["instance"], "main", "{usage}");
    assert_eq!(usage[0]["pid"], main["external"]["pid"]);
    assert_eq!(main["external"]["supervisor"], std::process::id());
    for action in ["start", "restart"] {
        let (status, body) = ours
            .post(
                &format!("/api/v1/projects/shop/instances/main/{action}"),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{action}: {body}");
        assert_eq!(body["error"], "running_elsewhere");
    }
    let (status, body) = ours.delete("/api/v1/projects/shop").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    let (status, body) = ours
        .post("/api/v1/projects/shop/instances/main/stop", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["was_running"], true);
    assert_eq!(body["status"], Value::Null);
    // The supervising service sees an ordinary exit.
    wait_main(&other, "shop", "stopped", |m| state(m) == "stopped").await;
    let main = wait_main(&ours, "shop", "gone", |m| m["external"].is_null()).await;
    assert_eq!(main["live"], Value::Null);
}
