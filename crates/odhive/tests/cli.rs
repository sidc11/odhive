//! End-to-end CLI tests: the real `odhive` binary against a fake Odoo source
//! whose `odoo-bin` is the `fake-odoo-bin` fixture. Every test gets its own
//! HOME, so config, workspace and PID files are private to it.
//!
//! Instances use pinned ports from 18000–18999: below the ephemeral range and
//! away from the other test files' ranges. Each test has its own block.

use std::fs;
use std::io::{BufRead, BufReader};
use std::net::{Ipv4Addr, TcpListener};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use odhive_core::project::{Instance, ProjectConfig};
use odhive_core::python::find_executable;
use rustix::process::{Pid, Signal, kill_process};

const WAIT: Duration = Duration::from_secs(30);

struct Env {
    _tmp: tempfile::TempDir,
    home: PathBuf,
}

impl Env {
    /// A HOME with a registered fake Odoo 19 source.
    fn new() -> Self {
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
        let env = Self { _tmp: tmp, home };
        let out = env.ok(&["source", "add", src.to_str().expect("utf8")]);
        assert!(out.contains("odoo-19.0"), "{out}");
        env
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_odhive"));
        cmd.args(args)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_DATA_HOME", self.home.join(".local/share"))
            .env_remove("RUST_LOG")
            .stdin(Stdio::null());
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().expect("run odhive")
    }

    /// Runs a command that must succeed; returns its stdout.
    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(
            out.status.success(),
            "odhive {args:?} failed: {}\nstdout: {stdout}\nstderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        stdout
    }

    /// Runs a command that must fail; returns its stderr.
    fn fails(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(!out.status.success(), "odhive {args:?} succeeded");
        String::from_utf8_lossy(&out.stderr).into_owned()
    }

    fn project_dir(&self, name: &str) -> PathBuf {
        self.home.join("OdHive/projects").join(name)
    }

    /// Creates project `name` with a venv that is a link to python3, and pins
    /// the ports of `instances` (`(id, http, gevent, extra args)`).
    fn project(&self, name: &str, instances: &[(&str, u16, u16, &[&str])]) -> PathBuf {
        self.ok(&[
            "project",
            "add",
            name,
            "--source",
            "odoo-19.0",
            "--no-setup",
        ]);
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
        p.instances = instances
            .iter()
            .map(|(id, http, gevent, extra)| {
                let mut i = Instance::new(*id);
                i.http_port = Some(*http);
                i.gevent_port = Some(*gevent);
                i.extra_args = extra.iter().map(|s| (*s).to_owned()).collect();
                i
            })
            .collect();
        p.save(&dir).expect("save");
        dir
    }

    fn start(&self, args: &[&str]) -> Running {
        let mut child = self
            .cmd(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn");
        let (tx, lines) = mpsc::channel();
        for stream in [
            Box::new(child.stdout.take().expect("stdout")) as Box<dyn std::io::Read + Send>,
            Box::new(child.stderr.take().expect("stderr")),
        ] {
            let tx = tx.clone();
            thread::spawn(move || {
                for line in BufReader::new(stream).lines() {
                    let Ok(line) = line else { break };
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            });
        }
        Running {
            child: Some(child),
            lines,
            seen: Vec::new(),
        }
    }
}

/// A foreground `odhive start`.
struct Running {
    child: Option<Child>,
    lines: mpsc::Receiver<String>,
    seen: Vec<String>,
}

impl Running {
    fn wait_for(&mut self, needle: &str) -> String {
        if let Some(l) = self.seen.iter().find(|l| l.contains(needle)) {
            return l.clone();
        }
        let deadline = Instant::now() + WAIT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    self.seen.push(line.clone());
                    if line.contains(needle) {
                        return line;
                    }
                }
                Err(_) => panic!(
                    "no line with {needle:?}; output so far:\n{}",
                    self.seen.join("\n")
                ),
            }
        }
    }

    fn pid(&self) -> Pid {
        let id = self.child.as_ref().expect("child").id();
        Pid::from_raw(i32::try_from(id).expect("pid")).expect("pid")
    }

    /// Waits for the process to exit; returns whether it succeeded, and all
    /// of its output.
    fn finish(&mut self) -> (bool, String) {
        let mut child = self.child.take().expect("child");
        let deadline = Instant::now() + WAIT;
        let status = loop {
            if let Some(s) = child.try_wait().expect("wait") {
                break s;
            }
            assert!(Instant::now() < deadline, "odhive start did not exit");
            thread::sleep(Duration::from_millis(50));
        };
        while let Ok(line) = self.lines.recv_timeout(Duration::from_secs(2)) {
            self.seen.push(line);
        }
        (status.success(), self.seen.join("\n"))
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        // On a failed test: Ctrl+C, then kill (the instances die with it).
        if let Some(mut child) = self.child.take() {
            let pid = Pid::from_raw(i32::try_from(child.id()).expect("pid")).expect("pid");
            let _ = kill_process(pid, Signal::INT);
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                if child.try_wait().ok().flatten().is_some() {
                    return;
                }
                thread::sleep(Duration::from_millis(50));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn port_free(port: u16) -> bool {
    TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_ok()
}

fn eventually(what: &str, check: impl Fn() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(50));
    }
}

