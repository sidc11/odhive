//! Supervisor integration tests against the `fake-odoo-bin` fixture.
//!
//! Each test allocates ports from its own range so tests running in parallel
//! processes do not race each other for the same port.

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use odhive_core::ports::{AllocOptions, PortOwner, PortRegistry, Reservation};
use odhive_core::project::EnvVars;
use odhive_core::python::find_executable;
use odhive_core::supervisor::{
    self, Event, LaunchSpec, Status, SuperviseOptions, Supervised, SupervisorError,
};
use rustix::process::{Pid, Signal, kill_process, test_kill_process, test_kill_process_group};

const WAIT: Duration = Duration::from_secs(20);

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/fake-odoo-bin")
}

fn python3() -> PathBuf {
    let path = std::env::var_os("PATH").expect("PATH");
    find_executable(&path, "python3").expect("python3 on PATH")
}

fn fake_spec(dir: &Path, extra: &[&str]) -> LaunchSpec {
    let mut spec = LaunchSpec::new(python3(), fixture(), dir.to_path_buf());
    spec.db = Some("devdb".into());
    spec.extra_args = extra.iter().map(|s| (*s).to_owned()).collect();
    spec
}

fn opts_from(base: u16) -> AllocOptions {
    AllocOptions {
        http_start: base,
        gevent_start: base + 50,
        scan: 40,
    }
}

fn alloc(reg: &PortRegistry, instance: &str, base: u16) -> Reservation {
    reg.allocate(
        PortOwner::new("shop", instance),
        None,
        None,
        &opts_from(base),
    )
    .expect("allocate")
}

#[derive(Clone, Default)]
struct Events(Arc<Mutex<Vec<Event>>>);

