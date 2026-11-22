//! Manager integration tests against the `fake-odoo-bin` fixture: several
//! instances side by side, the port-race retry, and re-attached instances.
//!
//! Ports come from 27000–29500, away from the other test files' ranges and
//! below the ephemeral range (Linux 32768–60999, macOS 49152–65535): polling
//! a port there with connects can make a socket connect to itself, which then
//! holds the port. Each test scans its own block.

use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use odhive_core::events::{BusOptions, Delivery, LogEvent, Subscription};
use odhive_core::logs::{self, LogSettings};
use odhive_core::manager::{
    InstanceEvent, InstancePlan, Manager, ManagerError, ManagerEvent, ManagerOptions, Notice,
    Recovery, StopOutcome,
};
use odhive_core::pidfile::{PidRecord, ProcessId, RECORD_VERSION, RunDir};
use odhive_core::ports::{AllocOptions, PortOwner};
use odhive_core::project::{Instance, ProjectConfig};
use odhive_core::python::find_executable;
use odhive_core::supervisor::{Event, Status};
use odhive_core::syslib::Library;
use rustix::process::{Pid, test_kill_process};
use tokio::process::{Child, Command};

const WAIT: Duration = Duration::from_secs(20);

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/fake-odoo-bin")
}

fn python3() -> PathBuf {
    let path = std::env::var_os("PATH").expect("PATH");
    find_executable(&path, "python3").expect("python3 on PATH")
}

fn options(base: u16) -> ManagerOptions {
    ManagerOptions {
        alloc: AllocOptions {
            http_start: base,
            gevent_start: base + 50,
            scan: 40,
        },
        stop_timeout: Duration::from_secs(5),
        attached_poll: Duration::from_millis(100),
        ..ManagerOptions::default()
    }
}

/// A project `shop` with instances `main` and `test`.
fn project() -> ProjectConfig {
    let mut p = ProjectConfig::new("shop", "odoo-19.0");
    let mut test = Instance::new("test");
    test.db = Some("shop_tests".into());
    test.extra_args = vec!["--odoo-version".into(), "20.0".into()];
    p.instances.push(test);
    p
}

fn plan_for(p: &ProjectConfig, dir: &Path, id: &str) -> InstancePlan {
    InstancePlan::for_instance(p, dir, id, python3(), fixture(), None).expect("plan")
}

fn plan(dir: &Path, id: &str, extra: &[&str]) -> InstancePlan {
    let mut p = project();
    for i in &mut p.instances {
        i.extra_args.extend(extra.iter().map(|s| (*s).to_owned()));
    }
    plan_for(&p, dir, id)
}

fn owner(id: &str) -> PortOwner {
    PortOwner::new("shop", id)
}

async fn wait_status(m: &Manager, id: &str, f: impl FnMut(&Status) -> bool) -> Status {
    tokio::time::timeout(WAIT, m.wait_for(&owner(id), f))
        .await
        .unwrap_or_else(|_| panic!("{id}: no matching status in time"))
        .expect("known instance")
}

async fn wait_running(m: &Manager, id: &str) -> u32 {
    let status = wait_status(m, id, |s| {
        matches!(s, Status::Running { .. }) || s.is_terminal()
    })
    .await;
    match status {
        Status::Running { pid } => pid,
        other => panic!("{id}: {other:?}"),
    }
}

