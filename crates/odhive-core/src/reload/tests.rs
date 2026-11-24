use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::Duration;

use notify::event::{AccessKind, AccessMode, CreateKind, DataChange, ModifyKind, RemoveKind};
use notify::{Event, EventKind};

use super::watch::{Watch, WatchError, WatchOptions, testing};
use super::*;

fn change(module: &str, kind: FileKind) -> Option<Change> {
    Some(Change {
        module: module.into(),
        kind,
    })
}

#[test]
fn classifier_table() {
    use FileKind::{Data, Manifest, Python, Static};
    let table: &[(&str, Option<Change>)] = &[
        // Python code.
        ("m1/models/sale.py", change("m1", Python)),
        ("m1/__init__.py", change("m1", Python)),
        ("m1/models/__init__.py", change("m1", Python)),
        ("m1/wizard/deep/nested/x.py", change("m1", Python)),
        ("m1/tests/test_sale.py", change("m1", Python)),
        ("m1/controllers/main.py", change("m1", Python)),
        (
            "m1/migrations/19.0.1.1/post-migrate.py",
            change("m1", Python),
        ),
        // Data: views, data, reports, translations, security.
        ("m1/views/x.xml", change("m1", Data)),
        ("m1/data/mail_template.xml", change("m1", Data)),
        ("m1/report/invoice.xml", change("m1", Data)),
        ("m1/data/products.csv", change("m1", Data)),
        ("m1/i18n/fr.po", change("m1", Data)),
        ("m1/security/ir.model.access.csv", change("m1", Data)),
        ("m1/security/rules.xml", change("m1", Data)),
        ("m1/security/groups.txt", change("m1", Data)),
        ("m1/views.xml", change("m1", Data)),
        // Manifest only at the module's top.
        ("m1/__manifest__.py", change("m1", Manifest)),
        ("m1/sub/__manifest__.py", change("m1", Python)),
        // Static assets, whatever the extension.
        ("m1/static/src/js/widget.js", change("m1", Static)),
        ("m1/static/src/xml/templates.xml", change("m1", Static)),
        ("m1/static/src/scss/style.scss", change("m1", Static)),
        ("m1/static/description/icon.png", change("m1", Static)),
        ("m1/static/lib/x.py", change("m1", Static)),
        // Files Odoo does not load.
        ("m1/README.md", None),
        ("m1/README.rst", None),
        ("m1/doc/index.rst", None),
        ("m1/views/x.xml.bak", None),
        ("m1/i18n/m1.pot", None),
        // Not inside a module.
        ("m1", None),
        ("setup.py", None),
        ("README.md", None),
        ("", None),
        // Ignored folders.
        (".git/index", None),
        (".git/objects/ab/cdef", None),
        ("m1/.git/HEAD", None),
        ("m1/__pycache__/sale.cpython-314.pyc", None),
        ("m1/models/__pycache__/x.py", None),
        ("m1/static/node_modules/lib/index.js", None),
        ("m1/.idea/workspace.xml", None),
        ("m1/.vscode/settings.json", None),
        // Editor temp, swap and backup files.
        ("m1/models/.sale.py.swp", None),
        ("m1/models/sale.py.swp", None),
        ("m1/models/sale.py~", None),
        ("m1/models/.#sale.py", None),
        ("m1/models/#sale.py#", None),
        ("m1/models/4913", None),
        ("m1/models/sale.py___jb_tmp___", None),
        ("m1/models/sale.py___jb_old___", None),
        ("m1/models/sale.py.kate-swp", None),
        ("m1/models/sale.py.orig", None),
        ("m1/models/sale.pyc", None),
        ("m1/views/x.xml.tmp", None),
        // Not a module name.
        ("my-module/models/x.py", None),
        ("m1,base/models/x.py", None),
        ("-u/models/x.py", None),
        (".hidden/models/x.py", None),
        ("../m1/models/x.py", None),
        ("/abs/m1/x.py", None),
    ];
    for (path, expected) in table {
        assert_eq!(&classify(Path::new(path)), expected, "{path}");
    }
}