impl Events {
    fn sink(&self) -> impl FnMut(Event) + Send + 'static {
        let events = self.0.clone();
        move |e| events.lock().expect("lock").push(e)
    }

    fn all(&self) -> Vec<Event> {
        self.0.lock().expect("lock").clone()
    }

    fn output(&self) -> String {
        self.all()
            .into_iter()
            .filter_map(|e| match e {
                Event::Output { line, .. } => Some(line),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn start(spec: LaunchSpec, ports: Reservation, events: &Events) -> Supervised {
    supervisor::start(spec, ports, SuperviseOptions::default(), events.sink()).expect("start")
}

async fn wait_running(sup: &Supervised, events: &Events) -> u32 {
    let status = tokio::time::timeout(
        WAIT,
        sup.wait_for(|s| matches!(s, Status::Running { .. }) || s.is_terminal()),
    )
    .await
    .unwrap_or_else(|_| panic!("not running in time; output:\n{}", events.output()))
    .expect("supervisor");
    match status {
        Status::Running { pid } => pid,
        other => panic!("{other:?}\noutput:\n{}", events.output()),
    }
}

async fn wait_terminal(sup: &Supervised) -> Status {
    tokio::time::timeout(WAIT, sup.wait_for(Status::is_terminal))
        .await
        .expect("no exit in time")
        .expect("supervisor")
}

fn pid(raw: u32) -> Pid {
    Pid::from_raw(i32::try_from(raw).expect("pid")).expect("pid")
}

async fn wait_gone(check: impl Fn() -> bool, what: &str) {
    let gone = async {
        while !check() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), gone)
        .await
        .unwrap_or_else(|_| panic!("{what} still alive"));
}

fn accepts(port: u16) -> bool {
    TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_ok()
}

#[tokio::test]
async fn start_listen_stop_leaves_no_processes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let reg = PortRegistry::new();
    let events = Events::default();
    let sup = start(
        fake_spec(dir.path(), &[]),
        alloc(&reg, "main", 36_000),
        &events,
    );
    let http = sup.http_port();
    let pid = wait_running(&sup, &events).await;
    assert!(accepts(http));
    assert!(matches!(
        events.all().first(),
        Some(Event::Spawned { http_port, .. }) if *http_port == http
    ));

    let status = sup.stop().await.expect("stop");
    let Status::Stopped { exit } = &status else {
        panic!("{status:?}");
    };
    assert!(exit.success(), "{exit:?}");
    assert!(events.output().contains("Initiating shutdown"));
    wait_gone(
        || test_kill_process_group(self::pid(pid)).is_err(),
        "process group",
    )
    .await;
    assert!(!accepts(http));

    // Ports stay reserved for a restart until the handle is shut down.
    assert_eq!(reg.reserved().len(), 2);
    sup.shutdown().await.expect("shutdown");
    assert!(reg.reserved().is_empty());
}

#[tokio::test]
async fn busy_port_moves_and_the_event_reports_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let reg = PortRegistry::new();
    let base = 36_200;
    let (busy, _hold) = (base..base + 40)
        .find_map(|p| {
            TcpListener::bind((Ipv4Addr::LOCALHOST, p))
                .ok()
                .map(|l| (p, l))
        })
        .expect("a port to occupy");
    let opts = AllocOptions {
        http_start: busy,
        ..opts_from(base)
    };
    let ports = reg
        .allocate(PortOwner::new("shop", "main"), None, None, &opts)
        .expect("allocate");
    let events = Events::default();
    let sup = start(fake_spec(dir.path(), &[]), ports, &events);
    assert!(sup.http_port() > busy);
    wait_running(&sup, &events).await;
    assert!(accepts(sup.http_port()));

    let Some(Event::Spawned { changes, .. }) = events.all().into_iter().next() else {
        panic!("first event is not Spawned");
    };
    let msg = changes.first().expect("a port change").to_string();
    assert_eq!(
        msg,
        format!("http port {busy} busy → using {}", sup.http_port())
    );
    sup.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn two_instances_of_one_project_run_side_by_side() {
    let dir = tempfile::tempdir().expect("tempdir");
    let reg = PortRegistry::new();
    let (ev_a, ev_b) = (Events::default(), Events::default());
    let a = start(
        fake_spec(dir.path(), &[]),
        alloc(&reg, "main", 36_400),
        &ev_a,
    );
    let b = start(
        fake_spec(dir.path(), &[]),
        alloc(&reg, "test", 36_400),
        &ev_b,
    );
    let pid_a = wait_running(&a, &ev_a).await;
    let pid_b = wait_running(&b, &ev_b).await;
    assert_ne!(pid_a, pid_b);
    let ports = [
        a.http_port(),
        a.gevent_port(),
        b.http_port(),
        b.gevent_port(),
    ];
    let mut unique = ports.to_vec();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), 4, "{ports:?}");
    assert!(accepts(a.http_port()) && accepts(b.http_port()));

    // Stopping one leaves the other running.
    a.shutdown().await.expect("shutdown a");
    assert!(matches!(b.status(), Status::Running { .. }));
    assert!(accepts(b.http_port()));
    b.shutdown().await.expect("shutdown b");
    assert!(reg.reserved().is_empty());
}

#[tokio::test]
async fn crash_sets_crashed_with_exit_code() {
    let dir = tempfile::tempdir().expect("tempdir");
    let reg = PortRegistry::new();
    let events = Events::default();
    let sup = start(
        fake_spec(dir.path(), &["--simulate", "crash"]),
        alloc(&reg, "main", 36_600),
        &events,
    );
    let status = wait_terminal(&sup).await;
    let Status::Crashed { exit } = status else {
        panic!("{status:?}");
    };
    assert_eq!(exit.code, Some(1));
    assert_eq!(exit.signal, None);
    assert!(!exit.port_in_use);
    assert!(
        exit.tail.contains("Failed to start server"),
        "{}",
        exit.tail
    );
    let json = serde_json::to_value(Status::Crashed { exit }).expect("json");
    assert_eq!(json["state"], "crashed");
}

#[tokio::test]
async fn port_taken_after_the_probe_is_flagged() {
    let dir = tempfile::tempdir().expect("tempdir");
    let reg = PortRegistry::new();
    let (busy, _hold) = (36_800..36_840)
        .find_map(|p| {
            TcpListener::bind((Ipv4Addr::LOCALHOST, p))
                .ok()
                .map(|l| (p, l))
        })
        .expect("a port to occupy");
    // `reserve` does not probe: it stands in for another program winning the
    // race between OdHive's probe and Odoo's bind.
    let ports = reg
        .reserve(PortOwner::new("shop", "main"), busy, busy + 100)
        .expect("reserve");
    let events = Events::default();
    let sup = start(fake_spec(dir.path(), &[]), ports, &events);
    let status = wait_terminal(&sup).await;
    let Status::Crashed { exit } = status else {
        panic!("{status:?}");
    };
    assert!(exit.port_in_use, "{}", events.output());
}

