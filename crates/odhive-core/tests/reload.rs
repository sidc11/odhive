//! Smart Reload against the `fake-odoo-bin` fixture and the real file
//! watcher: file changes in a custom addons folder restart the instance,
//! with `-u <module>` for data files.
//!
//! Ports come from 31100–31700 (see tests/manager.rs for why below the
//! ephemeral range); each test scans its own block.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use odhive_core::events::{Delivery, Subscription};
use odhive_core::manager::{
    InstanceEvent, InstancePlan, Manager, ManagerEvent, ManagerOptions, Notice, Reload,
};
use odhive_core::ports::{AllocOptions, PortOwner};
use odhive_core::project::{ProjectConfig, ReloadMode};
use odhive_core::python::find_executable;
use odhive_core::reload::Action;
use odhive_core::reload::watch::WatchOptions;
use odhive_core::supervisor::{Event, Status};

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

fn owner() -> PortOwner {
    PortOwner::new("shop", "main")
}

/// A project folder with `addons/{m1,m2}` modules.
fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    for m in ["m1", "m2"] {
        let module = dir.path().join("addons").join(m);
        fs::create_dir_all(module.join("views")).expect("mkdir");
        fs::create_dir_all(module.join("static/src")).expect("mkdir");
        fs::write(module.join("__manifest__.py"), "{'name': 'x'}").expect("manifest");
        fs::write(module.join("views/x.xml"), "<odoo/>").expect("view");
        fs::write(module.join("models.py"), "").expect("py");
    }
    dir
}

