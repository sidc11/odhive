//! Shared test harness: a private OdHive home with a registered-able fake
//! Odoo 19 source (its `odoo-bin` is the `fake-odoo-bin` fixture) and the
//! app router served in-process.

#![allow(dead_code)]

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use axum::response::Response;
use http_body_util::BodyExt;
use odhive_core::config::AppPaths;
use odhive_core::manager::{Manager, ManagerOptions};
use odhive_core::pidfile::RunDir;
use odhive_core::project::{Instance, ProjectConfig};
use odhive_core::python::find_executable;
use odhive_core::workspace::bootstrap;
use odhive_server::{AppConfig, Service, app};
use serde_json::Value;
use tower::ServiceExt;

/// The port the app believes it serves on.
pub const PORT: u16 = 7069;
pub const HOST: &str = "localhost:7069";

pub struct Env {
    _tmp: tempfile::TempDir,
    pub home: PathBuf,
    pub paths: AppPaths,
}

impl Env {
    /// A fresh home with a fake Odoo 19 tree at `<home>/odoo-src` (not
    /// registered yet).
    pub fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = fs::canonicalize(tmp.path()).expect("canonicalize");
        let src = home.join("odoo-src");
        fs::create_dir_all(src.join("odoo")).expect("mkdir");
        fs::create_dir_all(src.join("addons")).expect("mkdir");
        let fixture =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/fake-odoo-bin");
        fs::copy(fixture, src.join("odoo-bin")).expect("copy fixture");
        fs::set_permissions(src.join("odoo-bin"), fs::Permissions::from_mode(0o755))
            .expect("chmod");
        fs::write(
            src.join("odoo/release.py"),
            "version_info = (19, 0, 0, FINAL, 0, '')\n",
        )
        .expect("write");
        fs::write(src.join("requirements.txt"), "").expect("write");
        let paths = AppPaths {
            config_dir: home.join(".config/odhive"),
            data_dir: home.join(".local/share/odhive"),
            home_dir: home.clone(),
        };
        Self {
            _tmp: tmp,
            home,
            paths,
        }
    }

    pub fn source_dir(&self) -> PathBuf {
        self.home.join("odoo-src")
    }

    pub fn project_dir(&self, name: &str) -> PathBuf {
        self.home.join("OdHive/projects").join(name)
    }

    /// A service (with its own manager) and its router.
    pub fn app(&self) -> Api {
        let boot = bootstrap(&self.paths).expect("bootstrap");
        let manager = Manager::new(ManagerOptions {
            run_dir: Some(RunDir::new(self.paths.run_dir())),
            stop_timeout: Duration::from_secs(5),
            ..ManagerOptions::default()
        });
        let service = Service::new(self.paths.clone(), boot.workspace, manager);
        Api {
            router: app(AppConfig { port: PORT }, service.clone()),
            service,
        }
    }

    /// Gives project `name` a venv (a link to python3) and pins the ports of
    /// its one `main` instance.
    pub fn make_runnable(&self, name: &str, http: u16, gevent: u16) {
        let dir = self.project_dir(name);
        let bin = dir.join(".venv/bin");
        fs::create_dir_all(&bin).expect("mkdir");
        let path = std::env::var_os("PATH").expect("PATH");
        symlink(
            find_executable(&path, "python3").expect("python3"),
            bin.join("python"),
        )
        .expect("symlink");
        let mut p = ProjectConfig::load(&dir).expect("load");
        let mut i = Instance::new("main");
        i.http_port = Some(http);
        i.gevent_port = Some(gevent);
        p.instances = vec![i];
        p.save(&dir).expect("save");
    }
}

pub struct Api {
    pub router: Router,
    pub service: Service,
}

impl Api {
    /// Sends `req` as is.
    pub async fn send(&self, req: Request<Body>) -> Response {
        self.router.clone().oneshot(req).await.expect("response")
    }

    /// Sends an API request from the web UI (right Host, mutation header,
    /// JSON body) and returns the status and JSON body (`Null` when empty).
    pub async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, HOST)
            .header("x-odhive", "1");
        let body = match body {
            Some(v) => {
                req = req.header(header::CONTENT_TYPE, "application/json");
                Body::from(v.to_string())
            }
            None => Body::empty(),
        };
        let resp = self.send(req.body(body).expect("request")).await;
        split(resp).await
    }

    pub async fn get(&self, path: &str) -> (StatusCode, Value) {
        self.call(Method::GET, path, None).await
    }

    pub async fn post(&self, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        self.call(Method::POST, path, body).await
    }

    pub async fn delete(&self, path: &str) -> (StatusCode, Value) {
        self.call(Method::DELETE, path, None).await
    }

    /// Registers the fake source; returns its id.
    pub async fn add_source(&self, dir: &Path) -> String {
        let (status, body) = self
            .post(
                "/api/v1/sources",
                Some(serde_json::json!({ "path": dir.to_str().expect("utf8") })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body["id"].as_str().expect("id").to_owned()
    }

    /// Creates project `name` on source `source`.
    pub async fn create_project(&self, name: &str, source: &str) -> Value {
        let (status, body) = self
            .post(
                "/api/v1/projects",
                Some(serde_json::json!({ "name": name, "community": source })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body
    }
}

/// Status and JSON body of a response (`Null` for an empty body).
pub async fn split(resp: Response) -> (StatusCode, Value) {
    let status = resp.status();
    let bytes = resp.into_body().collect().await.expect("body").to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };
    (status, value)
}