#[test]
fn module_names() {
    for ok in [
        "m1",
        "sale_stock",
        "A",
        "l10n_be",
        &"x".repeat(MAX_MODULE_NAME),
    ] {
        assert!(is_module_name(ok), "{ok}");
    }
    for bad in [
        "",
        "a-b",
        "a,b",
        "-u",
        "a b",
        "été",
        ".x",
        &"x".repeat(MAX_MODULE_NAME + 1),
    ] {
        assert!(!is_module_name(bad), "{bad}");
    }
}

fn batch(changes: &[(&str, FileKind)]) -> Batch {
    let mut b = Batch::default();
    for (i, (m, k)) in changes.iter().enumerate() {
        b.add(
            Path::new(&format!("/a/{m}/{i}")),
            Change {
                module: (*m).into(),
                kind: *k,
            },
        );
    }
    b
}

fn modules(m: &[&str]) -> Vec<String> {
    m.iter().map(|s| (*s).to_owned()).collect()
}

#[test]
fn decide_table() {
    use FileKind::{Data, Manifest, Python, Static};
    use ReloadMode::{Auto, Off, Restart};
    let update = |m: &[&str]| {
        Some(Action::Update {
            modules: modules(m),
        })
    };
    let refresh = |m: &[&str]| {
        Some(Action::RefreshBrowser {
            modules: modules(m),
        })
    };
    type Row<'a> = (ReloadMode, &'a [(&'a str, FileKind)], Option<Action>);
    let table: &[Row<'_>] = &[
        (Auto, &[("m1", Python)], Some(Action::Restart)),
        (Auto, &[("m1", Data)], update(&["m1"])),
        (Auto, &[("m1", Manifest)], update(&["m1"])),
        (Auto, &[("m1", Static)], refresh(&["m1"])),
        (
            Auto,
            &[("m1", Static), ("m2", Static)],
            refresh(&["m1", "m2"]),
        ),
        // An update restarts, so it covers Python and static changes.
        (Auto, &[("m1", Python), ("m2", Data)], update(&["m2"])),
        (
            Auto,
            &[("m2", Data), ("m1", Manifest), ("m3", Static)],
            update(&["m1", "m2"]),
        ),
        (
            Auto,
            &[("m1", Python), ("m1", Static)],
            Some(Action::Restart),
        ),
        (Restart, &[("m1", Data)], Some(Action::Restart)),
        (Restart, &[("m1", Manifest)], Some(Action::Restart)),
        (Restart, &[("m1", Python)], Some(Action::Restart)),
        (Restart, &[("m1", Static)], refresh(&["m1"])),
        (Off, &[("m1", Python)], None),
        (Off, &[("m1", Data)], None),
        (Auto, &[], None),
    ];
    for (mode, changes, expected) in table {
        assert_eq!(
            &decide(*mode, &batch(changes)),
            expected,
            "{mode:?} {changes:?}"
        );
    }
}

#[test]
fn new_removed_and_lost_events() {
    // A new module alone: only reported, nothing restarts.
    let mut b = batch(&[("m9", FileKind::Manifest), ("m9", FileKind::Python)]);
    b.new_modules.insert("m9".into());
    assert_eq!(decide(ReloadMode::Auto, &b), None);
    assert!(!b.is_empty());
    // A new module next to an update: the update leaves it out.
    let mut b = batch(&[("m9", FileKind::Manifest), ("m1", FileKind::Data)]);
    b.new_modules.insert("m9".into());
    assert_eq!(
        decide(ReloadMode::Auto, &b),
        Some(Action::Update {
            modules: modules(&["m1"])
        })
    );
    // A removed module cannot be updated: plain restart.
    let mut b = batch(&[("m1", FileKind::Manifest), ("m2", FileKind::Data)]);
    b.removed_modules.insert("m1".into());
    assert_eq!(decide(ReloadMode::Auto, &b), Some(Action::Restart));
    // Lost events: plain restart.
    let b = Batch {
        rescan: true,
        ..Batch::default()
    };
    assert_eq!(decide(ReloadMode::Auto, &b), Some(Action::Restart));
    assert_eq!(decide(ReloadMode::Off, &b), None);
}

#[test]
fn action_args() {
    let a = Action::Update {
        modules: modules(&["m1", "m2"]),
    };
    assert_eq!(a.args(), ["-u", "m1,m2"].map(std::ffi::OsString::from));
    assert!(a.restarts());
    assert!(Action::Restart.args().is_empty());
    let r = Action::RefreshBrowser {
        modules: modules(&["m1"]),
    };
    assert!(r.args().is_empty() && !r.restarts());
    assert_eq!(
        serde_json::to_value(&a).expect("json"),
        serde_json::json!({"kind": "update", "modules": ["m1", "m2"]})
    );
}

#[test]
fn batch_lists_files_once_and_caps_them() {
    let mut b = Batch::default();
    let c = || Change {
        module: "m1".into(),
        kind: FileKind::Python,
    };
    b.add(Path::new("/a/m1/x.py"), c());
    b.add(Path::new("/a/m1/x.py"), c());
    assert_eq!((b.files.len(), b.file_count), (1, 1));
    for i in 0..MAX_LISTED_FILES * 2 {
        b.add(Path::new(&format!("/a/m1/{i}.py")), c());
    }
    assert_eq!(b.files.len(), MAX_LISTED_FILES);
    assert_eq!(b.file_count, MAX_LISTED_FILES * 2 + 1);
}

// --- Watch: debounce and module tracking, fed by hand -----------------------

/// `<tmp>/addons/{m1,m2}` modules, `<tmp>/addons/notyet/` (no manifest),
/// `<tmp>/outside/linked` module and `addons/linked -> ../outside/linked`.
fn addons() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("addons");
    for m in ["m1", "m2"] {
        fs::create_dir_all(root.join(m).join("views")).expect("mkdir");
        fs::write(root.join(m).join(MANIFEST), "{}").expect("manifest");
    }
    fs::create_dir_all(root.join("notyet")).expect("mkdir");
    let outside = dir.path().join("outside/linked");
    fs::create_dir_all(&outside).expect("mkdir");
    fs::write(outside.join(MANIFEST), "{}").expect("manifest");
    symlink(&outside, root.join("linked")).expect("symlink");
    let root = fs::canonicalize(root).expect("canonical");
    (dir, root)
}

fn ev(kind: EventKind, path: PathBuf) -> notify::Result<Event> {
    Ok(Event::new(kind).add_path(path))
}

fn modify(path: PathBuf) -> notify::Result<Event> {
    ev(
        EventKind::Modify(ModifyKind::Data(DataChange::Content)),
        path,
    )
}

const FAST: WatchOptions = WatchOptions {
    debounce: Duration::from_millis(500),
    max_delay: Duration::from_secs(5),
};

#[tokio::test(start_paused = true)]
async fn changes_within_the_debounce_window_form_one_batch() {
    let (_d, root) = addons();
    let (mut w, tx) = testing::channel(std::slice::from_ref(&root), FAST).expect("watch");
    assert_eq!(w.modules().iter().collect::<Vec<_>>(), ["m1", "m2"]);
    let t0 = tokio::time::Instant::now();
    tx.send(modify(root.join("m1/views/x.xml"))).expect("send");
    let tx2 = tx.clone();
    let root2 = root.clone();
    tokio::spawn(async move {
        for _ in 0..3 {
            tokio::time::sleep(Duration::from_millis(300)).await;
            tx2.send(modify(root2.join("m2/models.py"))).expect("send");
        }
    });
    let b = w.next().await.expect("batch");
    // Closed 500 ms after the last change (at 900 ms).
    assert_eq!(t0.elapsed(), Duration::from_millis(1400));
    assert_eq!(b.modules.len(), 2);
    assert_eq!(b.file_count, 2);
    assert_eq!(
        decide(ReloadMode::Auto, &b),
        Some(Action::Update {
            modules: modules(&["m1"])
        })
    );
}

#[tokio::test(start_paused = true)]
async fn a_steady_stream_of_changes_is_cut_at_the_max_delay() {
    let (_d, root) = addons();
    let (mut w, tx) = testing::channel(std::slice::from_ref(&root), FAST).expect("watch");
    let t0 = tokio::time::Instant::now();
    let path = root.join("m1/x.py");
    tokio::spawn(async move {
        loop {
            if tx.send(modify(path.clone())).is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });
    w.next().await.expect("batch");
    assert_eq!(t0.elapsed(), FAST.max_delay);
}

#[tokio::test(start_paused = true)]
async fn ignored_access_and_unknown_paths_make_no_batch() {
    let (_d, root) = addons();
    let (mut w, tx) = testing::channel(std::slice::from_ref(&root), FAST).expect("watch");
    // Reads, ignored files, folders without manifest, links out of the root,
    // paths outside the root: all dropped, so no batch at all.
    tx.send(ev(
        EventKind::Access(AccessKind::Open(AccessMode::Read)),
        root.join("m1/x.py"),
    ))
    .expect("send");
    tx.send(modify(root.join("m1/.x.py.swp"))).expect("send");
    tx.send(modify(root.join("m1/__pycache__/x.pyc")))
        .expect("send");
    tx.send(modify(root.join("notyet/x.py"))).expect("send");
    tx.send(modify(root.join("linked/x.py"))).expect("send");
    tx.send(modify(root.join("linked/views/x.xml")))
        .expect("send");
    tx.send(modify(root.parent().expect("parent").join("other/m1/x.py")))
        .expect("send");
    tx.send(Err(notify::Error::generic("boom"))).expect("send");
    // Then one real change, which must be the only thing in the batch.
    tx.send(ev(
        EventKind::Access(AccessKind::Close(AccessMode::Write)),
        root.join("m2/x.py"),
    ))
    .expect("send");
    let b = w.next().await.expect("batch");
    assert_eq!(b.files, [root.join("m2/x.py")]);
    assert!(b.new_modules.is_empty() && b.removed_modules.is_empty());
}

#[tokio::test(start_paused = true)]
async fn new_and_removed_modules_are_noticed() {
    let (_d, root) = addons();
    let (mut w, tx) = testing::channel(std::slice::from_ref(&root), FAST).expect("watch");

    // A manifest appears in `notyet`: a new module, nothing restarts.
    fs::write(root.join("notyet").join(MANIFEST), "{}").expect("manifest");
    tx.send(ev(
        EventKind::Create(CreateKind::File),
        root.join("notyet").join(MANIFEST),
    ))
    .expect("send");
    let b = w.next().await.expect("batch");
    assert_eq!(b.new_modules.iter().collect::<Vec<_>>(), ["notyet"]);
    assert_eq!(decide(ReloadMode::Auto, &b), None);
    // From now on it is a module like the others.
    tx.send(modify(root.join("notyet/views/a.xml")))
        .expect("send");
    let b = w.next().await.expect("batch");
    assert!(b.new_modules.is_empty());
    assert_eq!(
        decide(ReloadMode::Auto, &b),
        Some(Action::Update {
            modules: modules(&["notyet"])
        })
    );

    // A module folder renamed: only the folder event arrives.
    fs::rename(root.join("m2"), root.join("m3")).expect("rename");
    tx.send(ev(EventKind::Modify(ModifyKind::Any), root.join("m2")))
        .expect("send");
    tx.send(ev(EventKind::Modify(ModifyKind::Any), root.join("m3")))
        .expect("send");
    let b = w.next().await.expect("batch");
    assert_eq!(b.removed_modules.iter().collect::<Vec<_>>(), ["m2"]);
    assert_eq!(b.new_modules.iter().collect::<Vec<_>>(), ["m3"]);
    assert_eq!(decide(ReloadMode::Auto, &b), Some(Action::Restart));
    assert_eq!(
        w.modules().iter().collect::<Vec<_>>(),
        ["m1", "m3", "notyet"]
    );

    // Deleted entirely.
    fs::remove_dir_all(root.join("m1")).expect("rm");
    tx.send(ev(
        EventKind::Remove(RemoveKind::File),
        root.join("m1/views/x.xml"),
    ))
    .expect("send");
    let b = w.next().await.expect("batch");
    assert_eq!(b.removed_modules.iter().collect::<Vec<_>>(), ["m1"]);
    assert_eq!(decide(ReloadMode::Auto, &b), Some(Action::Restart));
}

#[tokio::test(start_paused = true)]
async fn lost_events_restart() {
    let (_d, root) = addons();
    let (mut w, tx) = testing::channel(std::slice::from_ref(&root), FAST).expect("watch");
    tx.send(Ok(
        Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan)
    ))
    .expect("send");
    let b = w.next().await.expect("batch");
    assert!(b.rescan);
    assert_eq!(decide(ReloadMode::Auto, &b), Some(Action::Restart));
}

#[tokio::test(start_paused = true)]
async fn the_watch_ends_when_its_source_does() {
    let (_d, root) = addons();
    let (mut w, tx) = testing::channel(std::slice::from_ref(&root), FAST).expect("watch");
    tx.send(modify(root.join("m1/x.py"))).expect("send");
    drop(tx);
    // The open batch is still handed out, then the end.
    assert!(w.next().await.is_some());
    assert!(w.next().await.is_none());
}

#[test]
fn start_errors() {
    assert!(matches!(
        Watch::start(&[], WatchOptions::default()),
        Err(WatchError::NoRoots)
    ));
    let dir = tempfile::tempdir().expect("tempdir");
    assert!(matches!(
        Watch::start(&[dir.path().join("missing")], WatchOptions::default()),
        Err(WatchError::Root { .. })
    ));
}

// --- Watch against the real file system --------------------------------------

async fn next(w: &mut Watch) -> Batch {
    tokio::time::timeout(Duration::from_secs(10), w.next())
        .await
        .expect("a batch in time")
        .expect("watching")
}

#[tokio::test]
async fn real_file_changes_arrive_as_batches() {
    let (_d, root) = addons();
    let opts = WatchOptions {
        debounce: Duration::from_millis(200),
        max_delay: Duration::from_secs(2),
    };
    let (mut w, started) = Watch::start(std::slice::from_ref(&root), opts).expect("watch");
    assert_eq!(started.roots, std::slice::from_ref(&root));
    assert_eq!(started.modules, 2);
    assert_eq!(started.unwatched, [root.join("linked")]);

    // FSEvents can still deliver the fixture's own setup writes after the
    // watch started. Write a marker and drop every batch up to the one that
    // carries it, so the checks below only see their own changes.
    tokio::time::sleep(Duration::from_millis(200)).await;
    fs::write(root.join("m1/views/settle.xml"), "<odoo/>").expect("write marker");
    loop {
        let b = next(&mut w).await;
        if b.files.iter().any(|f| f.ends_with("m1/views/settle.xml")) {
            break;
        }
    }
    fs::write(root.join("m1/views/x.xml"), "<odoo/>").expect("write");
    let b = next(&mut w).await;
    assert_eq!(
        decide(ReloadMode::Auto, &b),
        Some(Action::Update {
            modules: modules(&["m1"])
        }),
        "{b:?}"
    );

    // Changes through the link out of the root are not seen; a later change
    // inside is the only thing in the next batch.
    fs::write(root.join("linked/x.py"), "").expect("write via link");
    fs::create_dir_all(root.join("m2/static/src")).expect("mkdir");
    fs::write(root.join("m2/static/src/a.js"), "").expect("write");
    let b = next(&mut w).await;
    assert_eq!(
        decide(ReloadMode::Auto, &b),
        Some(Action::RefreshBrowser {
            modules: modules(&["m2"])
        }),
        "{b:?}"
    );
    assert!(b.modules.keys().all(|m| m != "linked"), "{b:?}");
}
