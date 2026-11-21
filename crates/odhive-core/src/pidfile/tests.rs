use std::os::unix::fs::{PermissionsExt, symlink};
use std::process::Stdio;

use rustix::process::test_kill_process;
use tokio::io::AsyncBufReadExt;

use super::*;

fn me() -> ProcessId {
    ProcessId::current().expect("current process")
}

fn record(project: &str, instance: &str, process: ProcessId) -> PidRecord {
    PidRecord {
        version: RECORD_VERSION,
        project: project.into(),
        instance: instance.into(),
        process,
        http_port: 39_600,
        gevent_port: 39_650,
        db: Some("devdb".into()),
        odoo_bin: "/src/odoo-bin".into(),
        supervisor: None,
    }
}

/// A process that no longer exists: a child that exited and was reaped.
async fn dead_process() -> ProcessId {
    // Blocks on stdin, so it surely runs while its identity is read.
    let mut child = tokio::process::Command::new("/bin/sh")
        .args(["-c", "read x"])
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn");
    let id = ProcessId::of(child.id().expect("pid")).expect("identity");
    drop(child.stdin.take());
    child.wait().await.expect("wait");
    assert!(!id.is_alive());
    id
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).expect("metadata").permissions().mode() & 0o777
}

#[test]
fn current_process_is_alive_and_a_wrong_start_time_is_not() {
    let me = me();
    assert!(me.is_alive());
    assert!(me.owns_group());
    let reused = ProcessId {
        start_time: me.start_time + 7,
        ..me
    };
    assert!(!reused.is_alive());
    assert!(!reused.owns_group());
}

#[test]
fn write_read_remove_round_trip_with_private_modes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let run = RunDir::new(dir.path().join("run"));
    let rec = record("shop", "main", me());
    run.write(&rec).expect("write");
    let file = run.path().join("shop.main.json");
    assert_eq!(run.file_for(&rec.owner()).expect("file"), file);
    assert_eq!(mode(&file), 0o600);
    assert_eq!(mode(run.path()), 0o700);
    assert_eq!(run.read(&rec.owner()).expect("read"), Some(rec.clone()));

    // A different process (a restart) is not removed by the old one's cleanup.
    let other = ProcessId {
        start_time: rec.process.start_time + 1,
        ..rec.process
    };
    assert!(!run.remove(&rec.owner(), other).expect("remove other"));
    assert!(file.exists());
    assert!(run.remove(&rec.owner(), rec.process).expect("remove"));
    assert!(!file.exists());
    assert!(!run.remove(&rec.owner(), rec.process).expect("remove again"));
}

#[test]
fn validation_rejects_dangerous_or_malformed_records() {
    let ok = record("shop", "main", me());
    ok.validate().expect("valid");
    let cases: Vec<(&str, PidRecord)> = vec![
        ("pid 0", {
            let mut r = ok.clone();
            r.process.pid = 0;
            r
        }),
        ("pid 1", {
            let mut r = ok.clone();
            r.process.pid = 1;
            r
        }),
        ("negative as i32", {
            let mut r = ok.clone();
            r.process.pid = u32::MAX;
            r
        }),
        ("supervisor pid 1", {
            let mut r = ok.clone();
            r.supervisor = Some(ProcessId {
                pid: 1,
                start_time: 0,
            });
            r
        }),
        ("newer version", {
            let mut r = ok.clone();
            r.version = RECORD_VERSION + 1;
            r
        }),
        ("bad project", {
            let mut r = ok.clone();
            r.project = "../etc".into();
            r
        }),
        ("dotted instance", {
            let mut r = ok.clone();
            r.instance = "a.b".into();
            r
        }),
        ("bad db", {
            let mut r = ok.clone();
            r.db = Some("x; DROP".into());
            r
        }),
        ("privileged port", {
            let mut r = ok.clone();
            r.http_port = 80;
            r
        }),
        ("same ports", {
            let mut r = ok.clone();
            r.gevent_port = r.http_port;
            r
        }),
    ];
    let dir = tempfile::tempdir().expect("tempdir");
    let run = RunDir::new(dir.path());
    for (what, r) in cases {
        assert!(
            matches!(r.validate(), Err(PidFileError::Invalid(_))),
            "{what}"
        );
        assert!(run.write(&r).is_err(), "{what} written");
    }
    assert!(
        run.file_for(&PortOwner::new("shop", "../x")).is_err(),
        "path in owner"
    );
}