fn plan(dir: &Path, mode: ReloadMode, args: &[&str]) -> InstancePlan {
    let mut p = ProjectConfig::new("shop", "odoo-19.0");
    p.instances[0].extra_args = args.iter().map(|a| (*a).to_owned()).collect();
    let mut plan =
        InstancePlan::for_instance(&p, dir, "main", python3(), fixture(), None).expect("plan");
    plan.logs_dir = None;
    plan.reload = Some(Reload {
        mode,
        roots: vec![dir.join("addons")],
        watch: WatchOptions {
            debounce: Duration::from_millis(150),
            max_delay: Duration::from_secs(2),
        },
    });
    plan
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

fn notices(events: &[ManagerEvent]) -> Vec<&Notice> {
    events
        .iter()
        .filter_map(|e| match &e.event {
            InstanceEvent::Manager(n) => Some(n),
            InstanceEvent::Supervisor(_) => None,
        })
        .collect()
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

fn spawns(events: &[ManagerEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e.event, InstanceEvent::Supervisor(Event::Spawned { .. })))
        .count()
}

fn reload_action(events: &[ManagerEvent]) -> Option<&Action> {
    notices(events).into_iter().find_map(|n| match n {
        Notice::Reload { action, .. } => Some(action),
        _ => None,
    })
}

/// Starts the instance and waits until it runs and its folders are watched.
async fn start_watched(m: &Manager, rx: &mut Subscription<ManagerEvent>, plan: InstancePlan) {
    m.start(plan).await.expect("start");
    let mut watching = false;
    events_until(rx, |ev| {
        watching |= notices(ev)
            .iter()
            .any(|n| matches!(n, Notice::ReloadWatching { .. }));
        watching && running(ev)
    })
    .await;
    // FSEvents (macOS) may still deliver the writes that set the project up,
    // from just before watching began. Let them pass, and the restart they
    // cause settle, so each test sees only its own changes.
    tokio::time::sleep(Duration::from_millis(800)).await;
    let late = rx.drain();
    if spawns(&late) > 0 && !running(&late) {
        events_until(rx, running).await;
    }
}

/// Log text of the instance so far.
fn log_text(m: &Manager) -> String {
    m.tail_log(&owner(), 10_000)
        .expect("log")
        .iter()
        .map(|r| r.entry.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn touching_a_view_restarts_with_update_of_its_module() {
    let dir = project();
    let m = Manager::new(options(31_100));
    let mut rx = m.subscribe();
    start_watched(&m, &mut rx, plan(dir.path(), ReloadMode::Auto, &[])).await;
    assert_eq!(log_text(&m).matches("Loading module m1 ").count(), 0);

    let view = dir.path().join("addons/m1/views/x.xml");
    fs::write(&view, "<odoo><!-- changed --></odoo>").expect("write");
    let events = events_until(&mut rx, running).await;
    assert_eq!(
        reload_action(&events),
        Some(&Action::Update {
            modules: vec!["m1".into()]
        }),
        "{events:#?}"
    );
    assert_eq!(spawns(&events), 1);
    let notice_files = notices(&events).into_iter().find_map(|n| match n {
        Notice::Reload { files, .. } => Some(files.clone()),
        _ => None,
    });
    assert_eq!(
        notice_files,
        Some(vec![fs::canonicalize(&view).expect("canonical")])
    );
    // The new process got `-u m1` and loaded the module.
    let text = log_text(&m);
    assert_eq!(text.matches("Loading module m1 ").count(), 1, "{text}");
    assert!(text.contains("changed; restarting with -u m1"), "{text}");
    let info = m.get(&owner()).expect("info").reload.expect("reload");
    assert!(info.watching);

    // A Python change: plain restart; the update is not repeated (the
    // process that had it is running).
    fs::write(dir.path().join("addons/m2/models.py"), "x = 1\n").expect("write");
    let events = events_until(&mut rx, running).await;
    assert_eq!(reload_action(&events), Some(&Action::Restart));
    assert_eq!(log_text(&m).matches("Loading module m1 ").count(), 1);

    // A static file: no restart, a refresh notice.
    fs::write(dir.path().join("addons/m2/static/src/a.js"), "1").expect("write");
    let events = events_until(&mut rx, |ev| reload_action(ev).is_some()).await;
    assert_eq!(
        reload_action(&events),
        Some(&Action::RefreshBrowser {
            modules: vec!["m2".into()]
        })
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(spawns(&rx.drain()), 0);
    m.stop(&owner()).await.expect("stop");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_mode_never_updates_and_off_never_watches() {
    let dir = project();
    let m = Manager::new(options(31_200));
    let mut rx = m.subscribe();
    start_watched(&m, &mut rx, plan(dir.path(), ReloadMode::Restart, &[])).await;
    fs::write(dir.path().join("addons/m1/views/x.xml"), "<odoo/>\n").expect("write");
    let events = events_until(&mut rx, running).await;
    assert_eq!(reload_action(&events), Some(&Action::Restart));
    assert_eq!(log_text(&m).matches("Loading module").count(), 0);
    m.stop(&owner()).await.expect("stop");

    let mut rx = m.subscribe();
    m.start(plan(dir.path(), ReloadMode::Off, &[]))
        .await
        .expect("start");
    events_until(&mut rx, running).await;
    assert!(m.get(&owner()).expect("info").reload.is_none());
    fs::write(dir.path().join("addons/m1/models.py"), "y = 2\n").expect("write");
    tokio::time::sleep(Duration::from_secs(1)).await;
    let events = rx.drain();
    assert_eq!(spawns(&events), 0, "{events:#?}");
    assert!(notices(&events).is_empty(), "{events:#?}");
    m.stop(&owner()).await.expect("stop");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crashed_instance_waits_for_a_fix_and_restarts_on_it() {
    let dir = project();
    let m = Manager::new(options(31_300));
    let mut rx = m.subscribe();
    m.start(plan(dir.path(), ReloadMode::Auto, &["--simulate", "crash"]))
        .await
        .expect("start");
    let crashed = |ev: &[ManagerEvent]| {
        ev.iter().any(|e| {
            matches!(
                e.event,
                InstanceEvent::Supervisor(Event::Status {
                    status: Status::Crashed { .. }
                })
            )
        })
    };
    events_until(&mut rx, crashed).await;
    // Wait for the watcher before judging `is_done`.
    let deadline = tokio::time::Instant::now() + WAIT;
    while !m
        .get(&owner())
        .and_then(|i| i.reload)
        .is_some_and(|r| r.watching)
    {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // As in `start_watched`: FSEvents (macOS) may still deliver the writes
    // that set the project up, which count as a fix. Let that restart crash
    // again, so only the write below is judged.
    tokio::time::sleep(Duration::from_millis(800)).await;
    let late = rx.drain();
    if spawns(&late) > 0 && !crashed(&late) {
        events_until(&mut rx, crashed).await;
    }
    let info = m.get(&owner()).expect("info");
    assert!(info.status.is_terminal() && !info.is_done(), "{info:?}");

    fs::write(dir.path().join("addons/m1/models.py"), "fixed = True\n").expect("write");
    // Judge the reload this write caused: a late setup write may still come
    // first on macOS and restart (with `-u`) on its own.
    let fix_reload = |ev: &[ManagerEvent]| {
        ev.iter().position(|e| {
            matches!(&e.event, InstanceEvent::Manager(Notice::Reload { files, .. })
                if files.iter().any(|f| f.ends_with("models.py")))
        })
    };
    let events = events_until(&mut rx, |ev| {
        fix_reload(ev).is_some_and(|at| spawns(&ev[at..]) >= 1)
    })
    .await;
    let at = fix_reload(&events).expect("reload of the fix");
    let InstanceEvent::Manager(Notice::Reload { action, files, .. }) = &events[at].event else {
        unreachable!("fix_reload finds reload notices");
    };
    if files.len() == 1 {
        assert_eq!(action, &Action::Restart);
    } else {
        // Batched with a late setup write: still a restart (maybe with `-u`).
        assert!(
            !matches!(action, Action::RefreshBrowser { .. }),
            "{action:?}"
        );
    }
    m.stop(&owner()).await.expect("stop");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_update_not_yet_loaded_goes_along_with_the_next_restart() {
    let dir = project();
    let m = Manager::new(options(31_400));
    let mut rx = m.subscribe();
    // Every process takes 2 s to run: long enough to change files meanwhile.
    start_watched(
        &m,
        &mut rx,
        plan(dir.path(), ReloadMode::Auto, &["--start-delay", "2"]),
    )
    .await;
    fs::write(dir.path().join("addons/m1/views/x.xml"), "<odoo>1</odoo>").expect("write");
    let events = events_until(&mut rx, |ev| spawns(ev) == 1).await;
    assert_eq!(
        reload_action(&events),
        Some(&Action::Update {
            modules: vec!["m1".into()]
        })
    );
    assert!(!running(&events));
    assert_eq!(
        m.get(&owner())
            .expect("info")
            .reload
            .expect("reload")
            .pending_update,
        ["m1"]
    );

    // A Python change while that process still starts: m1 goes along.
    fs::write(dir.path().join("addons/m2/models.py"), "z = 3\n").expect("write");
    let events = events_until(&mut rx, |ev| spawns(ev) == 1).await;
    assert_eq!(
        reload_action(&events),
        Some(&Action::Update {
            modules: vec!["m1".into()]
        }),
        "{events:#?}"
    );
    assert!(!running(&events));
    events_until(&mut rx, running).await;
    assert_eq!(log_text(&m).matches("Loading module m1 ").count(), 1);

    // Once a process with it runs, the next restart is plain.
    fs::write(dir.path().join("addons/m2/models.py"), "z = 4\n").expect("write");
    let events = events_until(&mut rx, |ev| spawns(ev) == 1).await;
    assert_eq!(reload_action(&events), Some(&Action::Restart));
    m.stop(&owner()).await.expect("stop");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_module_is_reported_and_stop_ends_watching() {
    let dir = project();
    let m = Manager::new(options(31_500));
    let mut rx = m.subscribe();
    start_watched(&m, &mut rx, plan(dir.path(), ReloadMode::Auto, &[])).await;

    let new = dir.path().join("addons/m9");
    fs::create_dir_all(&new).expect("mkdir");
    fs::write(new.join("__manifest__.py"), "{}").expect("manifest");
    let events = events_until(&mut rx, |ev| {
        notices(ev)
            .iter()
            .any(|n| matches!(n, Notice::ModuleAdded { module } if module == "m9"))
    })
    .await;
    assert_eq!(spawns(&events), 0);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let late = rx.drain();
    assert_eq!(
        spawns(&late),
        0,
        "a new module alone restarts nothing: {events:#?} {late:#?}"
    );

    m.stop(&owner()).await.expect("stop");
    fs::write(dir.path().join("addons/m1/models.py"), "after = 1\n").expect("write");
    tokio::time::sleep(Duration::from_millis(600)).await;
    let events = rx.drain();
    assert!(
        reload_action(&events).is_none() && spawns(&events) == 0,
        "{events:#?}"
    );
}
