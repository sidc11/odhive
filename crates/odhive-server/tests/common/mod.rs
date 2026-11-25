//! Shared test harness: a private OdHive home with a registered-able fake
//! Odoo 19 source (its `odoo-bin` is the `fake-odoo-bin` fixture) and the
//! app router served in-process.

#![allow(dead_code)]

pub mod pg;

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
use odhive_core::python::{DiscoverOptions, find_executable};
use odhive_core::sources::CloneOptions;
use odhive_core::token::Token;
use odhive_core::workspace::bootstrap;
use odhive_server::{AppConfig, Auth, Service, ServiceOptions, app};
use serde_json::Value;
use tower::ServiceExt;

/// The port the app believes it serves on.
pub const PORT: u16 = 7069;
pub const HOST: &str = "localhost:7069";
/// The `Origin` of the web UI served at [`HOST`].
pub const ORIGIN: &str = "http://localhost:7069";

/// Time a job may take in tests.
pub const JOB_WAIT: Duration = Duration::from_secs(120);

pub fn python3() -> PathBuf {
    let path = std::env::var_os("PATH").expect("PATH");
    find_executable(&path, "python3").expect("python3 on PATH")
}

/// Service options that look for nothing on `PATH` (setups name their
/// Python) and use uv only if it is installed; clones come from `url`.
pub fn job_options(url: &str) -> ServiceOptions {
    let path = std::env::var_os("PATH").expect("PATH");
    ServiceOptions {
        clone: CloneOptions {
            url: url.to_owned(),
            ..CloneOptions::default()
        },
        discover: DiscoverOptions {
            path_var: None,
            uv: find_executable(&path, "uv"),
            explicit: Vec::new(),
            timeout: Duration::from_secs(30),
        },
        ..ServiceOptions::default()
    }
}

/// Runs git in `dir` with a private identity and no user config.
pub fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=OdHive Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("run git");
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

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

    /// A local git repository holding the fake Odoo tree on branch `19.0`;
    /// returns its `file://` URL.
    pub fn upstream(&self) -> String {
        let repo = self.home.join("upstream");
        fs::create_dir_all(&repo).expect("mkdir");
        git(&repo, &["init", "-q", "-b", "19.0"]);
        copy_tree(&self.source_dir(), &repo);
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "init"]);
        format!("file://{}", repo.display())
    }

    pub fn source_dir(&self) -> PathBuf {
        self.home.join("odoo-src")
    }

    pub fn project_dir(&self, name: &str) -> PathBuf {
        self.home.join("OdHive/projects").join(name)
    }

    /// A service (with its own manager) and its router.
    pub fn app(&self) -> Api {
        self.app_with(ManagerOptions::default(), ServiceOptions::default(), PORT)
    }

    /// Like [`Env::app`] with other options, for an app serving on `port`.
    pub fn app_with(&self, manager: ManagerOptions, opts: ServiceOptions, port: u16) -> Api {
        let token = Token::generate().expect("token");
        self.app_with_auth(manager, opts, port, token, odhive_server::auth::CODE_TTL)
    }

    /// Like [`Env::app_with`] for `token`, with login codes valid for `ttl`.
    pub fn app_with_auth(
        &self,
        manager: ManagerOptions,
        opts: ServiceOptions,
        port: u16,
        token: Token,
        ttl: Duration,
    ) -> Api {
        let boot = bootstrap(&self.paths).expect("bootstrap");
        let manager = Manager::new(ManagerOptions {
            run_dir: Some(RunDir::new(self.paths.run_dir())),
            stop_timeout: Duration::from_secs(5),
            ..manager
        });
        let cookie = format!("{}={}", odhive_server::auth::SESSION_COOKIE, token.to_hex());
        let bearer = format!("Bearer {}", token.to_hex());
        let service = Service::with_options(
            self.paths.clone(),
            Auth::with_code_ttl(token, ttl),
            boot.workspace,
            manager,
            opts,
        );
        Api {
            router: app(AppConfig::new(port), service.clone()),
            service,
            host: format!("localhost:{port}"),
            cookie,
            bearer,
        }
    }

    /// Serves an app with these options on a loopback port; returns it and
    /// the port.
    pub async fn serve_with(&self, manager: ManagerOptions, opts: ServiceOptions) -> (Api, u16) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let api = self.app_with(manager, opts, port);
        let router = api.router.clone();
        tokio::spawn(async move {
            axum::serve(listener, router).await.expect("serve");
        });
        (api, port)
    }

    /// Gives project `name` a venv (a link to python3) and pins the ports of
    /// its one `main` instance.
    pub fn make_runnable(&self, name: &str, http: u16, gevent: u16) {
        self.make_runnable_with(name, http, gevent, &[]);
    }

    /// Like [`Env::make_runnable`], with extra `odoo-bin` arguments.
    pub fn make_runnable_with(&self, name: &str, http: u16, gevent: u16, args: &[&str]) {
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
        i.extra_args = args.iter().map(|&a| a.to_owned()).collect();
        p.instances = vec![i];
        p.save(&dir).expect("save");
    }
}