#[test]
fn list_skips_temp_and_foreign_files_and_flags_bad_ones() {
    let dir = tempfile::tempdir().expect("tempdir");
    let run = RunDir::new(dir.path());
    assert!(
        RunDir::new(dir.path().join("missing"))
            .list()
            .expect("missing")
            .is_empty()
    );

    run.write(&record("shop", "main", me())).expect("write");
    fs::write(dir.path().join(".shop.main.json.abc.tmp"), "x").expect("tmp");
    fs::write(dir.path().join("notes.txt"), "x").expect("txt");
    fs::write(dir.path().join("broken.main.json"), "{").expect("broken");
    // Valid content under the wrong name.
    let moved = serde_json::to_string(&record("shop", "test", me())).expect("json");
    fs::write(dir.path().join("other.main.json"), moved).expect("moved");
    fs::write(
        dir.path().join("big.main.json"),
        vec![b' '; usize::try_from(MAX_RECORD_BYTES).expect("size") + 1],
    )
    .expect("big");
    symlink(
        dir.path().join("shop.main.json"),
        dir.path().join("link.main.json"),
    )
    .expect("symlink");

    let listed = run.list().expect("list");
    let names: Vec<String> = listed
        .iter()
        .map(|l| {
            l.path
                .file_name()
                .expect("name")
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(
        names,
        [
            "big.main.json",
            "broken.main.json",
            "link.main.json",
            "other.main.json",
            "shop.main.json"
        ]
    );
    let ok: Vec<bool> = listed.iter().map(|l| l.record.is_ok()).collect();
    assert_eq!(ok, [false, false, false, false, true]);
}

#[tokio::test]
async fn recover_sorts_records_into_stale_supervised_and_invalid() {
    let dir = tempfile::tempdir().expect("tempdir");
    let run = RunDir::new(dir.path());
    let reg = PortRegistry::new();

    let dead = record("shop", "dead", dead_process().await);
    run.write(&dead).expect("write dead");
    // Our own pid with another start time: a reused pid. Must never be signalled.
    let reused = record(
        "shop",
        "reused",
        ProcessId {
            start_time: me().start_time + 3,
            ..me()
        },
    );
    run.write(&reused).expect("write reused");
    let mut owned = record("shop", "owned", me());
    owned.supervisor = Some(me());
    run.write(&owned).expect("write owned");
    fs::write(dir.path().join("junk.main.json"), "not json").expect("junk");

    let found = recover(&run, &reg).expect("recover");
    assert_eq!(found.len(), 4);
    for f in &found {
        match f {
            Recovered::Stale {
                record,
                killed_leftovers,
            } => {
                assert!(["dead", "reused"].contains(&record.instance.as_str()));
                assert!(!killed_leftovers, "{record:?}");
            }
            Recovered::Supervised { record, by } => {
                assert_eq!(record.instance, "owned");
                assert_eq!(*by, me());
            }
            Recovered::Invalid { path, .. } => {
                assert!(path.ends_with("junk.main.json"));
            }
            Recovered::Running { instance, .. } => panic!("{:?}", instance.record()),
        }
    }
    let left: Vec<_> = run
        .list()
        .expect("list")
        .into_iter()
        .map(|l| l.record.expect("valid").instance)
        .collect();
    assert_eq!(left, ["owned"]);
    assert!(reg.reserved().is_empty());
    assert!(me().is_alive());
}

/// A leader that is gone with members of its group still alive: the members
/// are killed.
#[tokio::test]
async fn stale_leader_with_live_group_members_gets_them_killed() {
    let mut child = tokio::process::Command::new("/bin/sh")
        .args(["-c", "sleep 300 & echo $!; read x"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .process_group(0)
        .spawn()
        .expect("spawn");
    let leader = ProcessId::of(child.id().expect("pid")).expect("identity");
    let mut out = tokio::io::BufReader::new(child.stdout.take().expect("stdout")).lines();
    let member: i32 = out
        .next_line()
        .await
        .expect("read")
        .expect("line")
        .parse()
        .expect("pid");
    drop(child.stdin.take());
    child.wait().await.expect("wait");
    let member = Pid::from_raw(member).expect("pid");
    assert!(test_kill_process(member).is_ok(), "member alive");

    let dir = tempfile::tempdir().expect("tempdir");
    let run = RunDir::new(dir.path());
    run.write(&record("shop", "main", leader)).expect("write");
    let found = recover(&run, &PortRegistry::new()).expect("recover");
    assert!(
        matches!(
            found.as_slice(),
            [Recovered::Stale {
                killed_leftovers: true,
                ..
            }]
        ),
        "{found:?}"
    );
    let gone = async {
        while test_kill_process(member).is_ok() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), gone)
        .await
        .expect("group member still alive");
    assert!(run.list().expect("list").is_empty());
}

#[test]
fn record_state_reports_running_supervised_and_stale() {
    let mut r = record("shop", "main", me());
    assert_eq!(
        r.state(),
        RecordState::Running {
            pid: me().pid,
            supervisor: None
        }
    );
    r.supervisor = Some(me());
    assert_eq!(
        r.state(),
        RecordState::Running {
            pid: me().pid,
            supervisor: Some(me().pid)
        }
    );
    // A supervisor whose pid was reused counts as gone.
    r.supervisor = Some(ProcessId {
        start_time: me().start_time + 9,
        ..me()
    });
    assert!(matches!(
        r.state(),
        RecordState::Running {
            supervisor: None,
            ..
        }
    ));
    r.process = ProcessId {
        start_time: me().start_time + 9,
        ..me()
    };
    assert_eq!(r.state(), RecordState::Stale { pid: me().pid });
}

/// The process is a child of this test, like an instance supervised by
/// another OdHive: it is stopped by SIGINT and seen as gone while it is a
/// zombie that only its parent can reap.
#[tokio::test]
async fn stop_recorded_interrupts_a_process_supervised_elsewhere() {
    let mut child = tokio::process::Command::new("sleep")
        .arg("300")
        .process_group(0)
        .spawn()
        .expect("spawn");
    let process = ProcessId::of(child.id().expect("pid")).expect("identity");
    let dir = tempfile::tempdir().expect("tempdir");
    let run = RunDir::new(dir.path());
    let mut r = record("shop", "main", process);
    r.supervisor = Some(me());
    run.write(&r).expect("write");

    let stop = stop_recorded(&run, &r, Duration::from_secs(5))
        .await
        .expect("stop");
    assert_eq!(
        stop,
        AttachedStop {
            already_gone: false,
            killed: false
        }
    );
    let status = child.wait().await.expect("wait");
    assert_eq!(
        std::os::unix::process::ExitStatusExt::signal(&status),
        Some(2)
    );
    assert!(run.list().expect("list").is_empty());
}

#[tokio::test]
async fn stop_recorded_never_signals_a_reused_pid_and_removes_the_record() {
    let dir = tempfile::tempdir().expect("tempdir");
    let run = RunDir::new(dir.path());
    let reused = record(
        "shop",
        "main",
        ProcessId {
            start_time: me().start_time + 5,
            ..me()
        },
    );
    run.write(&reused).expect("write");
    let stop = stop_recorded(&run, &reused, Duration::from_secs(1))
        .await
        .expect("stop");
    assert!(stop.already_gone && !stop.killed);
    assert!(me().is_alive());
    assert!(run.list().expect("list").is_empty());

    let mut bad = reused.clone();
    bad.process.pid = 1;
    assert!(matches!(
        stop_recorded(&run, &bad, Duration::from_secs(1)).await,
        Err(PidFileError::Invalid(_))
    ));
}