#[tokio::test]
async fn exit_code_zero_without_a_request_is_stopped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let reg = PortRegistry::new();
    let events = Events::default();
    let sup = start(
        fake_spec(dir.path(), &["--exit-after", "0.5"]),
        alloc(&reg, "main", 37_000),
        &events,
    );
    let status = wait_terminal(&sup).await;
    assert!(
        matches!(&status, Status::Stopped { exit } if exit.success()),
        "{status:?}"
    );
}

#[tokio::test]
async fn killed_from_outside_is_a_crash_with_the_signal() {
    let dir = tempfile::tempdir().expect("tempdir");
    let reg = PortRegistry::new();
    let events = Events::default();
    let sup = start(
        fake_spec(dir.path(), &[]),
        alloc(&reg, "main", 37_200),
        &events,
    );
    let raw = wait_running(&sup, &events).await;
    kill_process(pid(raw), Signal::KILL).expect("kill");
    let status = wait_terminal(&sup).await;
    let Status::Crashed { exit } = status else {
        panic!("{status:?}");
    };
    assert_eq!(exit.code, None);
    assert_eq!(exit.signal, Some(9));
}

#[tokio::test]
async fn restart_runs_a_new_process_on_the_same_ports() {
    let dir = tempfile::tempdir().expect("tempdir");
    let reg = PortRegistry::new();
    let events = Events::default();
    let sup = start(
        fake_spec(dir.path(), &[]),
        alloc(&reg, "main", 37_400),
        &events,
    );
    let first = wait_running(&sup, &events).await;
    let (http, gevent) = (sup.http_port(), sup.gevent_port());

    let second = sup.restart().await.expect("restart");
    assert_ne!(first, second);
    wait_gone(|| test_kill_process(pid(first)).is_err(), "first process").await;
    assert_eq!(wait_running(&sup, &events).await, second);
    assert_eq!((sup.http_port(), sup.gevent_port()), (http, gevent));
    assert!(accepts(http));
    let spawned = events
        .all()
        .iter()
        .filter(|e| matches!(e, Event::Spawned { .. }))
        .count();
    assert_eq!(spawned, 2);

    // A crashed instance can be restarted too.
    kill_process(pid(second), Signal::KILL).expect("kill");
    assert!(matches!(wait_terminal(&sup).await, Status::Crashed { .. }));
    let third = sup.restart().await.expect("restart after crash");
    assert_eq!(wait_running(&sup, &events).await, third);
    sup.shutdown().await.expect("shutdown");
}

