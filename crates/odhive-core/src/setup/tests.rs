use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use super::*;
use crate::python::find_executable;
use crate::version::RELEASE_PY;

fn community_tree(dir: &Path) -> PathBuf {
    fs::create_dir_all(dir.join("odoo")).expect("mkdir");
    fs::create_dir_all(dir.join("addons")).expect("mkdir");
    fs::write(dir.join("odoo-bin"), "#!/usr/bin/env python3\n").expect("write");
    fs::write(
        dir.join(RELEASE_PY),
        "version_info = (19, 0, 0, FINAL, 0, '')\n",
    )
    .expect("write");
    fs::write(dir.join("requirements.txt"), "# nothing\n").expect("write");
    fs::canonicalize(dir).expect("canonicalize")
}

struct Fixture {
    _tmp: tempfile::TempDir,
    base: PathBuf,
    cfg: GlobalConfig,
    ws: Workspace,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = fs::canonicalize(tmp.path()).expect("canonicalize");
    let ws = Workspace::init(&base.join("ws")).expect("init").workspace;
    let mut cfg = GlobalConfig::new(base.join("ws"));
    let src = community_tree(&base.join("odoo-src"));
    community_source(&mut cfg, src.to_str().expect("utf8")).expect("register");
    Fixture {
        _tmp: tmp,
        base,
        cfg,
        ws,
    }
}

fn new_project(name: &str) -> NewProject {
    NewProject {
        name: name.into(),
        community: "odoo-19.0".into(),
        enterprise: None,
        addons_paths: Vec::new(),
        python: None,
    }
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).expect("metadata").permissions().mode() & 0o777
}

#[test]
fn sources_are_found_by_id_or_registered_from_a_path() {
    let mut f = fixture();
    let by_id = community_source(&mut f.cfg, "odoo-19.0").expect("by id");
    let by_path = community_source(&mut f.cfg, f.base.join("odoo-src").to_str().expect("utf8"))
        .expect("by path");
    assert_eq!(by_id, by_path);
    assert_eq!(f.cfg.sources.len(), 1);
    assert!(matches!(
        community_source(&mut f.cfg, "odoo-20.0"),
        Err(SetupError::UnknownSource { .. })
    ));
    // An id of the wrong edition is not used.
    assert!(matches!(
        enterprise_source(&mut f.cfg, "odoo-19.0", "19.0".parse().expect("v")),
        Err(SetupError::UnknownSource { .. })
    ));
}

#[test]
fn create_project_makes_private_folders_and_registers_it() {
    let mut f = fixture();
    let extra = f.base.join("my-addons");
    fs::create_dir(&extra).expect("mkdir");
    let mut new = new_project("shop");
    new.addons_paths = vec![extra.clone(), extra.clone()];
    let p = create_project(&mut f.cfg, &f.ws, &new).expect("create");

    let dir = f.ws.projects_dir().join("shop");
    assert_eq!(p.entry.path, dir);
    assert_eq!(f.cfg.projects, vec![p.entry.clone()]);
    for sub in ["", "addons", "data", "logs", "backups"] {
        assert_eq!(mode(&dir.join(sub)), 0o700, "{sub}");
    }
    let saved = ProjectConfig::load(&dir).expect("load");
    assert_eq!(saved, p.config);
    assert_eq!(
        saved.addons_paths,
        vec![PathBuf::from("addons"), extra],
        "duplicates dropped"
    );
}

#[test]
fn create_project_refuses_and_leaves_nothing_behind() {
    let mut f = fixture();
    create_project(&mut f.cfg, &f.ws, &new_project("shop")).expect("create");
    assert!(matches!(
        create_project(&mut f.cfg, &f.ws, &new_project("shop")),
        Err(SetupError::AlreadyRegistered(_))
    ));
    assert!(matches!(
        create_project(&mut f.cfg, &f.ws, &new_project("Bad Name")),
        Err(SetupError::Name(_))
    ));

    // A folder that exists but is not registered is not taken over.
    fs::create_dir(f.ws.projects_dir().join("taken")).expect("mkdir");
    assert!(matches!(
        create_project(&mut f.cfg, &f.ws, &new_project("taken")),
        Err(SetupError::Exists(_))
    ));

    let mut bad = new_project("other");
    bad.addons_paths = vec![f.base.join("missing")];
    assert!(matches!(
        create_project(&mut f.cfg, &f.ws, &bad),
        Err(SetupError::BadAddonsPath(_))
    ));
    let mut bad = new_project("other");
    bad.enterprise = Some("enterprise-19.0".into());
    assert!(matches!(
        create_project(&mut f.cfg, &f.ws, &bad),
        Err(SetupError::Project(ProjectError::UnknownSource(_)))
    ));
    assert!(!f.ws.projects_dir().join("other").exists());
    assert_eq!(f.cfg.projects.len(), 1);

    let entry = unregister_project(&mut f.cfg, "shop").expect("registered");
    assert!(entry.path.join("odhive.toml").is_file(), "files are kept");
    assert!(unregister_project(&mut f.cfg, "shop").is_none());
}

fn python3() -> PathBuf {
    let path = std::env::var_os("PATH").expect("PATH");
    find_executable(&path, "python3").expect("python3 on PATH")
}

/// Uses the stdlib venv module (no uv) and an explicit interpreter.
#[tokio::test]
async fn setup_env_creates_the_venv_then_keeps_it() {
    let mut f = fixture();
    let mut p = create_project(&mut f.cfg, &f.ws, &new_project("shop")).expect("create");
    let opts = EnvOptions {
        python: Some(python3()),
        discover: DiscoverOptions {
            path_var: None,
            uv: None,
            explicit: Vec::new(),
            timeout: Duration::from_secs(30),
        },
        recreate: false,
        install: true,
    };
    let mut events = Vec::new();
    let report = match setup_env(&f.cfg, &mut p, &opts, |e| events.push(e)).await {
        Ok(r) => r,
        // Debian/Ubuntu without python3-venv cannot run this test.
        Err(SetupError::Venv(VenvError::StdlibVenvMissing { .. })) => return,
        Err(e) => panic!("{e}"),
    };
    assert_eq!(report.base.as_deref(), Some(python3().as_path()));
    assert!(report.python.is_file());
    assert_eq!(report.installed, 0);
    assert!(matches!(events.first(), Some(SetupEvent::Python { .. })));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SetupEvent::Collected { count: 0, .. }))
    );
    // The explicit interpreter is remembered.
    assert_eq!(
        ProjectConfig::load(p.dir()).expect("load").python,
        Some(python3())
    );

    let mut events = Vec::new();
    let again = setup_env(&f.cfg, &mut p, &opts, |e| events.push(e))
        .await
        .expect("again");
    assert_eq!(again.base, None);
    assert!(matches!(events.first(), Some(SetupEvent::VenvKept { .. })));
}

#[tokio::test]
async fn setup_env_refuses_a_broken_venv_unless_recreating() {
    let mut f = fixture();
    let mut p = create_project(&mut f.cfg, &f.ws, &new_project("shop")).expect("create");
    fs::create_dir(p.dir().join(VENV_DIR)).expect("mkdir");
    let mut opts = EnvOptions::from_env();
    opts.install = false;
    assert!(matches!(
        setup_env(&f.cfg, &mut p, &opts, |_| {}).await,
        Err(SetupError::BrokenVenv(_))
    ));
}
