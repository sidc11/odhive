//! Dependency auto-fix against the `fake-odoo-bin` fixture: a real (bare)
//! venv, and a fake `uv` that "installs" a package by writing an empty
//! module of that name into the venv's site-packages, so the next start of
//! `fake-odoo-bin --simulate-missing <name>` finds it. No network.
//!
//! Ports come from 30000–31500 (see tests/manager.rs for why below the
//! ephemeral range); each test scans its own block.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use odhive_core::autofix::{AutofixMode, GiveUp, Trust, Trusted};
use odhive_core::events::{Delivery, Subscription};
use odhive_core::manager::{
    AutoFix, FixAction, InstanceEvent, InstancePlan, Manager, ManagerError, ManagerEvent,
    ManagerOptions, Notice,
};
use odhive_core::ports::{AllocOptions, PortOwner};
use odhive_core::project::ProjectConfig;
use odhive_core::python::find_executable;
use odhive_core::requirements::InstallOptions;
use odhive_core::supervisor::{Event, Status};
use odhive_core::venv::VenvTool;

const WAIT: Duration = Duration::from_secs(30);

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
        ..ManagerOptions::default()
    }
}

/// A venv without pip (fast to create); returns its interpreter.
fn bare_venv(dir: &Path) -> PathBuf {
    let venv = dir.join(".venv");
    let st = std::process::Command::new(python3())
        .args(["-I", "-m", "venv", "--without-pip"])
        .arg(&venv)
        .status()
        .expect("venv");
    assert!(st.success());
    venv.join("bin/python")
}

