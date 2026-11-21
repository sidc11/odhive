//! PID file integration tests: the supervisor's records, and recovery of an
//! instance left behind by an OdHive process that was killed hard.
//!
//! Ports come from 39000–39500, away from the other test files' ranges.

use std::net::{Ipv4Addr, TcpStream};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use odhive_core::pidfile::{self, PidRecord, ProcessId, RECORD_VERSION, Recovered, RunDir};
use odhive_core::ports::{AllocOptions, PortOwner, PortRegistry};
use odhive_core::python::find_executable;
use odhive_core::supervisor::{self, LaunchSpec, Status, SuperviseOptions, Supervised};
use rustix::process::{Pid, test_kill_process, test_kill_process_group};
use tokio::process::{Child, Command};

const WAIT: Duration = Duration::from_secs(20);

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/fake-odoo-bin")
}

fn python3() -> PathBuf {
    let path = std::env::var_os("PATH").expect("PATH");
    find_executable(&path, "python3").expect("python3 on PATH")
}

fn pid(raw: u32) -> Pid {
    Pid::from_raw(i32::try_from(raw).expect("pid")).expect("pid")
}

fn accepts(port: u16) -> bool {
    TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_ok()
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

async fn wait_running(sup: &Supervised) -> u32 {
    let status = tokio::time::timeout(
        WAIT,
        sup.wait_for(|s| matches!(s, Status::Running { .. }) || s.is_terminal()),
    )
    .await
    .expect("running in time")
    .expect("supervisor");
    status.pid().unwrap_or_else(|| panic!("{status:?}"))
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

/// Starts `program args` the way the supervisor does (own group), but with no
/// pipes, like an instance whose OdHive is gone, and records it.
fn orphan(
    run: &RunDir,
    program: PathBuf,
    args: &[String],
    http: u16,
    supervisor: ProcessId,
) -> (Child, PidRecord) {
    let child = Command::new(program)
        .args(args)
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
        db: Some("devdb".into()),
        odoo_bin: fixture(),
        supervisor: Some(supervisor),
    };
    run.write(&record).expect("write record");
    (child, record)
}

fn attached(found: Vec<Recovered>) -> pidfile::Attached {
    match found.into_iter().next() {
        Some(Recovered::Running {
            instance,
            ports_error: None,
        }) => instance,
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn supervisor_keeps_a_pid_file_while_the_process_lives() {
    let dir = tempfile::tempdir().expect("tempdir");
    let run = RunDir::new(dir.path().join("run"));
    let reg = PortRegistry::new();
    let owner = PortOwner::new("shop", "main");
    let ports = reg
        .allocate(
            owner.clone(),
            None,
            None,
            &AllocOptions {
                http_start: 39_000,
                gevent_start: 39_050,
                scan: 40,
            },
        )
        .expect("allocate");
    let mut spec = LaunchSpec::new(python3(), fixture(), dir.path().to_path_buf());
    spec.db = Some("devdb".into());
    let opts = SuperviseOptions {
        run_dir: Some(run.clone()),
        ..SuperviseOptions::default()
    };
    let sup = supervisor::start(spec, ports, opts, |_| {}).expect("start");
    let first = wait_running(&sup).await;

    let rec = run.read(&owner).expect("read").expect("record");
    assert_eq!(rec.process.pid, first);
    assert_eq!(
        (rec.http_port, rec.gevent_port),
        (sup.http_port(), sup.gevent_port())
    );
    assert_eq!(rec.db.as_deref(), Some("devdb"));
    assert_eq!(rec.supervisor, ProcessId::current());

    // A live supervisor means another OdHive's recovery leaves it alone.
    let found = pidfile::recover(&run, &PortRegistry::new()).expect("recover");
    assert!(
        matches!(found.as_slice(), [Recovered::Supervised { .. }]),
        "{found:?}"
    );

    // Restart: the record follows the new process.
    let second = sup.restart().await.expect("restart");
    wait_running(&sup).await;
    let rec = run.read(&owner).expect("read").expect("record");
    assert_eq!(rec.process.pid, second);

    // Stopped: no process, no record.
    sup.stop().await.expect("stop");
    assert!(run.read(&owner).expect("read").is_none());

    // Dropping the handle removes it too.
    sup.restart().await.expect("restart");
    wait_running(&sup).await;
    assert!(run.read(&owner).expect("read").is_some());
    drop(sup);
    eventually(
        || run.read(&owner).expect("read").is_none(),
        "record removal on drop",
    )
    .await;
}

#[tokio::test]
async fn instance_left_by_a_killed_odhive_is_reattached_and_stopped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let run = RunDir::new(dir.path());
    let http = (39_200..39_240)
        .step_by(2)
        .find(|p| odhive_core::ports::is_free(*p).expect("probe") && !accepts(*p + 1))
        .expect("free port");
    let args = [
        fixture().display().to_string(),
        "--http-port".into(),
        http.to_string(),
        "--gevent-port".into(),
        (http + 1).to_string(),
        "-d".into(),
        "devdb".into(),
    ];
    let (mut child, rec) = orphan(&run, python3(), &args, http, dead_odhive().await);
    eventually(|| accepts(http), "orphan listening").await;

    let reg = PortRegistry::new();
    let inst = attached(pidfile::recover(&run, &reg).expect("recover"));
    assert!(inst.is_alive());
    assert_eq!(inst.record().process, rec.process);
    assert_eq!(reg.owner_of(http), Some(PortOwner::new("shop", "main")));
    // The record now names this OdHive as its supervisor.
    let updated = run.read(&inst.owner()).expect("read").expect("record kept");
    assert_eq!(updated.supervisor, ProcessId::current());

    let stopped = inst.stop(Duration::from_secs(15)).await.expect("stop");
    assert!(!stopped.already_gone && !stopped.killed, "{stopped:?}");
    let status = child.wait().await.expect("reap");
    assert_eq!(status.code(), Some(0), "graceful SIGINT exit");
    assert!(test_kill_process_group(pid(rec.process.pid)).is_err());
    assert!(!accepts(http));
    assert!(run.list().expect("list").is_empty());
    assert!(reg.reserved().is_empty());
}

#[tokio::test]
async fn attached_instance_that_ignores_sigint_is_killed_with_its_group() {
    let dir = tempfile::tempdir().expect("tempdir");
    let run = RunDir::new(dir.path().join("run"));
    let script = dir.path().join("stubborn");
    let member_file = dir.path().join("member.pid");
    std::fs::write(
        &script,
        format!(
            "trap '' INT\nsleep 300 &\necho $! > '{}'\nwait\n",
            member_file.display()
        ),
    )
    .expect("script");
    let (mut child, rec) = orphan(
        &run,
        "/bin/sh".into(),
        &[script.display().to_string()],
        39_400,
        dead_odhive().await,
    );
    eventually(|| member_file.exists(), "group member pid").await;
    let member: u32 = tokio::time::timeout(WAIT, async {
        loop {
            if let Ok(s) = std::fs::read_to_string(&member_file)
                && let Ok(p) = s.trim().parse()
            {
                return p;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("member pid");

    let reg = PortRegistry::new();
    let inst = attached(pidfile::recover(&run, &reg).expect("recover"));
    let stopped = inst.stop(Duration::from_millis(300)).await.expect("stop");
    assert!(stopped.killed, "{stopped:?}");
    child.wait().await.expect("reap");
    eventually(
        || test_kill_process(pid(member)).is_err(),
        "group member exit",
    )
    .await;
    assert!(test_kill_process_group(pid(rec.process.pid)).is_err());
    assert!(run.list().expect("list").is_empty());
}

#[tokio::test]
async fn unwritable_pid_file_fails_the_start_and_releases_the_ports() {
    let dir = tempfile::tempdir().expect("tempdir");
    let blocker = dir.path().join("not-a-dir");
    std::fs::write(&blocker, "x").expect("file");
    let reg = PortRegistry::new();
    let ports = reg
        .reserve(PortOwner::new("shop", "main"), 39_450, 39_451)
        .expect("reserve");
    let opts = SuperviseOptions {
        run_dir: Some(RunDir::new(blocker.join("run"))),
        ..SuperviseOptions::default()
    };
    let spec = LaunchSpec::new(python3(), fixture(), dir.path().to_path_buf());
    let err = supervisor::start(spec, ports, opts, |_| {}).expect_err("pid file");
    assert!(
        matches!(err, supervisor::SupervisorError::PidFile(_)),
        "{err:?}"
    );
    assert!(reg.reserved().is_empty());
    // The process was killed again: nothing listens on its port.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!accepts(39_450));
}