pub struct Api {
    pub router: Router,
    pub service: Service,
    /// The `Host` header requests carry.
    pub host: String,
    /// The `Cookie` header of a signed-in browser.
    pub cookie: String,
    /// The `Authorization` header of a non-browser client (the CLI).
    pub bearer: String,
}

impl Api {
    /// Sends `req` as is.
    pub async fn send(&self, req: Request<Body>) -> Response {
        self.router.clone().oneshot(req).await.expect("response")
    }

    /// Sends an API request from the signed-in web UI (right Host and
    /// Origin, session cookie, mutation header, JSON body) and returns the status and JSON
    /// body (`Null` when empty).
    pub async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        split(self.respond(method, path, body).await).await
    }

    /// As [`Api::call`], but returns the whole response (headers too).
    pub async fn respond(&self, method: Method, path: &str, body: Option<Value>) -> Response {
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, &self.host)
            .header(header::ORIGIN, format!("http://{}", self.host))
            .header(header::COOKIE, &self.cookie)
            .header("x-odhive", "1");
        let body = match body {
            Some(v) => {
                req = req.header(header::CONTENT_TYPE, "application/json");
                Body::from(v.to_string())
            }
            None => Body::empty(),
        };
        self.send(req.body(body).expect("request")).await
    }

    pub async fn get(&self, path: &str) -> (StatusCode, Value) {
        self.call(Method::GET, path, None).await
    }

    pub async fn post(&self, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        self.call(Method::POST, path, body).await
    }

    pub async fn put(&self, path: &str, body: Value) -> (StatusCode, Value) {
        self.call(Method::PUT, path, Some(body)).await
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

fn copy_tree(from: &Path, to: &Path) {
    for e in fs::read_dir(from).expect("read_dir") {
        let e = e.expect("entry");
        let dest = to.join(e.file_name());
        if e.file_type().expect("type").is_dir() {
            fs::create_dir_all(&dest).expect("mkdir");
            copy_tree(&e.path(), &dest);
        } else {
            fs::copy(e.path(), &dest).expect("copy");
        }
    }
}

impl Api {
    /// Sends `body` as a file upload (`application/octet-stream`, with its
    /// `Content-Length`) from the signed-in web UI.
    pub async fn upload(&self, path: &str, body: Body, length: Option<u64>) -> (StatusCode, Value) {
        let mut req = Request::builder()
            .method(Method::POST)
            .uri(path)
            .header(header::HOST, &self.host)
            .header(header::ORIGIN, format!("http://{}", self.host))
            .header(header::COOKIE, &self.cookie)
            .header("x-odhive", "1")
            .header(header::CONTENT_TYPE, "application/octet-stream");
        if let Some(n) = length {
            req = req.header(header::CONTENT_LENGTH, n);
        }
        split(self.send(req.body(body).expect("request")).await).await
    }

    /// Polls job `id` until it is no longer running; returns it.
    pub async fn wait_job(&self, id: u64) -> Value {
        let deadline = std::time::Instant::now() + JOB_WAIT;
        loop {
            let (status, job) = self.get(&format!("/api/v1/jobs/{id}")).await;
            assert_eq!(status, StatusCode::OK, "{job}");
            if job["state"] != "running" {
                return job;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "job {id} still runs: {job}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
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
