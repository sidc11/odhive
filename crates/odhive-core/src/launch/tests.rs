use std::os::unix::fs::symlink;

use super::*;
use crate::pidfile::{ProcessId, RECORD_VERSION};
use crate::python::find_executable;
use crate::setup::{NewProject, community_source, create_project};
use crate::workspace::Workspace;

#[test]
fn targets_parse_and_match() {
    let t: Target = "shop".parse().expect("parse");
    assert_eq!(t, Target::project("shop"));
    assert_eq!(t.to_string(), "shop");
    assert!(t.matches(&PortOwner::new("shop", "main")));
    assert!(!t.matches(&PortOwner::new("shop2", "main")));

    let t: Target = "shop#test".parse().expect("parse");
    assert_eq!(t.instance.as_deref(), Some("test"));
    assert_eq!(t.to_string(), "shop#test");
    assert!(t.matches(&PortOwner::new("shop", "test")));
    assert!(!t.matches(&PortOwner::new("shop", "main")));

    for bad in ["", "#main", "shop#", "a#b#c", "Shop", "../x", "shop#a/b"] {
        assert!(bad.parse::<Target>().is_err(), "{bad:?}");
    }
}

struct Fixture {
    _tmp: tempfile::TempDir,
    base: PathBuf,
    cfg: GlobalConfig,
    paths: AppPaths,
    project: LoadedProject,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = fs::canonicalize(tmp.path()).expect("canonicalize");
    let src = base.join("odoo-src");
    fs::create_dir_all(src.join("odoo")).expect("mkdir");
    fs::create_dir_all(src.join("addons")).expect("mkdir");
    fs::write(src.join("odoo-bin"), "").expect("write");
    fs::write(
        src.join("odoo/release.py"),
        "version_info = (19, 0, 0, FINAL, 0, '')\n",
    )
    .expect("write");
    let ws = Workspace::init(&base.join("ws")).expect("init").workspace;
    let mut cfg = GlobalConfig::new(base.join("ws"));
    community_source(&mut cfg, src.to_str().expect("utf8")).expect("source");
    let extra = base.join("custom");
    fs::create_dir_all(extra.join("my_mod")).expect("mkdir");
    fs::write(extra.join("my_mod/__manifest__.py"), "{'name': 'M'}").expect("write");
    let new = NewProject {
        name: "shop".into(),
        community: "odoo-19.0".into(),
        enterprise: None,
        addons_paths: vec![extra],
        python: None,
        instance: crate::project::Instance::new(crate::project::DEFAULT_INSTANCE),
    };
    let project = create_project(&mut cfg, &ws, &new).expect("create");
    let paths = AppPaths {
        config_dir: base.join("config"),
        data_dir: base.join("data"),
        home_dir: base.clone(),
    };
    Fixture {
        _tmp: tmp,
        base,
        cfg,
        paths,
        project,
    }
}

fn fake_venv(project: &LoadedProject) -> PathBuf {
    let bin = project.dir().join(".venv/bin");
    fs::create_dir_all(&bin).expect("mkdir");
    let path = std::env::var_os("PATH").expect("PATH");
    let py = find_executable(&path, "python3").expect("python3");
    symlink(py, bin.join("python")).expect("symlink");
    bin.join("python")
}

#[test]
fn load_project_checks_registry_and_name() {
    let mut f = fixture();
    let p = load_project(&f.cfg, "shop").expect("load");
    assert_eq!(p.config.name, "shop");
    assert!(matches!(
        load_project(&f.cfg, "nope"),
        Err(LaunchError::UnknownProject(_))
    ));
    f.cfg.projects.push(ProjectEntry {
        name: "alias".into(),
        path: p.entry.path.clone(),
    });
    assert!(matches!(
        load_project(&f.cfg, "alias"),
        Err(LaunchError::NameMismatch { .. })
    ));
}

#[test]
fn select_picks_instances_or_lists_the_known_ones() {
    let mut f = fixture();
    f.project
        .config
        .instances
        .push(crate::project::Instance::new("test"));
    let all = f.project.select(&Target::project("shop")).expect("all");
    assert_eq!(all.len(), 2);
    let one = f
        .project
        .select(&"shop#test".parse().expect("t"))
        .expect("one");
    assert_eq!(one[0].id, "test");
    let err = f
        .project
        .select(&"shop#nope".parse().expect("t"))
        .expect_err("unknown");
    assert!(err.to_string().contains("main, test"), "{err}");
}