fn status_line(env: &Env, instance: &str) -> String {
    env.ok(&["status"])
        .lines()
        .find(|l| l.starts_with(&format!("{instance} ")))
        .unwrap_or_else(|| panic!("{instance} not in status"))
        .to_owned()
}

#[test]
fn project_add_list_and_remove() {
    let env = Env::new();
    let out = env.ok(&[
        "project",
        "add",
        "shop",
        "--source",
        "odoo-19.0",
        "--no-setup",
    ]);
    assert!(out.contains("created project shop"), "{out}");
    let dir = env.project_dir("shop");
    for sub in ["odhive.toml", "addons", "data", "logs", "backups"] {
        assert!(dir.join(sub).exists(), "{sub}");
    }
    let err = env.fails(&[
        "project",
        "add",
        "shop",
        "--source",
        "odoo-19.0",
        "--no-setup",
    ]);
    assert!(err.contains("already exists"), "{err}");
    let err = env.fails(&["project", "add", "other", "--source", "nope", "--no-setup"]);
    assert!(err.contains("no Community source \"nope\""), "{err}");

    let list = env.ok(&["project", "list"]);
    assert!(
        list.contains("shop") && list.contains("19.0") && list.contains("0/1 running"),
        "{list}"
    );
    let err = env.fails(&["start", "shop"]);
    assert!(err.contains("odhive project setup shop"), "{err}");
    let err = env.fails(&["source", "remove", "odoo-19.0"]);
    assert!(err.contains("used by shop"), "{err}");

    let out = env.ok(&["project", "remove", "shop"]);
    assert!(out.contains("folder is kept"), "{out}");
    assert!(dir.join("odhive.toml").is_file());
    assert!(env.ok(&["project", "list"]).contains("no projects"));
    env.ok(&["source", "remove", "odoo-19.0"]);
}

#[test]
fn start_status_logs_and_stop_from_another_terminal() {
    let env = Env::new();
    let dir = env.project("shop", &[("main", 18_000, 18_001, &[])]);
    let mut run = env.start(&["start", "shop"]);
    let line = run.wait_for("shop#main: running at");
    assert!(line.ends_with("http://localhost:18000"), "{line}");
    run.wait_for("Odoo version 19.0");

    let st = status_line(&env, "shop#main");
    assert!(
        st.contains(" running ") && st.contains("http://localhost:18000"),
        "{st}"
    );
    assert!(st.contains("shop_main"), "{st}");
    let logs = env.ok(&["logs", "shop", "-n", "100"]);
    assert!(
        logs.contains("odoo-bin --http-port") || logs.contains("Odoo version 19.0"),
        "{logs}"
    );

    // The generated config is used and keeps Odoo on loopback.
    let conf = fs::read_to_string(dir.join("odoo.conf")).expect("conf");
    assert!(conf.contains("http_interface = 127.0.0.1"), "{conf}");
    assert!(logs.contains("Using configuration file at"), "{logs}");

    // A running instance blocks a second start and removing its project.
    let err = env.fails(&["start", "shop"]);
    assert!(err.contains("already running"), "{err}");
    let err = env.fails(&["project", "remove", "shop"]);
    assert!(err.contains("still running"), "{err}");

    let out = env.ok(&["stop", "shop"]);
    assert!(out.contains("shop#main: stopped"), "{out}");
    let (ok, all) = run.finish();
    assert!(ok, "{all}");
    assert!(all.contains("shop#main: stopped"), "{all}");
    assert!(port_free(18_000));
    assert!(status_line(&env, "shop#main").contains("stopped"));
    assert!(env.ok(&["stop", "shop"]).contains("nothing is running"));

    // The log file outlives the run.
    let logs = env.ok(&["logs", "shop#main"]);
    assert!(logs.contains("Initiating shutdown"), "{logs}");
}