async fn eventually(check: impl Fn() -> bool, what: &str) {
    let done = async {
        while !check() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(WAIT, done)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

fn drain<T: Clone>(rx: &mut Subscription<T>) -> Vec<T> {
    rx.drain()
}

fn output_of(events: &[LogEvent], id: &str) -> String {
    events
        .iter()
        .filter(|e| e.owner.instance == id)
        .map(|e| e.record.entry.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Collects log events until `id`'s output contains `needle`.
async fn logs_until(rx: &mut Subscription<LogEvent>, id: &str, needle: &str) -> Vec<LogEvent> {
    let mut events = Vec::new();
    let found = tokio::time::timeout(WAIT, async {
        while !output_of(&events, id).contains(needle) {
            match rx.recv().await.expect("bus open") {
                Delivery::Event(e) => events.push(e),
                Delivery::Lagged { missed } => panic!("lagged by {missed}"),
            }
        }
    })
    .await;
    assert!(
        found.is_ok(),
        "no {needle:?} from {id}:\n{}",
        output_of(&events, id)
    );
    events
}

fn spawned(events: &[ManagerEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e.event, InstanceEvent::Supervisor(Event::Spawned { .. })))
        .count()
}

fn retries(events: &[ManagerEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e.event, InstanceEvent::Manager(Notice::PortRetry { .. })))
        .count()
}

fn accepts(port: u16) -> bool {
    TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_ok()
}

fn alive(pid: u32) -> bool {
    let pid = Pid::from_raw(i32::try_from(pid).expect("pid")).expect("pid");
    test_kill_process(pid).is_ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn instances_of_a_project_run_with_their_own_ports_db_and_args() {
    let dir = tempfile::tempdir().expect("tempdir");
    let m = Manager::new(options(27_000));
    let mut rx = m.subscribe_logs();
    let p = project();
    let main = m
        .start(plan_for(&p, dir.path(), "main"))
        .await
        .expect("main");
    let test = m
        .start(plan_for(&p, dir.path(), "test"))
        .await
        .expect("test");
    let (pid_main, pid_test) = (
        wait_running(&m, "main").await,
        wait_running(&m, "test").await,
    );
    assert_ne!(pid_main, pid_test);

    let mut ports = vec![
        main.http_port.expect("port"),
        main.gevent_port.expect("port"),
        test.http_port.expect("port"),
        test.gevent_port.expect("port"),
    ];
    ports.sort_unstable();
    ports.dedup();
    assert_eq!(ports.len(), 4);
    assert!(accepts(main.http_port.expect("port")) && accepts(test.http_port.expect("port")));

    let list = m.list();
    assert_eq!(list.len(), 2);
    assert!(list.iter().all(|i| i.supervised && !i.port_retried));
    assert_eq!(list[0].db.as_deref(), Some("shop_main"));
    assert_eq!(list[1].db.as_deref(), Some("shop_tests"));
    assert_eq!(m.list_project("shop").len(), 2);
    assert!(m.list_project("other").is_empty());

    // The fixture logs the registry line after it binds the port, so keep
    // collecting output until both instances have printed it.
    let mut events = Vec::new();
    let collected = tokio::time::timeout(WAIT, async {
        while !(output_of(&events, "main").contains("odoo.registry")
            && output_of(&events, "test").contains("odoo.registry"))
        {
            events.extend(rx.next_event().await);
        }
    })
    .await;
    let (out_main, out_test) = (output_of(&events, "main"), output_of(&events, "test"));
    assert!(collected.is_ok(), "main:\n{out_main}\ntest:\n{out_test}");
    assert!(out_main.contains("Odoo version 19.0"), "{out_main}");
    assert!(out_test.contains("Odoo version 20.0"), "{out_test}");
    assert!(out_main.contains(" shop_main odoo.registry"), "{out_main}");
    assert!(out_test.contains(" shop_tests odoo.registry"), "{out_test}");

    // Stopping one leaves the other running.
    let stopped = m.stop(&owner("main")).await.expect("stop");
    assert!(matches!(
        stopped,
        StopOutcome::Supervised {
            status: Status::Stopped { .. }
        }
    ));
    assert!(!alive(pid_main));
    assert!(m.get(&owner("main")).is_none());
    assert!(matches!(
        m.get(&owner("test")).expect("test").status,
        Status::Running { .. }
    ));

    let results = m.shutdown().await;
    assert_eq!(results.len(), 1);
    assert!(results[0].1.is_ok());
    assert!(!alive(pid_test));
    assert!(m.list().is_empty());
    assert!(m.registry().reserved().is_empty());
}

#[tokio::test]
async fn a_running_instance_or_its_database_cannot_be_used_twice() {
    let dir = tempfile::tempdir().expect("tempdir");
    let m = Manager::new(options(27_300));
    m.start(plan(dir.path(), "main", &[])).await.expect("main");
    wait_running(&m, "main").await;

    assert!(matches!(
        m.start(plan(dir.path(), "main", &[])).await,
        Err(ManagerError::AlreadyRunning(o)) if o == owner("main")
    ));

    let mut other = ProjectConfig::new("blog", "odoo-19.0");
    other.instances[0].db = Some("shop_main".into());
    let err = m
        .start(plan_for(&other, dir.path(), "main"))
        .await
        .expect_err("same database");
    assert!(
        matches!(&err, ManagerError::DatabaseInUse { db, by } if db == "shop_main" && *by == owner("main")),
        "{err}"
    );
    assert_eq!(m.list().len(), 1);
    m.shutdown().await;
    assert!(m.registry().reserved().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lost_port_race_is_retried_once_on_new_ports() {
    let dir = tempfile::tempdir().expect("tempdir");
    let marker = dir.path().join("failed-once");
    let marker_arg = marker.to_str().expect("utf-8");
    let m = Manager::new(options(27_600));
    let mut rx = m.subscribe();
    m.start(plan(
        dir.path(),
        "main",
        &["--simulate", "port-in-use", "--marker", marker_arg],
    ))
    .await
    .expect("start");
    // The crash of the first process is not reported as the status.
    let status = wait_status(&m, "main", |s| {
        matches!(s, Status::Running { .. }) || s.is_terminal()
    })
    .await;
    assert!(matches!(status, Status::Running { .. }), "{status:?}");
    assert!(marker.exists());

    let info = m.get(&owner("main")).expect("info");
    assert!(info.port_retried);
    let events = drain(&mut rx);
    assert_eq!(spawned(&events), 2);
    assert_eq!(retries(&events), 1);
    assert!(!events.iter().any(|e| matches!(
        &e.event,
        InstanceEvent::Supervisor(Event::Status {
            status: Status::Crashed { .. }
        })
    )));
    let retry = events
        .iter()
        .find_map(|e| match &e.event {
            InstanceEvent::Manager(Notice::PortRetry { exit, .. }) => Some(exit.clone()),
            _ => None,
        })
        .expect("retry notice");
    assert!(retry.port_in_use && retry.code == Some(1));
    m.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_port_race_retry_happens_only_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let m = Manager::new(options(27_900));
    let mut rx = m.subscribe();
    m.start(plan(dir.path(), "main", &["--simulate", "port-in-use"]))
        .await
        .expect("start");
    let status = wait_status(
        &m,
        "main",
        |s| matches!(s, Status::Crashed { exit } if exit.port_in_use),
    )
    .await;
    assert!(status.is_terminal());
    let info = m.get(&owner("main")).expect("still listed");
    assert!(info.port_retried && info.supervised);
    let events = drain(&mut rx);
    assert_eq!(spawned(&events), 2);
    assert_eq!(retries(&events), 1);

    // A crashed instance is listed until it is stopped; that frees its ports.
    assert!(matches!(
        m.stop(&owner("main")).await.expect("stop"),
        StopOutcome::Supervised {
            status: Status::Crashed { .. }
        }
    ));
    assert!(m.list().is_empty());
    assert!(m.registry().reserved().is_empty());
}

#[tokio::test]
async fn pinned_ports_are_not_retried() {
    let dir = tempfile::tempdir().expect("tempdir");
    let free = (28_200..28_280)
        .find(|p| TcpListener::bind((Ipv4Addr::LOCALHOST, *p)).is_ok())
        .expect("a free port");
    let mut p = project();
    p.instances[0].http_port = Some(free);
    p.instances[0].extra_args = vec!["--simulate".into(), "port-in-use".into()];
    let m = Manager::new(options(28_300));
    let mut rx = m.subscribe();
    let info = m
        .start(plan_for(&p, dir.path(), "main"))
        .await
        .expect("start");
    assert_eq!(info.http_port, Some(free));
    let status = wait_status(&m, "main", Status::is_terminal).await;
    assert!(
        matches!(&status, Status::Crashed { exit } if exit.port_in_use),
        "{status:?}"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    let events = drain(&mut rx);
    assert_eq!(spawned(&events), 1);
    assert_eq!(retries(&events), 0);
    assert!(!m.get(&owner("main")).expect("listed").port_retried);
    m.shutdown().await;
}

#[tokio::test]
async fn restart_keeps_ports_and_replace_applies_new_settings() {
    let dir = tempfile::tempdir().expect("tempdir");
    let m = Manager::new(options(28_600));
    let mut rx = m.subscribe_logs();
    let first = m.start(plan(dir.path(), "main", &[])).await.expect("start");
    let pid = wait_running(&m, "main").await;

    let restarted = m.restart(&owner("main")).await.expect("restart");
    assert_eq!(restarted.http_port, first.http_port);
    let new_pid = wait_running(&m, "main").await;
    assert_ne!(pid, new_pid);
    assert!(!alive(pid));

    drain(&mut rx);
    let replaced = m
        .replace(plan(dir.path(), "main", &["--odoo-version", "20.0"]))
        .await
        .expect("replace");
    assert!(replaced.supervised);
    let newest = wait_running(&m, "main").await;
    assert_ne!(newest, new_pid);
    assert!(!alive(new_pid));
    logs_until(&mut rx, "main", "Odoo version 20.0").await;
    m.shutdown().await;
    assert!(m.registry().reserved().is_empty());
}

#[tokio::test]
async fn a_crashed_instance_can_be_started_again() {
    let dir = tempfile::tempdir().expect("tempdir");
    let m = Manager::new(options(28_900));
    m.start(plan(dir.path(), "main", &["--simulate", "crash"]))
        .await
        .expect("start");
    let status = wait_status(&m, "main", Status::is_terminal).await;
    assert!(
        matches!(&status, Status::Crashed { exit } if exit.code == Some(1)),
        "{status:?}"
    );
    // The new run replaces the crashed one; waiters follow the new run.
    let mut rx = m.watch(&owner("main")).expect("watch");
    m.start(plan(dir.path(), "main", &[])).await.expect("again");
    tokio::time::timeout(WAIT, rx.wait_for(|s| matches!(s, Status::Running { .. })))
        .await
        .expect("running in time")
        .expect("same status stream");
    m.shutdown().await;
}

#[tokio::test]
async fn supervised_instances_keep_pid_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let run = RunDir::new(dir.path().join("run"));
    let m = Manager::new(ManagerOptions {
        run_dir: Some(run.clone()),
        ..options(29_100)
    });
    m.start(plan(dir.path(), "main", &[])).await.expect("start");
    let pid = wait_running(&m, "main").await;
    let record = run.read(&owner("main")).expect("read").expect("record");
    assert_eq!(record.process.pid, pid);
    assert_eq!(record.db.as_deref(), Some("shop_main"));
    assert_eq!(record.supervisor, ProcessId::current());
    // This OdHive is alive, so a second recovery leaves the instance alone.
    let found = m.recover().expect("recover");
    assert!(
        matches!(found.as_slice(), [Recovery::SupervisedElsewhere { pid: p, .. }] if *p == pid),
        "{found:?}"
    );
    m.stop(&owner("main")).await.expect("stop");
    assert!(run.read(&owner("main")).expect("read").is_none());
}

/// The OdHive process that "died": a reaped child, so the pid is free.
async fn dead_odhive() -> ProcessId {
    // Blocks on stdin, so it surely runs while its identity is read.
    let mut child = Command::new("/bin/sh")
        .args(["-c", "read x"])
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn");
    let id = ProcessId::of(child.id().expect("pid")).expect("identity");
    drop(child.stdin.take());
    child.wait().await.expect("wait");
    id
}

/// Starts the fixture like the supervisor does (own group) but without
/// pipes, like an instance whose OdHive was killed hard, and records it.
async fn orphan(run: &RunDir, http: u16, extra: &[&str]) -> (Child, PidRecord) {
    let child = Command::new(python3())
        .arg(fixture())
        .args(["--http-port", &http.to_string(), "-d", "shop_main"])
        .args(extra)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .expect("spawn");
    let process = ProcessId::of(child.id().expect("pid")).expect("identity");
    let record = PidRecord {
        version: RECORD_VERSION,
        project: "shop".into(),
        instance: "main".into(),
        process,
        http_port: http,
        gevent_port: http + 1,
        db: Some("shop_main".into()),
        odoo_bin: fixture(),
        supervisor: Some(dead_odhive().await),
    };
    run.write(&record).expect("write record");
    eventually(|| accepts(http), "orphan listening").await;
    (child, record)
}

fn recovering_manager(dir: &Path, base: u16) -> (Manager, RunDir) {
    let run = RunDir::new(dir.join("run"));
    let m = Manager::new(ManagerOptions {
        run_dir: Some(run.clone()),
        ..options(base)
    });
    (m, run)
}

fn attached_pid(found: &[Recovery]) -> u32 {
    match found {
        [
            Recovery::Attached {
                pid,
                ports_error: None,
                ..
            },
        ] => *pid,
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn recovered_instances_are_listed_and_can_be_stopped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (m, run) = recovering_manager(dir.path(), 29_200);
    let (mut child, record) = orphan(&run, 29_295, &[]).await;
    let pid = attached_pid(&m.recover().expect("recover"));
    assert_eq!(pid, record.process.pid);

    let info = m.get(&owner("main")).expect("listed");
    assert!(!info.supervised);
    assert_eq!(info.status, Status::Running { pid });
    assert_eq!(info.http_port, Some(29_295));
    assert_eq!(m.registry().owner_of(29_295), Some(owner("main")));
    assert!(matches!(
        m.restart(&owner("main")).await,
        Err(ManagerError::NoPlan(_))
    ));
    assert!(matches!(
        m.start(plan(dir.path(), "main", &[])).await,
        Err(ManagerError::AlreadyRunning(_))
    ));
    // Its database counts as in use.
    let mut other = ProjectConfig::new("blog", "odoo-19.0");
    other.instances[0].db = Some("shop_main".into());
    assert!(matches!(
        m.start(plan_for(&other, dir.path(), "main")).await,
        Err(ManagerError::DatabaseInUse { .. })
    ));

    let outcome = m.stop(&owner("main")).await.expect("stop");
    assert!(matches!(
        outcome,
        StopOutcome::Attached { stop } if !stop.already_gone && !stop.killed
    ));
    child.wait().await.expect("reap");
    assert!(m.list().is_empty());
    assert!(m.registry().reserved().is_empty());
    assert!(run.read(&owner("main")).expect("read").is_none());
}

#[tokio::test]
async fn an_attached_instance_that_exits_is_forgotten() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (m, run) = recovering_manager(dir.path(), 29_300);
    let (mut child, _) = orphan(&run, 29_395, &["--exit-after", "1"]).await;
    attached_pid(&m.recover().expect("recover"));
    let status = wait_status(&m, "main", |s| matches!(s, Status::Stopped { .. })).await;
    assert!(matches!(status, Status::Stopped { exit } if exit.code.is_none()));
    child.wait().await.expect("reap");
    eventually(|| m.list().is_empty(), "forgotten").await;
    assert!(m.registry().reserved().is_empty());
    assert!(run.read(&owner("main")).expect("read").is_none());
}

#[tokio::test]
async fn replace_takes_an_attached_instance_under_supervision() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (m, run) = recovering_manager(dir.path(), 29_400);
    let (mut child, record) = orphan(&run, 29_495, &[]).await;
    attached_pid(&m.recover().expect("recover"));
    let mut rx = m.subscribe_logs();

    let info = m
        .replace(plan(dir.path(), "main", &[]))
        .await
        .expect("replace");
    assert!(info.supervised);
    child.wait().await.expect("reap");
    let pid = wait_running(&m, "main").await;
    assert_ne!(pid, record.process.pid);
    let new_record = run.read(&owner("main")).expect("read").expect("record");
    assert_eq!(new_record.process.pid, pid);

    // Supervised again: its output is captured.
    logs_until(&mut rx, "main", "Odoo version").await;
    m.shutdown().await;
    assert!(run.read(&owner("main")).expect("read").is_none());
}

fn log_file(dir: &Path, id: &str) -> String {
    let path = logs::log_path(&dir.join(logs::LOGS_DIR), id).expect("path");
    std::fs::read_to_string(path).expect("log file")
}

#[tokio::test]
async fn output_goes_to_the_log_buffer_file_and_bus() {
    let dir = tempfile::tempdir().expect("tempdir");
    let m = Manager::new(options(28_000));
    let mut rx = m.subscribe_logs();
    m.start(plan(dir.path(), "main", &["--simulate", "traceback"]))
        .await
        .expect("start");

    // The traceback is the last output: it is completed once output pauses.
    let events = logs_until(&mut rx, "main", "ValueError").await;
    let tb = events.last().expect("traceback entry");
    let entry = &tb.record.entry;
    assert_eq!(entry.summary(), "Exception during request handling.");
    assert_eq!(
        entry.exceptions,
        ["ValueError: Expected singleton: res.partner(1, 2)"]
    );
    assert!(
        entry
            .locations
            .iter()
            .any(|l| l.path.ends_with("models/sale_order.py") && l.line == 57)
    );

    // The bus delivered what the buffer holds, in order, from seq 1.
    let seqs: Vec<u64> = events.iter().map(|e| e.record.seq).collect();
    assert_eq!(seqs, (1..=seqs.len() as u64).collect::<Vec<_>>());
    let page = m.read_log(&owner("main"), 1, 10_000).expect("log");
    assert_eq!(page.skipped, 0);
    assert_eq!(page.records.len(), events.len());
    assert!(
        page.records
            .iter()
            .zip(&events)
            .all(|(a, b)| std::sync::Arc::ptr_eq(a, &b.record))
    );
    assert!(
        page.records[0]
            .entry
            .message
            .starts_with("----- odhive: started pid")
    );
    assert_eq!(
        m.tail_log(&owner("main"), 1).expect("log")[0].seq,
        *seqs.last().expect("seq")
    );
    assert_eq!(m.log_next_seq(&owner("main")), Some(seqs.len() as u64 + 1));

    let file = log_file(dir.path(), "main");
    assert!(file.starts_with("----- odhive: started pid"), "{file}");
    assert!(
        file.contains("Traceback (most recent call last):"),
        "{file}"
    );
    assert!(file.contains("ValueError: Expected singleton"), "{file}");

    m.shutdown().await;
    assert!(
        m.read_log(&owner("main"), 1, 10).is_none(),
        "gone once stopped"
    );
}

#[tokio::test]
async fn the_log_survives_restarts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let m = Manager::new(options(28_100));
    let mut rx = m.subscribe_logs();
    m.start(plan(dir.path(), "main", &[])).await.expect("start");
    logs_until(&mut rx, "main", "odoo.registry").await;
    let first_run = m.log_next_seq(&owner("main")).expect("log");

    m.restart(&owner("main")).await.expect("restart");
    wait_running(&m, "main").await;
    let events = logs_until(&mut rx, "main", "odoo.registry").await;
    assert!(events.iter().all(|e| e.record.seq >= first_run));

    let page = m.read_log(&owner("main"), 1, 10_000).expect("log");
    let markers = page
        .records
        .iter()
        .filter(|r| r.entry.message.starts_with("----- odhive: started pid"))
        .count();
    assert_eq!(markers, 2, "one buffer across both runs");
    let file = log_file(dir.path(), "main");
    assert_eq!(file.matches("----- odhive: started pid").count(), 2);
    m.shutdown().await;
}

#[tokio::test]
async fn a_lagging_log_subscriber_catches_up_from_the_buffer() {
    let dir = tempfile::tempdir().expect("tempdir");
    let m = Manager::new(ManagerOptions {
        bus: BusOptions {
            log_capacity: 2,
            ..BusOptions::default()
        },
        ..options(28_200)
    });
    let mut slow = m.subscribe_logs();
    let mut instances = m.subscribe();
    m.start(plan(dir.path(), "main", &[])).await.expect("start");
    wait_running(&m, "main").await;
    eventually(
        || {
            m.tail_log(&owner("main"), 1).is_some_and(|t| {
                t.first()
                    .is_some_and(|r| r.entry.message.starts_with("Registry"))
            })
        },
        "the registry line",
    )
    .await;

    let Some(Delivery::Lagged { missed }) = slow.try_recv().ok() else {
        panic!("expected a lag notice");
    };
    let held: Vec<LogEvent> = slow.drain();
    assert_eq!(held.len(), 2);
    let first_held = held[0].record.seq;
    assert_eq!(missed, first_held - 1);

    // Everything missed is still in the buffer.
    let page = m
        .read_log(&owner("main"), 1, first_held as usize - 1)
        .expect("log");
    assert_eq!(page.skipped, 0);
    assert_eq!(page.records.len() as u64, missed);
    assert_eq!(page.next, first_held);

    // The log flood did not cost the instance topic anything.
    let events = drain(&mut instances);
    assert_eq!(instances.missed(), 0);
    assert!(events.iter().any(|e| matches!(
        e.event,
        InstanceEvent::Supervisor(Event::Status {
            status: Status::Running { .. }
        })
    )));
    assert!(
        events
            .iter()
            .all(|e| !matches!(e.event, InstanceEvent::Supervisor(Event::Output { .. }))),
        "output lines go to the log topic only"
    );
    m.shutdown().await;
}

#[tokio::test]
async fn an_unwritable_log_dir_keeps_the_log_in_memory() {
    let dir = tempfile::tempdir().expect("tempdir");
    let not_a_dir = dir.path().join("logs-file");
    std::fs::write(&not_a_dir, "").expect("write");
    let m = Manager::new(options(29_000));
    let mut instances = m.subscribe();
    let mut rx = m.subscribe_logs();
    let mut p = plan(dir.path(), "main", &[]);
    p.logs_dir = Some(not_a_dir.join("sub"));
    p.log_settings = LogSettings::default();
    m.start(p).await.expect("start");

    logs_until(&mut rx, "main", "odoo.registry").await;
    let notices: Vec<_> = drain(&mut instances)
        .into_iter()
        .filter_map(|e| match e.event {
            InstanceEvent::Manager(Notice::LogFile { error }) => Some(error),
            _ => None,
        })
        .collect();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(
        m.read_log(&owner("main"), 1, 10)
            .is_some_and(|p| !p.records.is_empty())
    );
    m.shutdown().await;
}

#[tokio::test]
async fn a_missing_wkhtmltopdf_is_reported_once_with_a_hint() {
    let dir = tempfile::tempdir().expect("tempdir");
    let m = Manager::new(options(28_450));
    let mut instances = m.subscribe();
    let mut rx = m.subscribe_logs();
    m.start(plan(dir.path(), "main", &["--simulate", "no-wkhtmltopdf"]))
        .await
        .expect("start");
    wait_running(&m, "main").await;
    logs_until(&mut rx, "main", "You need Wkhtmltopdf").await;

    // The restarted process logs it again; the log (and its report) lives on.
    m.restart(&owner("main")).await.expect("restart");
    wait_running(&m, "main").await;
    // The notice is published before its entry reaches the log topic.
    logs_until(&mut rx, "main", "You need Wkhtmltopdf").await;

    let hints: Vec<_> = drain(&mut instances)
        .into_iter()
        .filter_map(|e| match e.event {
            InstanceEvent::Manager(Notice::SystemLibraryMissing { hint }) => Some(hint),
            _ => None,
        })
        .collect();
    assert_eq!(hints.len(), 1, "{hints:?}");
    assert_eq!(hints[0].library, Library::Wkhtmltopdf);
    assert!(
        hints[0].to_string().contains("wkhtmltopdf/packaging"),
        "{}",
        hints[0]
    );
    m.shutdown().await;
}
