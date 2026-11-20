//! Verifies the `fake-odoo-bin` fixture behaves like Odoo, so later supervisor tests can rely on it.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process};

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/fake-odoo-bin")
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

fn spawn(args: &[&str]) -> Child {
    // Executed directly with an argument vector — never through a shell.
    Command::new(fixture())
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn fake-odoo-bin")
}

/// Reads stdout lines until one contains `needle`; returns everything read so far.
fn read_until(child: &mut Child, needle: &str) -> (BufReader<std::process::ChildStdout>, String) {
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);
    let mut seen = String::new();
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).expect("read line");
        assert!(
            n > 0,
            "fixture exited before printing {needle:?}; output:\n{seen}"
        );
        seen.push_str(&line);
        if line.contains(needle) {
            return (reader, seen);
        }
    }
}

fn wait_with_timeout(child: &mut Child, timeout: Duration) -> std::process::ExitStatus {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            panic!("fixture did not exit within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn binds_http_port_logs_and_stops_on_sigint() {
    let port = free_port();
    let port_s = port.to_string();
    let mut child = spawn(&[
        "--http-port",
        &port_s,
        "-d",
        "devdb",
        "--odoo-version",
        "20.0",
    ]);
    let (mut reader, seen) = read_until(&mut child, "HTTP service (werkzeug) running on");
    assert!(seen.contains("INFO ? odoo: Odoo version 20.0"), "{seen}");
    assert!(seen.contains(&format!(":{port}")));

    let mut conn = TcpStream::connect(("127.0.0.1", port)).expect("connect to fake odoo");
    conn.write_all(b"GET /web/webclient/version_info HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .expect("write");
    let mut resp = String::new();
    conn.read_to_string(&mut resp).expect("read");
    assert!(resp.starts_with("HTTP/1.0 200"), "{resp}");
    assert!(resp.contains("\"server_version\": \"20.0\""));

    let pid = Pid::from_raw(i32::try_from(child.id()).expect("pid")).expect("pid");
    kill_process(pid, Signal::INT).expect("send SIGINT");
    let status = wait_with_timeout(&mut child, Duration::from_secs(10));
    assert!(status.success(), "exit status {status:?}");

    let mut rest = String::new();
    reader.read_to_string(&mut rest).expect("read rest");
    assert!(
        rest.contains("odoo.service.server: Initiating shutdown"),
        "{rest}"
    );
}

#[test]
fn simulates_module_not_found() {
    let mut child = spawn(&[
        "--simulate",
        "module-not-found",
        "--missing-module",
        "phonenumbers",
    ]);
    let status = wait_with_timeout(&mut child, Duration::from_secs(10));
    let mut out = String::new();
    child
        .stdout
        .take()
        .expect("stdout")
        .read_to_string(&mut out)
        .expect("read");
    assert_eq!(status.code(), Some(1));
    assert!(out.contains("Traceback (most recent call last):"));
    assert!(
        out.contains("ModuleNotFoundError: No module named 'phonenumbers'"),
        "{out}"
    );
}

#[test]
fn simulates_traceback_and_keeps_running() {
    let port = free_port().to_string();
    let mut child = spawn(&[
        "--http-port",
        &port,
        "-d",
        "db1",
        "--simulate",
        "traceback",
        "--exit-after",
        "0.5",
    ]);
    let status = wait_with_timeout(&mut child, Duration::from_secs(10));
    let mut out = String::new();
    child
        .stdout
        .take()
        .expect("stdout")
        .read_to_string(&mut out)
        .expect("read");
    assert!(status.success());
    assert!(
        out.contains(" ERROR db1 odoo.http: Exception during request handling."),
        "{out}"
    );
    assert!(out.contains("ValueError: Expected singleton"));
}

#[test]
fn simulates_crash() {
    let port = free_port().to_string();
    let mut child = spawn(&["--http-port", &port, "--simulate", "crash"]);
    let status = wait_with_timeout(&mut child, Duration::from_secs(10));
    let mut out = String::new();
    child
        .stdout
        .take()
        .expect("stdout")
        .read_to_string(&mut out)
        .expect("read");
    assert_eq!(status.code(), Some(1));
    assert!(
        out.contains("CRITICAL ? odoo.service.server: Failed to start server"),
        "{out}"
    );
}

#[test]
fn log_lines_use_odoo_format() {
    let mut child = spawn(&["--simulate", "module-not-found"]);
    let _ = wait_with_timeout(&mut child, Duration::from_secs(10));
    let mut out = String::new();
    child
        .stdout
        .take()
        .expect("stdout")
        .read_to_string(&mut out)
        .expect("read");
    let first = out.lines().next().expect("a line");
    // "2026-11-21 10:00:00,123 4242 INFO ? odoo: Odoo version 19.0"
    let parts: Vec<&str> = first.splitn(6, ' ').collect();
    assert_eq!(parts.len(), 6, "{first}");
    assert_eq!(parts[0].len(), 10);
    assert!(parts[1].contains(','));
    assert!(parts[2].parse::<u32>().is_ok());
    assert_eq!(parts[3], "INFO");
    assert_eq!(parts[4], "?");
}