#[test]
fn two_instances_run_side_by_side_and_stop_one_at_a_time() {
    let env = Env::new();
    env.project(
        "shop",
        &[("main", 18_100, 18_101, &[]), ("test", 18_102, 18_103, &[])],
    );
    let mut run = env.start(&["start", "shop", "--quiet"]);
    run.wait_for("shop#main: running at http://localhost:18100");
    run.wait_for("shop#test: running at http://localhost:18102");
    assert!(
        !run.seen.iter().any(|l| l.contains("Odoo version")),
        "--quiet prints no instance output"
    );

    env.ok(&["stop", "shop#test"]);
    run.wait_for("shop#test: stopped");
    assert!(port_free(18_102));
    assert!(status_line(&env, "shop#main").contains(" running "));
    assert!(status_line(&env, "shop#test").contains("stopped"));
    // Both logs are kept apart; `logs shop` shows both, prefixed.
    let logs = env.ok(&["logs", "shop"]);
    assert!(
        logs.contains("[main] ") && logs.contains("[test] "),
        "{logs}"
    );

    env.ok(&["stop", "shop"]);
    let (ok, all) = run.finish();
    assert!(ok, "{all}");
    assert!(port_free(18_100));
}

#[test]
fn ctrl_c_stops_everything_and_restart_takes_over() {
    let env = Env::new();
    env.project("shop", &[("main", 18_200, 18_201, &[])]);
    let mut first = env.start(&["start", "shop"]);
    first.wait_for("shop#main: running");

    // `restart` from another terminal: the first run ends, the second takes over.
    let mut second = env.start(&["restart", "shop"]);
    second.wait_for("shop#main: running at http://localhost:18200");
    let (ok, all) = first.finish();
    assert!(ok, "{all}");

    kill_process(second.pid(), Signal::INT).expect("SIGINT");
    second.wait_for("stopping…");
    let (ok, all) = second.finish();
    assert!(ok, "{all}");
    assert!(all.contains("shop#main: stopped"), "{all}");
    assert!(port_free(18_200));
    assert!(status_line(&env, "shop#main").contains("stopped"));
}

#[test]
fn a_crash_ends_the_run_with_failure() {
    let env = Env::new();
    env.project(
        "shop",
        &[("main", 18_300, 18_301, &["--simulate", "crash"])],
    );
    let mut run = env.start(&["start", "shop"]);
    let (ok, all) = run.finish();
    assert!(!ok, "{all}");
    assert!(all.contains("shop#main: crashed (exit code 1)"), "{all}");
    assert!(all.contains("simulated fatal error"), "{all}");
    assert!(status_line(&env, "shop#main").contains("stopped"));
}