/// A process that ignores SIGINT is killed after the stop timeout, and so is
/// the child it started.
#[tokio::test]
async fn ignored_sigint_falls_back_to_sigkill_of_the_group() {
    let dir = tempfile::tempdir().expect("tempdir");
    let script = dir.path().join("stubborn");
    let pidfile = dir.path().join("grandchild.pid");
    std::fs::write(
        &script,
        "trap '' INT\nsleep 300 &\necho $! > \"$GRANDCHILD_PIDFILE\"\nwait\n",
    )
    .expect("write script");
    let mut spec = LaunchSpec::new("/bin/sh".into(), script, dir.path().to_path_buf());
    spec.env = EnvVars(BTreeMap::from([(
        "GRANDCHILD_PIDFILE".to_owned(),
        pidfile.display().to_string(),
    )]));
    let reg = PortRegistry::new();
    let opts = SuperviseOptions {
        stop_timeout: Duration::from_millis(300),
        ..SuperviseOptions::default()
    };
    let events = Events::default();
    let sup =
        supervisor::start(spec, alloc(&reg, "main", 37_600), opts, events.sink()).expect("start");
    let leader = sup.status().pid().expect("pid");
    let grandchild = tokio::time::timeout(WAIT, async {
        loop {
            if let Ok(s) = std::fs::read_to_string(&pidfile)
                && let Ok(p) = s.trim().parse::<u32>()
            {
                return p;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("grandchild pid");
    // Never listens, so it stays `starting`.
    assert!(matches!(sup.status(), Status::Starting { .. }));

    let status = sup.stop().await.expect("stop");
    let Status::Stopped { exit } = &status else {
        panic!("{status:?}");
    };
    assert_eq!(exit.signal, Some(9), "{exit:?}");
    wait_gone(
        || test_kill_process_group(pid(leader)).is_err(),
        "process group",
    )
    .await;
    wait_gone(|| test_kill_process(pid(grandchild)).is_err(), "grandchild").await;
}

/// Leftovers of the group are killed when the main process exits by itself.
#[tokio::test]
async fn orphans_are_killed_when_the_leader_exits() {
    let dir = tempfile::tempdir().expect("tempdir");
    let script = dir.path().join("leaver");
    let pidfile = dir.path().join("orphan.pid");
    std::fs::write(
        &script,
        "sleep 300 &\necho $! > \"$ORPHAN_PIDFILE\"\nexit 3\n",
    )
    .expect("write script");
    let mut spec = LaunchSpec::new("/bin/sh".into(), script, dir.path().to_path_buf());
    spec.env = EnvVars(BTreeMap::from([(
        "ORPHAN_PIDFILE".to_owned(),
        pidfile.display().to_string(),
    )]));
    let reg = PortRegistry::new();
    let events = Events::default();
    let sup = start(spec, alloc(&reg, "main", 37_800), &events);
    let status = wait_terminal(&sup).await;
    assert!(
        matches!(&status, Status::Crashed { exit } if exit.code == Some(3)),
        "{status:?}"
    );
    let orphan: u32 = std::fs::read_to_string(&pidfile)
        .expect("pidfile")
        .trim()
        .parse()
        .expect("pid");
    wait_gone(|| test_kill_process(pid(orphan)).is_err(), "orphan").await;
}

#[tokio::test]
async fn dropping_the_handle_kills_the_group_and_releases_ports() {
    let dir = tempfile::tempdir().expect("tempdir");
    let reg = PortRegistry::new();
    let events = Events::default();
    let sup = start(
        fake_spec(dir.path(), &[]),
        alloc(&reg, "main", 38_000),
        &events,
    );
    let raw = wait_running(&sup, &events).await;
    drop(sup);
    wait_gone(
        || test_kill_process_group(pid(raw)).is_err(),
        "process group",
    )
    .await;
    wait_gone(|| reg.reserved().is_empty(), "reservation").await;
}

#[tokio::test]
async fn spawn_failures_are_reported() {
    let dir = tempfile::tempdir().expect("tempdir");
    let reg = PortRegistry::new();

    // At start: an error, and the ports are released.
    let mut spec = fake_spec(dir.path(), &[]);
    spec.python = dir.path().join("missing-python");
    let err = supervisor::start(
        spec,
        alloc(&reg, "main", 38_200),
        SuperviseOptions::default(),
        |_| {},
    )
    .expect_err("spawn");
    assert!(matches!(err, SupervisorError::Spawn { .. }), "{err:?}");
    assert!(reg.reserved().is_empty());

    // On restart: `failed_to_start`, and a later restart can recover.
    let link = dir.path().join("python");
    std::os::unix::fs::symlink(python3(), &link).expect("symlink");
    let mut spec = fake_spec(dir.path(), &[]);
    spec.python = link.clone();
    let events = Events::default();
    let sup = start(spec, alloc(&reg, "main", 38_200), &events);
    wait_running(&sup, &events).await;
    std::fs::remove_file(&link).expect("remove link");
    let err = sup.restart().await.expect_err("restart");
    assert!(matches!(err, SupervisorError::Restart(_)), "{err:?}");
    assert!(matches!(sup.status(), Status::FailedToStart { .. }));
    std::os::unix::fs::symlink(python3(), &link).expect("symlink");
    let pid = sup.restart().await.expect("restart");
    assert_eq!(wait_running(&sup, &events).await, pid);
    sup.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn invalid_specs_are_rejected_before_spawning() {
    let dir = tempfile::tempdir().expect("tempdir");
    let reg = PortRegistry::new();
    let spec = fake_spec(dir.path(), &["--http-port=9999"]);
    let err = supervisor::start(
        spec,
        alloc(&reg, "main", 38_400),
        SuperviseOptions::default(),
        |_| {},
    )
    .expect_err("managed option");
    assert!(matches!(err, SupervisorError::ManagedOption(_)));
    assert!(reg.reserved().is_empty());
}