/// A fake uv. Each call appends its requirements to `<dir>/uv.calls`, runs
/// `before`, then (if `install`) writes `<name>.py` per requirement into the
/// target venv's site-packages.
fn fake_uv(dir: &Path, before: &str, install: bool) -> PathBuf {
    let path = dir.join("uv");
    let calls = dir.join("uv.calls");
    let body = format!(
        r#"#!/bin/sh
py=; req=; prev=
for a in "$@"; do
  [ "$prev" = "--python" ] && py="$a"
  [ "$prev" = "-r" ] && req="$a"
  prev="$a"
done
cat "$req" >> '{calls}'
{before}
if [ {install} = 1 ]; then
  site=$("$py" -I -c 'import sysconfig; print(sysconfig.get_paths()["purelib"])')
  while read -r name; do echo "Installed $name"; : > "$site/$name.py"; done < "$req"
fi
"#,
        calls = calls.display(),
        install = u8::from(install),
    );
    fs::write(&path, body).expect("write uv");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

fn calls(dir: &Path) -> Vec<String> {
    fs::read_to_string(dir.join("uv.calls"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

struct Setup {
    dir: tempfile::TempDir,
    python: PathBuf,
    uv: PathBuf,
}

fn setup(before: &str, install: bool) -> Setup {
    let dir = tempfile::tempdir().expect("tempdir");
    let python = bare_venv(dir.path());
    let uv = fake_uv(dir.path(), before, install);
    Setup { dir, python, uv }
}

fn plan(s: &Setup, mode: AutofixMode, trusted: Trusted, args: &[&str]) -> InstancePlan {
    let mut p = ProjectConfig::new("shop", "odoo-19.0");
    p.instances[0].extra_args = args.iter().map(|a| (*a).to_owned()).collect();
    let mut plan =
        InstancePlan::for_instance(&p, s.dir.path(), "main", s.python.clone(), fixture(), None)
            .expect("plan");
    plan.logs_dir = None;
    plan.autofix = Some(AutoFix {
        mode,
        install: InstallOptions::new(s.python.clone(), VenvTool::Uv(s.uv.clone())),
        trusted: Arc::new(trusted),
    });
    plan
}

fn owner() -> PortOwner {
    PortOwner::new("shop", "main")
}

fn notices(events: &[ManagerEvent]) -> Vec<&Notice> {
    events
        .iter()
        .filter_map(|e| match &e.event {
            InstanceEvent::Manager(n) => Some(n),
            InstanceEvent::Supervisor(_) => None,
        })
        .collect()
}

fn spawns(events: &[ManagerEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e.event, InstanceEvent::Supervisor(Event::Spawned { .. })))
        .count()
}

/// Collects instance events until `done` holds for them.
async fn events_until(
    rx: &mut Subscription<ManagerEvent>,
    mut done: impl FnMut(&[ManagerEvent]) -> bool,
) -> Vec<ManagerEvent> {
    let mut events = Vec::new();
    let r = tokio::time::timeout(WAIT, async {
        while !done(&events) {
            match rx.recv().await.expect("bus open") {
                Delivery::Event(e) => events.push(e),
                Delivery::Lagged { missed } => panic!("lagged by {missed}"),
            }
        }
    })
    .await;
    assert!(r.is_ok(), "timed out; events so far: {events:#?}");
    events
}

fn has(events: &[ManagerEvent], f: impl Fn(&Notice) -> bool) -> bool {
    notices(events).into_iter().any(f)
}

fn running(events: &[ManagerEvent]) -> bool {
    events.iter().any(|e| {
        matches!(
            e.event,
            InstanceEvent::Supervisor(Event::Status {
                status: Status::Running { .. }
            })
        )
    })
}

/// Waits until the instance is done (terminal and no fix under way).
async fn wait_done(m: &Manager) {
    let deadline = Instant::now() + WAIT;
    while !m.get(&owner()).expect("known").is_done() {
        assert!(Instant::now() < deadline, "{:?}", m.get(&owner()));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_trusted_module_is_installed_and_the_instance_restarts() {
    // The pause keeps the install running while the crash is looked at.
    let s = setup("sleep 0.3", true);
    let m = Manager::new(options(30_000));
    let mut rx = m.subscribe();
    let mut logs = m.subscribe_logs();
    // `requests` is in the built-in table, so trusted.
    let p = plan(
        &s,
        AutofixMode::Auto,
        Trusted::builtin(),
        &["--simulate-missing", "requests"],
    );
    m.start(p).await.expect("start");

    let mut saw_busy_crash = false;
    let events = events_until(&mut rx, |ev| {
        // While the package is installed, the crashed instance is not done.
        if let Some(info) = m.get(&owner())
            && info.status.is_terminal()
            && !info.is_done()
        {
            saw_busy_crash = true;
        }
        running(ev)
    })
    .await;
    assert_eq!(spawns(&events), 2, "{events:#?}");
    let n = notices(&events);
    assert!(
        matches!(n[0], Notice::DependencyMissing { module, package: Some(p), trust: Some(Trust::Table), action: FixAction::Install } if module == "requests" && p == "requests"),
        "{n:#?}"
    );
    assert!(matches!(
        n[1],
        Notice::DependencyInstalling { package, attempt: 1, max: 3, .. } if package == "requests"
    ));
    assert!(matches!(n[2], Notice::DependencyInstalled { package } if package == "requests"));
    assert_eq!(calls(s.dir.path()), ["requests"]);
    assert!(saw_busy_crash, "the crash was reported while the fix ran");

    let info = m.get(&owner()).expect("info");
    let fix = info.autofix.expect("autofix info");
    assert_eq!((fix.attempts, fix.active, fix.pending), (1, false, None));

    // The installer's output and OdHive's notes are in the instance's log.
    let text: Vec<String> = logs
        .drain()
        .iter()
        .map(|e| e.record.entry.to_string())
        .collect();
    let text = text.join("\n");
    assert!(text.contains("odhive: installing requests"), "{text}");
    assert!(text.contains("odhive: Installed requests"), "{text}");
    m.stop(&owner()).await.expect("stop");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_instance_that_keeps_running_is_restarted_after_the_install() {
    let s = setup("", true);
    let m = Manager::new(options(30_100));
    let mut rx = m.subscribe();
    let p = plan(
        &s,
        AutofixMode::Auto,
        Trusted::builtin(),
        &["--simulate-missing", "yaml", "--missing-keeps-running"],
    );
    m.start(p).await.expect("start");
    let events = events_until(&mut rx, |ev| spawns(ev) == 2 && running(ev)).await;
    assert!(has(
        &events,
        |n| matches!(n, Notice::DependencyInstalled { package } if package == "PyYAML")
    ));
    // The table names PyYAML; the fake uv wrote `PyYAML.py`, so `yaml` is
    // still missing in the second run: the same package is never retried.
    let events = events_until(&mut rx, |ev| {
        has(ev, |n| {
            matches!(
                n,
                Notice::DependencyMissing {
                    action: FixAction::GiveUp { .. },
                    ..
                }
            )
        })
    })
    .await;
    assert!(has(&events, |n| matches!(
        n,
        Notice::DependencyMissing {
            action: FixAction::GiveUp { reason: GiveUp::StillMissing { package } },
            ..
        } if package == "PyYAML"
    )));
    assert_eq!(calls(s.dir.path()), ["PyYAML"]);
    m.stop(&owner()).await.expect("stop");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn untrusted_package_waits_for_confirmation() {
    let s = setup("", true);
    let m = Manager::new(options(30_200));
    let mut rx = m.subscribe();
    let p = plan(
        &s,
        AutofixMode::Auto,
        Trusted::builtin(),
        &["--simulate-missing", "zz_unknown_mod"],
    );
    m.start(p).await.expect("start");
    let events = events_until(&mut rx, |ev| {
        has(ev, |n| matches!(n, Notice::DependencyMissing { .. }))
    })
    .await;
    assert!(has(&events, |n| matches!(
        n,
        Notice::DependencyMissing { package: Some(p), trust: None, action: FixAction::Confirm, .. }
            if p == "zz_unknown_mod"
    )));
    wait_done(&m).await;
    let info = m.get(&owner()).expect("info");
    assert!(matches!(info.status, Status::Crashed { .. }));
    let pending = info.autofix.and_then(|f| f.pending).expect("pending");
    assert_eq!(pending.package, "zz_unknown_mod");
    assert!(calls(s.dir.path()).is_empty(), "nothing installed unasked");

    let c = m.confirm_fix(&owner()).await.expect("confirm");
    assert_eq!(c.package, "zz_unknown_mod");
    events_until(&mut rx, running).await;
    assert_eq!(calls(s.dir.path()), ["zz_unknown_mod"]);
    assert!(matches!(
        m.confirm_fix(&owner()).await,
        Err(ManagerError::NoPendingFix(_))
    ));
    m.stop(&owner()).await.expect("stop");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ask_mode_asks_even_for_trusted_packages_and_decline_drops_it() {
    let s = setup("", true);
    let m = Manager::new(options(30_300));
    let mut rx = m.subscribe();
    let p = plan(
        &s,
        AutofixMode::Ask,
        Trusted::builtin(),
        &["--simulate-missing", "requests"],
    );
    m.start(p).await.expect("start");
    events_until(&mut rx, |ev| {
        has(ev, |n| {
            matches!(
                n,
                Notice::DependencyMissing {
                    action: FixAction::Confirm,
                    trust: Some(_),
                    ..
                }
            )
        })
    })
    .await;
    wait_done(&m).await;
    assert_eq!(
        m.decline_fix(&owner()).expect("decline").package,
        "requests"
    );
    assert!(matches!(
        m.decline_fix(&owner()),
        Err(ManagerError::NoPendingFix(_))
    ));
    assert!(calls(s.dir.path()).is_empty());
    m.stop(&owner()).await.expect("stop");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn off_mode_only_reports() {
    let s = setup("", true);
    let m = Manager::new(options(30_400));
    let mut rx = m.subscribe();
    let p = plan(
        &s,
        AutofixMode::Off,
        Trusted::builtin(),
        &["--simulate-missing", "requests"],
    );
    m.start(p).await.expect("start");
    events_until(&mut rx, |ev| {
        has(ev, |n| {
            matches!(
                n,
                Notice::DependencyMissing {
                    action: FixAction::Report,
                    ..
                }
            )
        })
    })
    .await;
    wait_done(&m).await;
    assert!(calls(s.dir.path()).is_empty());
    m.stop(&owner()).await.expect("stop");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_install_leaves_the_instance_crashed() {
    let s = setup("echo 'error: no matching distribution' >&2; exit 1", false);
    let m = Manager::new(options(30_500));
    let mut rx = m.subscribe();
    let p = plan(
        &s,
        AutofixMode::Auto,
        Trusted::builtin(),
        &["--simulate-missing", "requests"],
    );
    m.start(p).await.expect("start");
    let events = events_until(&mut rx, |ev| {
        has(ev, |n| matches!(n, Notice::DependencyFailed { .. }))
    })
    .await;
    assert!(has(&events, |n| matches!(
        n,
        Notice::DependencyFailed { package, error } if package == "requests" && error.contains("no matching distribution")
    )));
    wait_done(&m).await;
    assert_eq!(spawns(&events), 1);
    assert!(matches!(
        m.get(&owner()).expect("info").status,
        Status::Crashed { .. }
    ));
    m.stop(&owner()).await.expect("stop");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopping_cancels_a_running_install() {
    let s = setup("sleep 30", true);
    let m = Manager::new(options(30_600));
    let mut rx = m.subscribe();
    let p = plan(
        &s,
        AutofixMode::Auto,
        Trusted::builtin(),
        &["--simulate-missing", "requests"],
    );
    m.start(p).await.expect("start");
    events_until(&mut rx, |ev| {
        has(ev, |n| matches!(n, Notice::DependencyInstalling { .. }))
    })
    .await;
    assert!(matches!(
        m.start(plan(&s, AutofixMode::Auto, Trusted::builtin(), &[]))
            .await,
        Err(ManagerError::Fixing(_))
    ));
    let started = Instant::now();
    m.stop(&owner()).await.expect("stop");
    assert!(m.get(&owner()).is_none());
    // Give a wrongly surviving installer the chance to restart it.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(m.get(&owner()).is_none());
    assert!(started.elapsed() < Duration::from_secs(10));
    let rest = rx.drain();
    assert_eq!(spawns(&rest), 0, "{rest:#?}");
}