#[test]
fn smart_reload_restarts_on_changes_and_waits_after_a_crash() {
    let env = Env::new();
    let dir = env.project(
        "shop",
        &[("main", 18_800, 18_801, &["--simulate", "crash"])],
    );
    let module = dir.join("addons/m1");
    fs::create_dir_all(module.join("views")).expect("mkdir");
    fs::write(module.join("__manifest__.py"), "{}").expect("manifest");
    let mut run = env.start(&["start", "shop"]);
    run.wait_for("shop#main: smart reload watching");
    run.wait_for("shop#main: crashed (exit code 1)");
    // The crash does not end the run: a fix is waited for.
    run.wait_for("shop#main: waiting for a file change to restart it");
    fs::write(module.join("views/x.xml"), "<odoo/>").expect("write");
    let line = run.wait_for("changed; restarting");
    assert!(
        line.contains("views/x.xml changed; restarting with -u m1"),
        "{line}"
    );
    run.wait_for("shop#main: starting (pid");
    run.wait_for("Loading module m1 ");
    kill_process(run.pid(), Signal::INT).expect("SIGINT");
    let (_, all) = run.finish();
    assert!(all.contains("stopping…"), "{all}");
    eventually("port 18800 free", || port_free(18_800));
    let log = fs::read_to_string(dir.join("logs/main.log")).expect("log");
    assert!(log.contains("changed; restarting with -u m1"), "{log}");
    assert!(log.contains("Loading module m1 "), "{log}");
}

#[test]
fn missing_modules_are_reported_and_never_installed_unasked() {
    let env = Env::new();
    // The test venv is a link to python3, not a venv: the installer refuses
    // it, so nothing is downloaded even for a trusted package.
    env.project(
        "shop",
        &[
            (
                "main",
                18_700,
                18_701,
                &["--simulate-missing", "zz_cli_missing_mod"],
            ),
            ("test", 18_702, 18_703, &["--simulate-missing", "ebaysdk"]),
        ],
    );
    let mut run = env.start(&["start", "shop"]);
    let (ok, all) = run.finish();
    assert!(!ok, "{all}");
    // Untrusted, and stdin is not a terminal: not installed.
    assert!(
        all.contains("shop#main: missing Python module \"zz_cli_missing_mod\"; zz_cli_missing_mod is not in the project's requirements"),
        "{all}"
    );
    assert!(
        all.contains("shop#main: not installing zz_cli_missing_mod without confirmation"),
        "{all}"
    );
    // Trusted by the built-in table: installed, which fails here.
    assert!(
        all.contains("shop#test: missing Python module \"ebaysdk\"; installing ebaysdk (from OdHive's package table)"),
        "{all}"
    );
    assert!(
        all.contains("shop#test: installing ebaysdk (attempt 1/3)"),
        "{all}"
    );
    assert!(
        all.contains("shop#test: installing ebaysdk failed"),
        "{all}"
    );
    assert!(
        all.contains("not a virtual environment interpreter"),
        "{all}"
    );
    let log = fs::read_to_string(env.project_dir("shop").join("logs/test.log")).expect("log");
    assert!(
        log.contains("odhive: installing ebaysdk for missing module \"ebaysdk\""),
        "{log}"
    );
}

#[test]
fn instances_of_a_killed_odhive_are_shown_detached_and_can_be_stopped() {
    let env = Env::new();
    env.project("shop", &[("main", 18_400, 18_401, &[])]);
    let mut run = env.start(&["start", "shop", "--quiet"]);
    run.wait_for("shop#main: running");
    kill_process(run.pid(), Signal::KILL).expect("SIGKILL");
    let _ = run.finish();

    // Odoo survives its supervisor.
    assert!(!port_free(18_400));
    eventually("detached state", || {
        status_line(&env, "shop#main").contains("running, detached")
    });
    let out = env.ok(&["stop", "shop"]);
    assert!(out.contains("shop#main"), "{out}");
    eventually("port 18400 free", || port_free(18_400));
    assert!(status_line(&env, "shop#main").contains("stopped"));
}

#[test]
fn unknown_targets_are_reported() {
    let env = Env::new();
    env.project("shop", &[("main", 18_500, 18_501, &[])]);
    let err = env.fails(&["start", "nope"]);
    assert!(err.contains("no project named \"nope\""), "{err}");
    let err = env.fails(&["stop", "shop#nope"]);
    assert!(
        err.contains("has no instance \"nope\" (instances: main)"),
        "{err}"
    );
    let err = env.fails(&["logs", "Bad#x"]);
    assert!(err.contains("invalid"), "{err}");
    let out = env.ok(&["logs", "shop"]);
    assert!(out.contains("no log yet"), "{out}");
}

