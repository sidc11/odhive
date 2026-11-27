//! `/projects/{name}/postgres/log`: finding, choosing and following the
//! PostgreSQL server log.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;

use axum::http::StatusCode;
use odhive_core::manager::ManagerOptions;
use odhive_core::pglog::SearchDir;
use odhive_server::service::ServiceOptions;
use serde_json::{Value, json};

mod common;
use common::{Api, Env, PORT};

const URL: &str = "/api/v1/projects/shop/postgres/log";

async fn app(env: &Env, logs: &Path) -> Api {
    let opts = ServiceOptions {
        pg_log_dirs: vec![SearchDir {
            dir: logs.to_path_buf(),
            prefix: "postgresql",
        }],
        ..ServiceOptions::default()
    };
    let api = env.app_with(ManagerOptions::default(), opts, PORT);
    let source = api.add_source(&env.source_dir()).await;
    api.create_project("shop", &source).await;
    api
}

fn messages(body: &Value) -> Vec<String> {
    body["records"]
        .as_array()
        .expect("records")
        .iter()
        .map(|r| r["message"].as_str().unwrap_or("").to_owned())
        .collect()
}

#[tokio::test]
async fn finds_the_log_then_follows_it_from_next() {
    let env = Env::new();
    let logs = tempfile::tempdir().expect("tempdir");
    let api = app(&env, logs.path()).await;

    let (status, body) = api.get(URL).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "not_found");
    assert_eq!(body["path"], Value::Null);
    assert!(body["hint"].is_string(), "{body}");

    let path = logs.path().join("postgresql-16-main.log");
    let mut f = fs::File::create(&path).expect("create");
    writeln!(
        f,
        "2026-11-27 10:15:01.123 UTC [42] odoo@shop_main ERROR:  relation \"x\" does not exist"
    )
    .expect("write");
    writeln!(
        f,
        "2026-11-27 10:15:01.123 UTC [42] odoo@shop_main STATEMENT:  SELECT 1"
    )
    .expect("write");

    let (status, body) = api.get(URL).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "ok", "{body}");
    assert_eq!(body["path"], path.display().to_string());
    assert_eq!(body["configured"], Value::Null);
    assert_eq!(body["found"], json!([path.display().to_string()]));
    let rec = &body["records"][0];
    assert_eq!(rec["level"], "error");
    assert_eq!(rec["db"], "shop_main");
    assert_eq!(rec["logger"], "postgresql");
    assert_eq!(rec["timestamp"], "2026-11-27T10:15:01.123Z");
    assert_eq!(
        rec["message"],
        "relation \"x\" does not exist\nSTATEMENT:  SELECT 1"
    );
    let stream = body["stream"].clone();
    let next = body["next"].as_u64().expect("next");

    writeln!(
        f,
        "2026-11-27 10:15:02.000 UTC [43] LOG:  checkpoint starting"
    )
    .expect("write");
    // The first poll reads the line, the next one completes the entry.
    api.get(&format!("{URL}?from={next}")).await;
    let (_, body) = api.get(&format!("{URL}?from={next}")).await;
    assert_eq!(body["stream"], stream);
    assert_eq!(messages(&body), ["checkpoint starting"]);
}

#[tokio::test]
async fn a_chosen_file_is_saved_and_bad_paths_are_refused() {
    let env = Env::new();
    let logs = tempfile::tempdir().expect("tempdir");
    let api = app(&env, logs.path()).await;
    let base = fs::canonicalize(logs.path()).expect("canonical");
    let mine = base.join("mine.log");
    fs::write(&mine, "2026-01-01 00:00:00.000 UTC [1] LOG:  hello\n").expect("write");
    let secret = base.join("secret.txt");
    fs::write(&secret, "s3cret\n").expect("write");
    symlink(&secret, base.join("sneaky.log")).expect("symlink");
    fs::create_dir(base.join("dir.log")).expect("mkdir");
    let toml = env.project_dir("shop").join("odhive.toml");
    let before = fs::read_to_string(&toml).expect("read");

    for path in [
        "relative.log".to_owned(),
        format!("{}/../{}/mine.log", base.display(), base.display()),
        secret.display().to_string(),
        base.join("sneaky.log").display().to_string(),
        base.join("dir.log").display().to_string(),
        base.join("missing.log").display().to_string(),
    ] {
        let (status, body) = api.put(URL, json!({ "path": path })).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{path}: {body}");
        assert_eq!(body["error"], "bad_log_path", "{path}");
        assert!(!body.to_string().contains("s3cret"));
    }
    assert_eq!(fs::read_to_string(&toml).expect("read"), before);

    let (status, body) = api
        .put(URL, json!({ "path": mine.display().to_string() }))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "ok");
    assert_eq!(body["configured"], mine.display().to_string());
    assert_eq!(body["records"], json!([]));
    assert!(
        fs::read_to_string(&toml)
            .expect("read")
            .contains("postgres_log")
    );

    let (_, body) = api.get(URL).await;
    assert_eq!(messages(&body), ["hello"]);

    // A configured file that went away is reported, not an error.
    fs::remove_file(&mine).expect("rm");
    let (status, body) = api.get(URL).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "not_found");

    let (status, body) = api.put(URL, json!({ "path": null })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["configured"], Value::Null);
    assert!(
        !fs::read_to_string(&toml)
            .expect("read")
            .contains("postgres_log")
    );
}

#[tokio::test]
async fn an_unreadable_log_gets_a_hint() {
    if odhive_core::platform::effective_uid() == 0 {
        return;
    }
    let env = Env::new();
    let logs = tempfile::tempdir().expect("tempdir");
    let api = app(&env, logs.path()).await;
    let path = logs.path().join("postgresql-16-main.log");
    fs::write(&path, "x\n").expect("write");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).expect("chmod");

    let (status, body) = api.get(URL).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "permission_denied", "{body}");
    assert!(
        body["hint"]
            .as_str()
            .is_some_and(|h| h.contains("postgresql-16-main.log")),
        "{body}"
    );
    assert_eq!(body["records"], json!([]));
}

#[tokio::test]
async fn limit_is_checked_and_unknown_projects_are_404() {
    let env = Env::new();
    let logs = tempfile::tempdir().expect("tempdir");
    let api = app(&env, logs.path()).await;
    let (status, body) = api.get(&format!("{URL}?limit=5001")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, _) = api.get(&format!("{URL}?bogus=1")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = api.get("/api/v1/projects/nope/postgres/log").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