#[tokio::test]
async fn prepare_needs_a_venv() {
    let f = fixture();
    let all = f.project.select(&Target::project("shop")).expect("all");
    assert!(matches!(
        prepare(&f.cfg, &f.paths, &f.project, &all).await,
        Err(LaunchError::NoVenv(..))
    ));
}

#[tokio::test]
async fn prepare_writes_a_loopback_conf_without_empty_addons_and_builds_plans() {
    let f = fixture();
    let python = fake_venv(&f.project);
    let all = f.project.select(&Target::project("shop")).expect("all");
    let prepared = prepare(&f.cfg, &f.paths, &f.project, &all)
        .await
        .expect("prepare");

    let conf = fs::read_to_string(&prepared.conf).expect("conf");
    assert!(conf.contains("http_interface = 127.0.0.1"), "{conf}");
    let addons = conf
        .lines()
        .find(|l| l.starts_with("addons_path"))
        .expect("addons_path");
    assert!(addons.contains(&*f.base.join("odoo-src/addons").to_string_lossy()));
    assert!(addons.contains(&*f.base.join("custom").to_string_lossy()));
    assert!(!addons.contains(&*f.project.dir().join("addons").to_string_lossy()));
    assert_eq!(
        prepared.skipped_addons,
        vec![f.project.dir().join("addons")]
    );
    // The fake source has no tools/config.py: the bundled schema is used.
    assert!(matches!(
        prepared.schema_origin,
        SchemaOrigin::Bundled { .. }
    ));

    let plan = &prepared.plans[0];
    assert_eq!(plan.owner, PortOwner::new("shop", "main"));
    assert_eq!(plan.spec.python, python);
    assert_eq!(plan.spec.odoo_bin, f.base.join("odoo-src/odoo-bin"));
    assert_eq!(plan.spec.conf.as_deref(), Some(prepared.conf.as_path()));
    assert_eq!(plan.spec.db.as_deref(), Some("shop_main"));
    assert!(plan.ports_automatic());
}

#[tokio::test]
async fn a_configured_http_interface_is_kept() {
    let mut f = fixture();
    fake_venv(&f.project);
    f.project
        .config
        .odoo_options
        .0
        .insert("http_interface".into(), "0.0.0.0".into());
    let all = f.project.select(&Target::project("shop")).expect("all");
    let prepared = prepare(&f.cfg, &f.paths, &f.project, &all)
        .await
        .expect("prepare");
    let conf = fs::read_to_string(&prepared.conf).expect("conf");
    assert!(conf.contains("http_interface = 0.0.0.0"), "{conf}");
}

#[test]
fn states_report_stopped_running_and_stale() {
    let f = fixture();
    let run = RunDir::new(f.base.join("run"));
    let target = Target::project("shop");
    let s = states(&run, &f.project, &target).expect("states");
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].run, None);
    assert_eq!(s[0].db, "shop_main");
    assert!(!s[0].is_running());
    assert!(running_records(&run).expect("records").is_empty());

    let me = ProcessId::current().expect("me");
    let mut record = PidRecord {
        version: RECORD_VERSION,
        project: "shop".into(),
        instance: "main".into(),
        process: me,
        http_port: 18_069,
        gevent_port: 18_072,
        db: Some("custom_db".into()),
        odoo_bin: "/x/odoo-bin".into(),
        supervisor: None,
    };
    run.write(&record).expect("write");
    let s = states(&run, &f.project, &target).expect("states");
    assert!(s[0].is_running());
    assert_eq!(s[0].db, "custom_db");
    assert_eq!(
        running_records(&run).expect("records"),
        vec![record.clone()]
    );

    record.process.start_time += 1;
    run.write(&record).expect("write");
    let s = states(&run, &f.project, &target).expect("states");
    assert_eq!(s[0].run, Some(RecordState::Stale { pid: me.pid }));
    assert!(running_records(&run).expect("records").is_empty());
}
