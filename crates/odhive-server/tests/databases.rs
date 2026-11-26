//! `GET /projects/{name}/postgres` and `/databases`: the project's
//! PostgreSQL server and the databases on it.

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use axum::body::Body;
use axum::http::StatusCode;
use odhive_core::project::ProjectConfig;
use odhive_server::security::{RequestLimits, UploadLimits};
use odhive_server::{AppConfig, app};
use serde_json::{Value, json};

mod common;
use common::Env;
use common::pg::TempPg;

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

#[tokio::test]
async fn database_list_reports_an_unusable_server_and_echoes_the_filter() {
    let env = Env::new();
    let api = env.app();
    let source = api.add_source(&env.source_dir()).await;
    api.create_project("shop", &source).await;
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
                    "dbfilter": "^%d$",
                },
                "secrets": { "db_password": "pg-s3cret-value" },
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = api.get("/api/v1/projects/shop/databases").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body.to_string().contains("pg-s3cret-value"), "{body}");
    assert_eq!(body["databases"], json!([]));
    assert_eq!(body["error"]["status"], "error", "{body}");
    assert_eq!(body["filter"], "^%d$");
    assert!(body["filter_note"].as_str().is_some(), "{body}");
    assert_eq!(body["hidden"], 0);
    assert_eq!(body["settings"]["port"], port);

    let (status, _) = api.get("/api/v1/projects/nope/databases").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn database_creation_is_refused_before_anything_runs() {
    let env = Env::new();
    let api = env.app();
    let source = api.add_source(&env.source_dir()).await;
    api.create_project("shop", &source).await;
    let create = |body: serde_json::Value| {
        let api = &api;
        async move {
            api.post("/api/v1/projects/shop/databases", Some(body))
                .await
        }
    };

    // Names that could break out of an argument or a query.
    for name in [
        "x;DROP DATABASE postgres",
        "x'y",
        "x\"y",
        "two words",
        "x;--",
        "--x",
        "-x",
        "_x",
        "a",
        "",
    ] {
        let (status, body) = create(json!({ "name": name, "password": "pw" })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{name:?}: {body}");
        assert!(
            ["invalid_name", "invalid_request"].contains(&body["error"].as_str().unwrap_or("")),
            "{name:?}: {body}"
        );
    }
    for (field, value) in [
        ("language", json!("en-US")),
        ("country", json!("BEL")),
        ("login", json!(" admin")),
        ("modules", json!(["../../etc"])),
        ("modules", json!(["sale,stock"])),
    ] {
        let mut body = json!({ "name": "shop_main", "password": "pw" });
        body[field] = value;
        let (status, body) = create(body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{field}: {body}");
        assert_eq!(body["error"], "invalid_request", "{body}");
    }
    let (status, _) = create(json!({ "name": "shop_main" })).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "password is required"
    );
    let (status, _) = create(json!({ "name": "shop_main", "password": "pw", "x": 1 })).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "unknown fields are refused"
    );

    // No venv yet.
    let (status, body) = create(json!({ "name": "shop_main", "password": "pw" })).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "no_venv");

    // Modules must exist in the project's addons paths.
    env.make_runnable("shop", 18_069, 18_072);
    let (status, body) = create(json!({
        "name": "shop_main",
        "password": "pw",
        "modules": ["sale", "base"],
    }))
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "unknown_modules");
    assert!(body["message"].as_str().is_some_and(|m| m.contains("sale")));

    // A server that cannot be used is reported before a job starts.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .expect("bind")
        .port();
    let (status, body) = api
        .put(
            "/api/v1/projects/shop/options",
            json!({ "options": { "db_host": "127.0.0.1", "db_port": port.to_string() } }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = create(json!({ "name": "shop_main", "password": "Very-s3cret" })).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"], "postgres_unavailable");
    assert!(!body.to_string().contains("Very-s3cret"));
    let (_, jobs) = api.get("/api/v1/jobs").await;
    assert_eq!(jobs, json!([]), "no job was started");

    let (status, _) = api
        .post(
            "/api/v1/projects/nope/databases",
            Some(json!({ "name": "shop_main", "password": "pw" })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

const RESTORE: &str = "/api/v1/projects/shop/databases/restore";

/// Stand-ins for the PostgreSQL client tools: present, but do nothing.
fn dummy_pg_tools(dir: &Path, names: &[&str]) {
    std::fs::create_dir_all(dir).expect("mkdir");
    for name in names {
        let p = dir.join(name);
        std::fs::write(&p, "#!/bin/sh\nexit 0\n").expect("write");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
}

/// Staging folders left in the project's data folder.
fn leftovers(env: &Env) -> Vec<String> {
    std::fs::read_dir(env.project_dir("shop").join("data"))
        .map(|d| {
            d.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with(".odhive-restore-"))
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn restore_is_refused_before_anything_runs() {
    let env = Env::new();
    let api = env.app();
    let source = api.add_source(&env.source_dir()).await;
    api.create_project("shop", &source).await;
    let up = |query: &str| {
        let api = &api;
        let path = format!("{RESTORE}?{query}");
        async move { api.upload(&path, Body::from("SELECT 1;"), None).await }
    };

    for name in [
        "x;DROP DATABASE postgres",
        "x'y",
        "x\"y",
        "two words",
        "x;--",
        "-x",
        "_x",
        "a",
    ] {
        let encoded: String = name
            .bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' {
                    (b as char).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect();
        let (status, body) = up(&format!("name={encoded}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{name:?}: {body}");
        assert!(
            ["invalid_name", "invalid_request"].contains(&body["error"].as_str().unwrap_or("")),
            "{name:?}: {body}"
        );
    }
    let (status, _) = up("").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "name is required");
    let (status, _) = up("name=shop_main&x=1").await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "unknown fields are refused"
    );
    let (status, _) = api
        .upload(
            "/api/v1/projects/nope/databases/restore?name=shop_main",
            Body::empty(),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Declared too large: refused before the handler, whatever the rest.
    let (status, body) = api
        .upload(
            &format!("{RESTORE}?name=shop_main"),
            Body::empty(),
            Some(UploadLimits::MAX_BYTES + 1),
        )
        .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert_eq!(body["error"], "body_too_large");
    // Other routes keep the 1 MiB limit.
    let (status, _) = api
        .upload(
            "/api/v1/projects/shop/databases",
            Body::empty(),
            Some(2 * 1024 * 1024),
        )
        .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);

    let (status, body) = up("name=shop_main").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "no_venv");

    env.make_runnable("shop", 18_169, 18_172);
    let tools = env.project_dir("shop").join("pgbin");
    dummy_pg_tools(&tools, &["psql", "createdb"]);
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .expect("bind")
        .port();
    let (status, body) = api
        .put(
            "/api/v1/projects/shop/options",
            json!({ "options": {
                "db_host": "127.0.0.1",
                "db_port": port.to_string(),
                "pg_path": tools.to_str().expect("utf-8"),
            } }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = up("name=shop_main").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "pg_tools_missing");
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|m| m.contains("pg_restore"))
    );

    dummy_pg_tools(&tools, &["pg_restore", "pg_dump"]);
    let (status, body) = up("name=shop_main").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"], "postgres_unavailable");

    let (_, jobs) = api.get("/api/v1/jobs").await;
    assert_eq!(jobs, json!([]), "no job was started");
    assert!(leftovers(&env).is_empty());
}

/// Gives project `shop` a venv and points it (and the fake odoo-bin) at `pg`.
async fn point_at(env: &Env, api: &common::Api, pg: &TempPg) {
    env.make_runnable("shop", 18_269, 18_272);
    let dir = env.project_dir("shop");
    let mut p = ProjectConfig::load(&dir).expect("load");
    let socket = pg.socket_dir.to_str().expect("utf-8").to_owned();
    for (k, v) in [
        ("PGHOST", socket.as_str()),
        ("PGPORT", "5432"),
        ("FAKE_PG_BIN", pg.bin.to_str().expect("utf-8")),
    ] {
        p.env.0.insert(k.into(), v.into());
    }
    p.save(&dir).expect("save");
    let (status, body) = api
        .put(
            "/api/v1/projects/shop/options",
            json!({ "options": {
                "db_host": socket,
                "db_port": "5432",
                "pg_path": pg.bin.to_str().expect("utf-8"),
            } }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

fn odoo_like_db(pg: &TempPg, name: &str) {
    pg.psql("postgres", &format!("CREATE DATABASE {name}"));
    pg.psql(
        name,
        "CREATE TABLE ir_module_module (name text PRIMARY KEY, state text, \
             latest_version text, write_date timestamp DEFAULT now()); \
         INSERT INTO ir_module_module (name, state, latest_version) \
             VALUES ('base', 'installed', '19.0.1.3'); \
         CREATE TABLE ir_config_parameter (key text PRIMARY KEY, value text); \
         CREATE TABLE res_partner (id serial PRIMARY KEY, name text); \
         INSERT INTO res_partner (name) VALUES ('Azure'), ('Deco')",
    );
}

fn zip_with(path: &Path, entries: &[(&str, &[u8])]) {
    let mut w = zip::ZipWriter::new(std::fs::File::create(path).expect("create"));
    for (name, data) in entries {
        w.start_file(*name, zip::write::SimpleFileOptions::default())
            .expect("start");
        w.write_all(data).expect("write");
    }
    w.finish().expect("finish");
}

async fn upload_file(api: &common::Api, query: &str, file: &Path) -> (StatusCode, Value) {
    let bytes = std::fs::read(file).expect("read");
    let n = bytes.len() as u64;
    api.upload(&format!("{RESTORE}?{query}"), Body::from(bytes), Some(n))
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn an_uploaded_dump_is_restored_and_listed() {
    let Some(pg) = TempPg::start() else {
        eprintln!("skipped: no PostgreSQL server binaries");
        return;
    };
    let env = Env::new();
    let api = env.app();
    let source = api.add_source(&env.source_dir()).await;
    api.create_project("shop", &source).await;
    point_at(&env, &api, &pg).await;
    odoo_like_db(&pg, "prod");
    let work = tempfile::tempdir().expect("tempdir");
    let dump = work.path().join("prod.dump");
    pg.dump("prod", "c", &dump);

    let (status, job) = upload_file(&api, "name=restored", &dump).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    assert_eq!(job["kind"], "restore_database");
    assert_eq!(job["project"], "shop");
    assert_eq!(job["database"], "restored");
    let done = api.wait_job(job["id"].as_u64().expect("id")).await;
    assert_eq!(done["state"], "succeeded", "{done}");
    let db = &done["result"]["database"];
    assert_eq!(db["name"], "restored");
    assert_eq!(db["format"], "custom");
    assert_eq!(db["neutralized"], true);
    assert_eq!(db["new_uuid"], true);
    assert!(
        done["steps"]
            .as_array()
            .expect("steps")
            .iter()
            .any(|s| s["job"] == "restore_database" && s["step"] == "neutralize"),
        "{done}"
    );
    assert_eq!(
        pg.psql(
            "restored",
            "SELECT value FROM ir_config_parameter WHERE key = 'database.is_neutralized'"
        ),
        ["true"]
    );
    assert!(leftovers(&env).is_empty(), "{:?}", leftovers(&env));

    let (status, list) = api.get("/api/v1/projects/shop/databases").await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let listed = list["databases"]
        .as_array()
        .expect("databases")
        .iter()
        .find(|d| d["name"] == "restored")
        .cloned()
        .unwrap_or_else(|| panic!("not listed: {list}"));
    assert_eq!(listed["odoo_series"], "19.0");

    // Not neutralized, kept UUID, when asked.
    let (status, job) = upload_file(&api, "name=moved&neutralize=false&copy=false", &dump).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    let done = api.wait_job(job["id"].as_u64().expect("id")).await;
    assert_eq!(done["state"], "succeeded", "{done}");
    assert_eq!(done["result"]["database"]["neutralized"], false);
    assert!(
        pg.psql(
            "moved",
            "SELECT value FROM ir_config_parameter WHERE key = 'database.is_neutralized'"
        )
        .is_empty()
    );

    // Taken name.
    let (status, body) = upload_file(&api, "name=restored", &dump).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "database_exists");

    // Unsafe and unusable files are refused before a job starts.
    let evil = work.path().join("evil.zip");
    for (entry, code) in [
        ("../../evil", "unsafe_archive"),
        ("filestore/../../../evil", "unsafe_archive"),
        ("/tmp/evil", "unsafe_archive"),
    ] {
        zip_with(&evil, &[("dump.sql", b"SELECT 1;\n"), (entry, b"pwned")]);
        let (status, body) = upload_file(&api, "name=evil", &evil).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{entry}: {body}");
        assert_eq!(body["error"], code, "{body}");
    }
    let gz = work.path().join("prod.sql.gz");
    std::fs::write(&gz, b"\x1f\x8b\x08\x00rest").expect("write");
    let (status, body) = upload_file(&api, "name=evil", &gz).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "invalid_dump");
    assert!(body["message"].as_str().is_some_and(|m| m.contains("gzip")));
    let (status, body) = api
        .upload(&format!("{RESTORE}?name=evil"), Body::empty(), Some(0))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "invalid_dump");
    let (_, jobs) = api.get("/api/v1/jobs").await;
    assert_eq!(jobs.as_array().map(Vec::len), Some(2), "{jobs}");
    assert!(
        pg.psql(
            "postgres",
            "SELECT datname FROM pg_database WHERE datname = 'evil'"
        )
        .is_empty()
    );
    assert!(leftovers(&env).is_empty(), "{:?}", leftovers(&env));
}

#[tokio::test(flavor = "multi_thread")]
async fn uploads_without_a_length_are_cut_at_the_limit_and_stalls_time_out() {
    let Some(pg) = TempPg::start() else {
        eprintln!("skipped: no PostgreSQL server binaries");
        return;
    };
    let env = Env::new();
    let mut api = env.app();
    let source = api.add_source(&env.source_dir()).await;
    api.create_project("shop", &source).await;
    point_at(&env, &api, &pg).await;
    let limits = RequestLimits {
        upload: UploadLimits {
            max_bytes: 1000,
            idle: Duration::from_millis(300),
        },
        ..RequestLimits::default()
    };
    api.router = app(
        AppConfig {
            limits,
            ..AppConfig::new(common::PORT)
        },
        api.service.clone(),
    );

    // Declared over the limit.
    let (status, _) = api
        .upload(
            &format!("{RESTORE}?name=big"),
            Body::from(vec![b'x'; 2000]),
            Some(2000),
        )
        .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    // No length (chunked): cut while streaming.
    let (status, body) = api
        .upload(
            &format!("{RESTORE}?name=big"),
            Body::from(vec![b'x'; 2000]),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    // Under the limit is accepted (and fails later only for not being Odoo).
    let (status, body) = api
        .upload(
            &format!("{RESTORE}?name=small"),
            Body::from("SELECT 1;\n"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let done = api.wait_job(body["id"].as_u64().expect("id")).await;
    assert_eq!(done["state"], "failed", "{done}");
    assert!(
        done["error"].to_string().contains("not an Odoo database"),
        "{done}"
    );

    let (status, body) = api
        .upload(
            &format!("{RESTORE}?name=stalled"),
            Body::from_stream(futures_util::stream::pending::<
                Result<axum::body::Bytes, std::io::Error>,
            >()),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::REQUEST_TIMEOUT, "{body}");
    assert_eq!(body["error"], "upload_timeout");
    assert!(leftovers(&env).is_empty(), "{:?}", leftovers(&env));
}

async fn delete_db(api: &common::Api, db: &str, body: Value) -> (StatusCode, Value) {
    api.call(
        axum::http::Method::DELETE,
        &format!("/api/v1/projects/shop/databases/{db}"),
        Some(body),
    )
    .await
}

#[tokio::test]
async fn database_deletion_is_refused_before_anything_runs() {
    let env = Env::new();
    let api = env.app();
    let source = api.add_source(&env.source_dir()).await;
    api.create_project("shop", &source).await;

    // Names that could break out of an argument or a query (URL-encoded).
    for name in [
        "x%3BDROP%20DATABASE%20postgres",
        "x'y",
        "two%20words",
        "-x",
        "x%22y",
    ] {
        let (status, body) = delete_db(&api, name, json!({ "confirm": name })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{name:?}: {body}");
        assert_eq!(body["error"], "invalid_name", "{name:?}: {body}");
    }
    // The name typed again, exactly.
    for typed in ["", "shop", "Shop_main", " shop_main", "shop_main "] {
        let (status, body) = delete_db(&api, "shop_main", json!({ "confirm": typed })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{typed:?}: {body}");
        assert_eq!(body["error"], "not_confirmed", "{body}");
    }
    let (status, _) = delete_db(&api, "shop_main", json!({})).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "confirm is required"
    );
    let (status, _) = delete_db(&api, "shop_main", json!({ "confirm": "shop_main", "x": 1 })).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "unknown fields are refused"
    );

    let (status, body) = delete_db(&api, "shop_main", json!({ "confirm": "shop_main" })).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "no_venv");

    let (status, body) = api
        .call(
            axum::http::Method::DELETE,
            "/api/v1/projects/nope/databases/shop_main",
            Some(json!({ "confirm": "shop_main" })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"], "unknown_project", "{body}");

    // A server that cannot be used is reported before a job starts.
    env.make_runnable("shop", 18_369, 18_372);
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .expect("bind")
        .port();
    let (status, body) = api
        .put(
            "/api/v1/projects/shop/options",
            json!({ "options": { "db_host": "127.0.0.1", "db_port": port.to_string() } }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = delete_db(&api, "shop_main", json!({ "confirm": "shop_main" })).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"], "postgres_unavailable");
    let (_, jobs) = api.get("/api/v1/jobs").await;
    assert_eq!(jobs, json!([]), "no job was started");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_listed_database_is_deleted_with_its_filestore() {
    let Some(pg) = TempPg::start() else {
        eprintln!("skipped: no PostgreSQL server binaries");
        return;
    };
    let env = Env::new();
    let api = env.app();
    let source = api.add_source(&env.source_dir()).await;
    api.create_project("shop", &source).await;
    point_at(&env, &api, &pg).await;
    odoo_like_db(&pg, "old");
    let filestore = env.project_dir("shop").join("data/filestore/old");
    std::fs::create_dir_all(filestore.join("ab")).expect("mkdir");
    std::fs::write(filestore.join("ab/cafe"), "x").expect("write");

    // Only what the project lists: not `postgres`, not a missing database,
    // not one its dbfilter hides.
    for db in ["postgres", "template1", "missing"] {
        let (status, body) = delete_db(&api, db, json!({ "confirm": db })).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{db}: {body}");
        assert_eq!(body["error"], "database_not_found", "{body}");
    }
    let (status, body) = api
        .put(
            "/api/v1/projects/shop/options",
            json!({ "options": { "dbfilter": "^shop_.*$" } }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = delete_db(&api, "old", json!({ "confirm": "old" })).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "hidden by the filter: {body}"
    );
    let (status, body) = api
        .put(
            "/api/v1/projects/shop/options",
            json!({ "options": { "dbfilter": "" } }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, job) = delete_db(&api, "old", json!({ "confirm": "old" })).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    assert_eq!(job["kind"], "delete_database");
    assert_eq!(job["project"], "shop");
    assert_eq!(job["database"], "old");
    let done = api.wait_job(job["id"].as_u64().expect("id")).await;
    assert_eq!(done["state"], "succeeded", "{done}");
    assert_eq!(
        done["result"]["database"],
        json!({ "name": "old", "filestore_removed": true })
    );
    assert!(
        done["steps"]
            .as_array()
            .expect("steps")
            .iter()
            .any(|s| s["job"] == "delete_database" && s["step"] == "drop"),
        "{done}"
    );
    assert!(
        pg.psql(
            "postgres",
            "SELECT datname FROM pg_database WHERE datname = 'old'"
        )
        .is_empty()
    );
    assert!(filestore.symlink_metadata().is_err());
    let (_, list) = api.get("/api/v1/projects/shop/databases").await;
    assert!(
        !list["databases"]
            .as_array()
            .expect("databases")
            .iter()
            .any(|d| d["name"] == "old"),
        "{list}"
    );
    let (status, body) = delete_db(&api, "old", json!({ "confirm": "old" })).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "deleted already: {body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_database_an_instance_runs_on_is_not_deleted() {
    let Some(pg) = TempPg::start() else {
        eprintln!("skipped: no PostgreSQL server binaries");
        return;
    };
    let env = Env::new();
    let api = env.app();
    let source = api.add_source(&env.source_dir()).await;
    api.create_project("shop", &source).await;
    point_at(&env, &api, &pg).await;
    // Instance `main` uses database `shop_main`.
    odoo_like_db(&pg, "shop_main");
    let (status, body) = api
        .post("/api/v1/projects/shop/instances/main/start", None)
        .await;
    assert!(status.is_success(), "{body}");

    let (status, body) = delete_db(&api, "shop_main", json!({ "confirm": "shop_main" })).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "database_in_use", "{body}");
    assert!(
        body["message"].as_str().is_some_and(|m| m.contains("main")),
        "{body}"
    );
    assert_eq!(
        pg.psql(
            "postgres",
            "SELECT datname FROM pg_database WHERE datname = 'shop_main'"
        ),
        ["shop_main"]
    );

    let (status, body) = api
        .post("/api/v1/projects/shop/instances/main/stop", None)
        .await;
    assert!(status.is_success(), "{body}");
    let (status, job) = delete_db(&api, "shop_main", json!({ "confirm": "shop_main" })).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    let done = api.wait_job(job["id"].as_u64().expect("id")).await;
    assert_eq!(done["state"], "succeeded", "{done}");
    assert_eq!(done["result"]["database"]["filestore_removed"], false);
}

async fn duplicate_db(api: &common::Api, db: &str, body: Value) -> (StatusCode, Value) {
    api.post(
        &format!("/api/v1/projects/shop/databases/{db}/duplicate"),
        Some(body),
    )
    .await
}

#[tokio::test]
async fn database_copies_are_refused_before_anything_runs() {
    let env = Env::new();
    let api = env.app();
    let source = api.add_source(&env.source_dir()).await;
    api.create_project("shop", &source).await;

    // Names that could break out of an argument or a query.
    for name in [
        "x;DROP DATABASE postgres",
        "x'y",
        "two words",
        "-x",
        "x\"y",
        "_x",
        "x",
    ] {
        let (status, body) = duplicate_db(&api, "shop_main", json!({ "name": name })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{name:?}: {body}");
        assert!(
            ["invalid_name", "invalid_request"].contains(&body["error"].as_str().unwrap_or("")),
            "{name:?}: {body}"
        );
    }
    for source in ["x%3BDROP%20DATABASE%20postgres", "x'y", "-x"] {
        let (status, body) = duplicate_db(&api, source, json!({ "name": "copy" })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{source:?}: {body}");
        assert_eq!(body["error"], "invalid_name", "{source:?}: {body}");
    }
    let (status, body) = duplicate_db(&api, "shop_main", json!({ "name": "shop_main" })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "invalid_request", "{body}");
    let (status, _) = duplicate_db(&api, "shop_main", json!({})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "name is required");
    let (status, _) = duplicate_db(&api, "shop_main", json!({ "name": "copy", "x": 1 })).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "unknown fields are refused"
    );

    let (status, body) = duplicate_db(&api, "shop_main", json!({ "name": "copy" })).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "no_venv");
    let (status, body) = api
        .post(
            "/api/v1/projects/nope/databases/shop_main/duplicate",
            Some(json!({ "name": "copy" })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"], "unknown_project", "{body}");

    // A server that cannot be used is reported before a job starts.
    env.make_runnable("shop", 18_469, 18_472);
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .expect("bind")
        .port();
    let (status, body) = api
        .put(
            "/api/v1/projects/shop/options",
            json!({ "options": { "db_host": "127.0.0.1", "db_port": port.to_string() } }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = duplicate_db(&api, "shop_main", json!({ "name": "copy" })).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"], "postgres_unavailable");
    let (_, jobs) = api.get("/api/v1/jobs").await;
    assert_eq!(jobs, json!([]), "no job was started");
}

fn table_counts(pg: &TempPg, db: &str) -> Vec<String> {
    pg.psql(
        db,
        "SELECT t || '=' || n FROM ( \
             SELECT 'ir_module_module' t, count(*) n FROM ir_module_module UNION ALL \
             SELECT 'res_partner', count(*) FROM res_partner UNION ALL \
             SELECT 'tables', count(*) FROM pg_tables WHERE schemaname = 'public') c \
         ORDER BY t",
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_listed_database_is_copied_with_its_filestore() {
    let Some(pg) = TempPg::start() else {
        eprintln!("skipped: no PostgreSQL server binaries");
        return;
    };
    let env = Env::new();
    let api = env.app();
    let source = api.add_source(&env.source_dir()).await;
    api.create_project("shop", &source).await;
    point_at(&env, &api, &pg).await;
    odoo_like_db(&pg, "prod");
    pg.psql(
        "prod",
        "INSERT INTO ir_config_parameter VALUES ('database.uuid', 'prod-uuid')",
    );
    let data = env.project_dir("shop").join("data/filestore");
    std::fs::create_dir_all(data.join("prod/ab")).expect("mkdir");
    std::fs::write(data.join("prod/ab/cafe"), "attachment").expect("write");

    for db in ["postgres", "template1", "missing"] {
        let (status, body) = duplicate_db(&api, db, json!({ "name": "copy" })).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{db}: {body}");
        assert_eq!(body["error"], "database_not_found", "{body}");
    }
    odoo_like_db(&pg, "taken");
    let (status, body) = duplicate_db(&api, "prod", json!({ "name": "taken" })).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "database_exists", "{body}");
    std::fs::create_dir_all(data.join("leftover")).expect("mkdir");
    let (status, body) = duplicate_db(&api, "prod", json!({ "name": "leftover" })).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "filestore_exists", "{body}");

    let (status, job) = duplicate_db(
        &api,
        "prod",
        json!({ "name": "prod_test", "neutralize": true }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    assert_eq!(job["kind"], "duplicate_database");
    assert_eq!(job["project"], "shop");
    assert_eq!(job["database"], "prod_test");
    assert_eq!(job["source"], "prod");
    let done = api.wait_job(job["id"].as_u64().expect("id")).await;
    assert_eq!(done["state"], "succeeded", "{done}");
    assert_eq!(
        done["result"],
        json!({
            "kind": "duplicate_database",
            "database": {
                "name": "prod_test",
                "source": "prod",
                "neutralized": true,
                "filestore_copied": true,
            },
            "stopped": [],
            "restarted": [],
        })
    );
    let steps: Vec<&str> = done["steps"]
        .as_array()
        .expect("steps")
        .iter()
        .filter(|s| s["job"] == "duplicate_database" && s["event"] == "step")
        .filter_map(|s| s["step"].as_str())
        .collect();
    assert_eq!(steps, ["copy", "check"], "{done}");

    // Identical tables and rows; a new UUID; the source untouched.
    assert_eq!(table_counts(&pg, "prod_test"), table_counts(&pg, "prod"));
    assert_eq!(table_counts(&pg, "prod")[1], "res_partner=2");
    let uuid = |db| {
        pg.psql(
            db,
            "SELECT value FROM ir_config_parameter WHERE key = 'database.uuid'",
        )
    };
    assert_eq!(uuid("prod"), ["prod-uuid"]);
    assert_eq!(uuid("prod_test").len(), 1);
    assert_ne!(uuid("prod_test"), ["prod-uuid"]);
    assert!(
        pg.psql(
            "prod",
            "SELECT value FROM ir_config_parameter WHERE key = 'database.is_neutralized'"
        )
        .is_empty()
    );
    assert_eq!(
        std::fs::read_to_string(data.join("prod_test/ab/cafe")).expect("copied"),
        "attachment"
    );
    let (_, list) = api.get("/api/v1/projects/shop/databases").await;
    assert!(
        list["databases"]
            .as_array()
            .expect("databases")
            .iter()
            .any(|d| d["name"] == "prod_test"),
        "{list}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn instances_on_the_source_are_stopped_only_when_asked() {
    let Some(pg) = TempPg::start() else {
        eprintln!("skipped: no PostgreSQL server binaries");
        return;
    };
    let env = Env::new();
    let api = env.app();
    let source = api.add_source(&env.source_dir()).await;
    api.create_project("shop", &source).await;
    point_at(&env, &api, &pg).await;
    // Instance `main` uses database `shop_main`.
    odoo_like_db(&pg, "shop_main");
    let (status, body) = api
        .post("/api/v1/projects/shop/instances/main/start", None)
        .await;
    assert!(status.is_success(), "{body}");

    // The warning: which instances run on it.
    let (status, body) = duplicate_db(&api, "shop_main", json!({ "name": "shop_copy" })).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "database_in_use", "{body}");
    assert!(
        body["message"].as_str().is_some_and(|m| m.contains("main")),
        "{body}"
    );
    let (_, jobs) = api.get("/api/v1/jobs").await;
    assert_eq!(jobs, json!([]), "no job was started");

    let (status, job) = duplicate_db(
        &api,
        "shop_main",
        json!({ "name": "shop_copy", "stop_instances": true }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    let done = api.wait_job(job["id"].as_u64().expect("id")).await;
    assert_eq!(done["state"], "succeeded", "{done}");
    assert_eq!(done["result"]["stopped"], json!(["main"]), "{done}");
    assert_eq!(done["result"]["restarted"], json!(["main"]), "{done}");
    assert_eq!(done["result"]["database"]["neutralized"], false);
    let steps: Vec<&str> = done["steps"]
        .as_array()
        .expect("steps")
        .iter()
        .filter(|s| s["event"] == "step")
        .filter_map(|s| s["step"].as_str())
        .collect();
    assert_eq!(
        steps,
        ["stop_instances", "copy", "check", "restart_instances"],
        "{done}"
    );
    assert_eq!(
        table_counts(&pg, "shop_copy"),
        table_counts(&pg, "shop_main")
    );
    // `main` runs again.
    let (status, body) = api
        .post("/api/v1/projects/shop/instances/main/start", None)
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "already_running", "{body}");
    let (status, body) = api
        .post("/api/v1/projects/shop/instances/main/stop", None)
        .await;
    assert!(status.is_success(), "{body}");
}

async fn backup_db(api: &common::Api, db: &str, body: Value) -> (StatusCode, Value) {
    api.post(
        &format!("/api/v1/projects/shop/databases/{db}/backup"),
        Some(body),
    )
    .await
}

#[tokio::test]
async fn backups_are_refused_before_anything_runs() {
    let env = Env::new();
    let api = env.app();
    let source = api.add_source(&env.source_dir()).await;
    api.create_project("shop", &source).await;

    for name in ["x%3BDROP%20DATABASE%20postgres", "x'y", "two%20words", "-x"] {
        let (status, body) = backup_db(&api, name, json!({})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{name:?}: {body}");
        assert_eq!(body["error"], "invalid_name", "{name:?}: {body}");
    }
    for bad in [json!({ "format": "sql" }), json!({ "x": 1 })] {
        let (status, body) = backup_db(&api, "shop_main", bad.clone()).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}: {body}");
    }
    let (status, body) = backup_db(&api, "shop_main", json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "no_venv");
    let (status, body) = api
        .post(
            "/api/v1/projects/nope/databases/shop_main/backup",
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"], "unknown_project");

    // Backup files: names only, never paths or other project files.
    let (status, list) = api.get("/api/v1/projects/shop/backups").await;
    assert_eq!((status, list), (StatusCode::OK, json!([])));
    for file in [
        "..%2Fodhive.toml",
        "%2E%2E%2Fodhive.toml",
        "odhive.toml",
        ".hidden.zip",
        "a%2Fb.zip",
    ] {
        for method in [axum::http::Method::GET, axum::http::Method::DELETE] {
            let (status, body) = api
                .call(
                    method.clone(),
                    &format!("/api/v1/projects/shop/backups/{file}"),
                    None,
                )
                .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{method} {file}: {body}");
            assert_eq!(body["error"], "invalid_backup_name", "{body}");
        }
    }
    let (status, body) = api.get("/api/v1/projects/shop/backups/none.zip").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"], "backup_not_found");
    let (status, body) = api.get("/api/v1/projects/nope/backups").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (_, jobs) = api.get("/api/v1/jobs").await;
    assert_eq!(jobs, json!([]), "no job was started");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_listed_database_is_backed_up_downloaded_and_restored() {
    let Some(pg) = TempPg::start() else {
        eprintln!("skipped: no PostgreSQL server binaries");
        return;
    };
    let env = Env::new();
    let api = env.app();
    let source = api.add_source(&env.source_dir()).await;
    api.create_project("shop", &source).await;
    point_at(&env, &api, &pg).await;
    odoo_like_db(&pg, "shop_main");
    let dir = env.project_dir("shop");
    let filestore = dir.join("data/filestore/shop_main");
    std::fs::create_dir_all(filestore.join("ab")).expect("mkdir");
    std::fs::write(filestore.join("ab/cafe"), "attachment").expect("write");

    for db in ["postgres", "missing"] {
        let (status, body) = backup_db(&api, db, json!({})).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{db}: {body}");
        assert_eq!(body["error"], "database_not_found", "{body}");
    }

    let mut files = Vec::new();
    for format in ["zip", "dump"] {
        let (status, job) = backup_db(&api, "shop_main", json!({ "format": format })).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{job}");
        assert_eq!(job["kind"], "backup_database");
        assert_eq!(job["database"], "shop_main");
        let done = api.wait_job(job["id"].as_u64().expect("id")).await;
        assert_eq!(done["state"], "succeeded", "{done}");
        let b = &done["result"]["backup"];
        assert_eq!(b["format"], format);
        assert_eq!(b["database"], "shop_main");
        assert!(
            done["steps"]
                .as_array()
                .expect("steps")
                .iter()
                .any(|s| s["job"] == "backup_database" && s["step"] == "check"),
            "{done}"
        );
        files.push(b["file"].as_str().expect("file").to_owned());
        // Same-second names would clash; real backups are seconds apart.
        tokio::time::sleep(Duration::from_millis(1100)).await;
    }
    let (status, list) = api.get("/api/v1/projects/shop/backups").await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let listed: Vec<&str> = list
        .as_array()
        .expect("list")
        .iter()
        .map(|b| b["file"].as_str().expect("file"))
        .collect();
    assert_eq!(
        listed,
        [files[1].as_str(), files[0].as_str()],
        "newest first"
    );
    // Nothing but the two files in the folder.
    let mut on_disk: Vec<String> = std::fs::read_dir(dir.join("backups"))
        .expect("backups")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    on_disk.sort();
    let mut want = files.clone();
    want.sort();
    assert_eq!(on_disk, want);

    // Download: the file's bytes, as an attachment.
    let mut downloaded = Vec::new();
    for file in &files {
        let resp = api
            .respond(
                axum::http::Method::GET,
                &format!("/api/v1/projects/shop/backups/{file}"),
                None,
            )
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let h = resp.headers().clone();
        assert_eq!(
            h["content-disposition"],
            format!("attachment; filename=\"{file}\"").as_str()
        );
        assert_eq!(h["cache-control"], "no-store");
        let bytes = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .expect("body")
            .to_bytes();
        assert_eq!(h["content-length"], bytes.len().to_string().as_str());
        assert_eq!(
            bytes.as_ref(),
            std::fs::read(dir.join("backups").join(file))
                .expect("read")
                .as_slice()
        );
        downloaded.push(bytes);
    }
    let zip = zip::ZipArchive::new(std::io::Cursor::new(downloaded[0].clone())).expect("zip");
    let mut names: Vec<String> = zip
        .file_names()
        .map(|n| n.expect("name").into_owned())
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["dump.sql", "filestore/ab/cafe", "manifest.json"]);

    // A link in backups/ is neither listed nor served.
    std::os::unix::fs::symlink(dir.join("odhive.toml"), dir.join("backups/link.zip"))
        .expect("symlink");
    let (_, list) = api.get("/api/v1/projects/shop/backups").await;
    assert_eq!(list.as_array().expect("list").len(), 2, "{list}");
    let (status, _) = api.get("/api/v1/projects/shop/backups/link.zip").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Drop the database, then restore the downloaded dump.
    let (status, job) = delete_db(&api, "shop_main", json!({ "confirm": "shop_main" })).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    let done = api.wait_job(job["id"].as_u64().expect("id")).await;
    assert_eq!(done["state"], "succeeded", "{done}");
    let work = tempfile::tempdir().expect("tempdir");
    let dump = work.path().join("back.dump");
    std::fs::write(&dump, &downloaded[1]).expect("write");
    let (status, job) = upload_file(&api, "name=shop_main", &dump).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    let done = api.wait_job(job["id"].as_u64().expect("id")).await;
    assert_eq!(done["state"], "succeeded", "{done}");
    assert_eq!(
        pg.psql(
            "shop_main",
            "SELECT value FROM ir_config_parameter WHERE key = 'database.is_neutralized'"
        ),
        ["true"]
    );

    // Delete a backup.
    let (status, body) = api
        .delete(&format!("/api/v1/projects/shop/backups/{}", files[0]))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert!(!dir.join("backups").join(&files[0]).exists());
    let (status, _) = api
        .delete(&format!("/api/v1/projects/shop/backups/{}", files[0]))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = api.delete("/api/v1/projects/shop/backups/link.zip").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "links are not deleted");
    assert!(dir.join("odhive.toml").is_file());
}

async fn snapshot_db(api: &common::Api, db: &str, body: Value) -> (StatusCode, Value) {
    api.post(
        &format!("/api/v1/projects/shop/databases/{db}/snapshot"),
        Some(body),
    )
    .await
}

async fn restore_snapshot(api: &common::Api, snap: &str, body: Value) -> (StatusCode, Value) {
    api.post(
        &format!("/api/v1/projects/shop/snapshots/{snap}/restore"),
        Some(body),
    )
    .await
}

fn step_names(job: &Value) -> Vec<String> {
    job["steps"]
        .as_array()
        .expect("steps")
        .iter()
        .filter(|s| s["event"] == "step")
        .filter_map(|s| s["step"].as_str().map(str::to_owned))
        .collect()
}

#[tokio::test]
async fn snapshots_are_refused_before_anything_runs() {
    let env = Env::new();
    let api = env.app();
    let source = api.add_source(&env.source_dir()).await;
    api.create_project("shop", &source).await;

    for name in ["x%3BDROP%20DATABASE%20postgres", "x'y", "two%20words", "-x"] {
        let (status, body) = snapshot_db(&api, name, json!({})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{name:?}: {body}");
        assert_eq!(body["error"], "invalid_name", "{name:?}: {body}");
    }
    let long = "d".repeat(43);
    let (status, body) = snapshot_db(&api, &long, json!({})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "name_too_long", "{body}");
    let (status, body) = snapshot_db(&api, "shop_main", json!({ "x": 1 })).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, body) = api
        .post(
            "/api/v1/projects/nope/databases/shop_main/snapshot",
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"], "unknown_project");

    // Snapshot names only, never paths or other databases.
    for snap in [
        "shop_main",
        "..%2Fshop_main__snap_20261126090503",
        "shop_main__snap_2026",
        "x%3BDROP__snap_20261126090503",
        "shop_main__snap_20260231000000",
    ] {
        let (status, body) = restore_snapshot(&api, snap, json!({})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{snap}: {body}");
        assert_eq!(body["error"], "invalid_snapshot_name", "{snap}: {body}");
        let (status, body) = api
            .call(
                axum::http::Method::DELETE,
                &format!("/api/v1/projects/shop/snapshots/{snap}"),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{snap}: {body}");
        assert_eq!(body["error"], "invalid_snapshot_name", "{snap}: {body}");
    }
    // An unknown snapshot: the project has no folder for it.
    let (status, body) = api
        .call(
            axum::http::Method::DELETE,
            "/api/v1/projects/shop/snapshots/shop_main__snap_20261126090503",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"], "snapshot_not_found", "{body}");
    let (_, jobs) = api.get("/api/v1/jobs").await;
    assert_eq!(jobs, json!([]), "no job was started");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_snapshot_is_taken_restored_and_undoes_a_delete() {
    let Some(pg) = TempPg::start() else {
        eprintln!("skipped: no PostgreSQL server binaries");
        return;
    };
    let env = Env::new();
    let api = env.app();
    let source = api.add_source(&env.source_dir()).await;
    api.create_project("shop", &source).await;
    point_at(&env, &api, &pg).await;
    odoo_like_db(&pg, "shop_main");
    let dir = env.project_dir("shop");
    let filestore = dir.join("data/filestore/shop_main");
    std::fs::create_dir_all(filestore.join("ab")).expect("mkdir");
    std::fs::write(filestore.join("ab/cafe"), "attachment").expect("write");

    for db in ["postgres", "missing"] {
        let (status, body) = snapshot_db(&api, db, json!({})).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{db}: {body}");
        assert_eq!(body["error"], "database_not_found", "{body}");
    }
    let (status, list) = api.get("/api/v1/projects/shop/snapshots").await;
    assert_eq!((status, list), (StatusCode::OK, json!([])));

    // Instance `main` runs on `shop_main`: refused unless asked to stop it.
    let (status, body) = api
        .post("/api/v1/projects/shop/instances/main/start", None)
        .await;
    assert!(status.is_success(), "{body}");
    let (status, body) = snapshot_db(&api, "shop_main", json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "database_in_use", "{body}");
    let (status, job) = snapshot_db(&api, "shop_main", json!({ "stop_instances": true })).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    assert_eq!(job["kind"], "snapshot_database");
    assert_eq!(job["database"], "shop_main");
    let done = api.wait_job(job["id"].as_u64().expect("id")).await;
    assert_eq!(done["state"], "succeeded", "{done}");
    assert_eq!(
        step_names(&done),
        ["stop_instances", "copy", "filestore", "restart_instances"]
    );
    assert_eq!(done["result"]["stopped"], json!(["main"]), "{done}");
    assert_eq!(done["result"]["restarted"], json!(["main"]), "{done}");
    let snap = done["result"]["snapshot"].clone();
    let name = snap["name"].as_str().expect("name").to_owned();
    assert!(name.starts_with("shop_main__snap_"), "{snap}");
    assert_eq!(snap["filestore"], true);
    let (status, body) = api
        .post("/api/v1/projects/shop/instances/main/stop", None)
        .await;
    assert!(status.is_success(), "{body}");

    // Listed as a snapshot, never as a database.
    let (_, list) = api.get("/api/v1/projects/shop/snapshots").await;
    assert_eq!(list, json!([snap]));
    let (_, dbs) = api.get("/api/v1/projects/shop/databases").await;
    let names: Vec<&str> = dbs["databases"]
        .as_array()
        .expect("databases")
        .iter()
        .filter_map(|d| d["name"].as_str())
        .collect();
    assert_eq!(names, ["shop_main"], "{dbs}");

    // Changes since the snapshot are undone.
    pg.psql("shop_main", "DELETE FROM res_partner");
    std::fs::remove_file(filestore.join("ab/cafe")).expect("unlink");
    let (status, job) = restore_snapshot(&api, &name, json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    assert_eq!(job["kind"], "restore_snapshot");
    assert_eq!(job["snapshot"], name.as_str());
    let done = api.wait_job(job["id"].as_u64().expect("id")).await;
    assert_eq!(done["state"], "succeeded", "{done}");
    assert_eq!(step_names(&done), ["copy", "filestore", "swap"]);
    assert_eq!(done["result"]["database"]["database"], "shop_main");
    assert_eq!(
        pg.psql("shop_main", "SELECT count(*) FROM res_partner"),
        ["2"]
    );
    assert!(filestore.join("ab/cafe").is_file());

    // Delete with a snapshot first, then undo it.
    let (status, job) = delete_db(
        &api,
        "shop_main",
        json!({ "confirm": "shop_main", "snapshot": true }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    let done = api.wait_job(job["id"].as_u64().expect("id")).await;
    assert_eq!(done["state"], "succeeded", "{done}");
    assert_eq!(step_names(&done), ["snapshot", "drop", "check"]);
    let before_delete = done["result"]["snapshot"]["name"]
        .as_str()
        .expect("snapshot")
        .to_owned();
    assert!(
        pg.psql(
            "postgres",
            "SELECT datname FROM pg_database WHERE datname = 'shop_main'"
        )
        .is_empty()
    );
    let (status, job) = restore_snapshot(&api, &before_delete, json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    let done = api.wait_job(job["id"].as_u64().expect("id")).await;
    assert_eq!(done["state"], "succeeded", "{done}");
    assert_eq!(
        pg.psql("shop_main", "SELECT count(*) FROM res_partner"),
        ["2"]
    );
    assert!(filestore.join("ab/cafe").is_file());

    // Deleting snapshots leaves the database.
    for snap in [&name, &before_delete] {
        let (status, body) = api
            .call(
                axum::http::Method::DELETE,
                &format!("/api/v1/projects/shop/snapshots/{snap}"),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    }
    let (_, list) = api.get("/api/v1/projects/shop/snapshots").await;
    assert_eq!(list, json!([]));
    let (status, body) = restore_snapshot(&api, &name, json!({})).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"], "snapshot_not_found");
    assert_eq!(
        pg.psql("shop_main", "SELECT count(*) FROM res_partner"),
        ["2"]
    );
}