#[test]
fn ctrl_c_during_startup_leaves_no_orphans() {
    let env = Env::new();
    env.project("shop", &[("main", 18_600, 18_601, &[])]);
    let mut run = env.start(&["start", "shop", "--quiet"]);
    run.wait_for("shop#main: starting");
    kill_process(run.pid(), Signal::INT).expect("SIGINT");
    let (ok, all) = run.finish();
    assert!(ok, "{all}");
    eventually("port 18600 free", || port_free(18_600));
    let st = status_line(&env, "shop#main");
    assert!(st.contains("stopped") && !st.contains("stale"), "{st}");
}

/// OdHive's data folder inside this test's HOME.
fn data_dir(env: &Env) -> PathBuf {
    if cfg!(target_os = "macos") {
        env.home.join("Library/Application Support/odhive")
    } else {
        env.home.join(".local/share/odhive")
    }
}

/// Sends a plain HTTP/1.1 GET to the UI port; returns the raw response.
fn http_get(port: u16, path: &str) -> String {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("connect");
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n"
    )
    .expect("write");
    let mut out = String::new();
    s.read_to_string(&mut out).expect("read");
    out
}

#[test]
fn serve_creates_a_private_token_and_prints_a_one_time_sign_in_link() {
    let env = Env::new();
    let mut run = env.start(&["--port", "18950"]);
    let line = run.wait_for("Open OdHive");
    let url = line.split_whitespace().last().expect("url").to_owned();
    let rest = url.strip_prefix("http://localhost:").expect("url");
    let (port, path) = rest.split_once('/').expect("path");
    let port: u16 = port.parse().expect("port");
    let code = path.strip_prefix("login?t=").expect("login path");
    assert_eq!(code.len(), 64, "{url}");

    let token = data_dir(&env).join("token");
    let mode = fs::metadata(&token).expect("token").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let hex = fs::read_to_string(&token).expect("read");
    let hex = hex.trim_end();
    assert!(!line.contains(hex), "the token itself is never printed");

    let resp = http_get(port, &format!("/{path}"));
    assert!(resp.starts_with("HTTP/1.1 303"), "{resp}");
    assert!(resp.contains("\r\nlocation: /\r\n"), "{resp}");
    assert!(
        resp.contains(&format!("set-cookie: odhive_session={hex}; ")),
        "{resp}"
    );
    let again = http_get(port, &format!("/{path}"));
    assert!(again.starts_with("HTTP/1.1 401"), "reused: {again}");
    let api = http_get(port, "/api/v1/health");
    assert!(api.starts_with("HTTP/1.1 401"), "no cookie: {api}");

    kill_process(run.pid(), Signal::INT).expect("SIGINT");
    let (ok, all) = run.finish();
    assert!(ok, "{all}");
    assert!(!all.contains(hex), "{all}");

    // The next start keeps the token.
    let mut run = env.start(&["--port", "18950"]);
    run.wait_for("Open OdHive");
    assert_eq!(fs::read_to_string(&token).expect("read").trim_end(), hex);
    kill_process(run.pid(), Signal::INT).expect("SIGINT");
    assert!(run.finish().0);
}

#[test]
fn serve_refuses_a_token_file_others_can_read() {
    let env = Env::new();
    let token = data_dir(&env).join("token");
    fs::create_dir_all(token.parent().expect("parent")).expect("mkdir");
    fs::write(&token, format!("{}\n", "ab".repeat(32))).expect("write");
    fs::set_permissions(&token, fs::Permissions::from_mode(0o644)).expect("chmod");
    let err = env.fails(&["--port", "18951"]);
    assert!(err.contains("too wide"), "{err}");
    assert!(port_free(18_951));
    // Left as it was for the user to fix.
    let mode = fs::metadata(&token).expect("token").permissions().mode() & 0o777;
    assert_eq!(mode, 0o644);
}
