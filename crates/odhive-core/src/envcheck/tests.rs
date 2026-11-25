use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::*;
use crate::python::{Origin, PyVersion};
use crate::testpg::TempPg;

fn v(major: u8) -> OdooVersion {
    OdooVersion::new(major, 0, false).expect("version")
}

fn interpreter(path: &str, minor: u8, implementation: &str) -> Interpreter {
    Interpreter {
        path: PathBuf::from(path),
        version: PyVersion {
            major: 3,
            minor,
            patch: 1,
        },
        implementation: implementation.into(),
        free_threaded: false,
        origins: vec![Origin::Path],
    }
}

/// Nothing on `PATH`, no Python, no PostgreSQL.
fn empty_options() -> CheckOptions {
    CheckOptions {
        path_var: None,
        discover: DiscoverOptions {
            path_var: None,
            uv: None,
            explicit: Vec::new(),
            timeout: Duration::from_secs(5),
        },
        postgres: PgTarget::nowhere(),
        requirements: Vec::new(),
        package_manager: Some(PackageManager::Apt),
        timeout: Duration::from_secs(5),
    }
}

fn script(dir: &Path, name: &str, body: &str) {
    let p = dir.join(name);
    fs::write(&p, format!("#!/bin/sh\n{body}\n")).expect("write");
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).expect("chmod");
}

fn by_id(report: &Report, id: CheckId) -> &Check {
    report.checks.iter().find(|c| c.id == id).expect("check")
}

#[test]
fn min_pg_version_is_read_from_release_py() {
    assert_eq!(min_pg_from_release_py("MIN_PG_VERSION = 13\n"), Some(13));
    assert_eq!(
        min_pg_from_release_py("x = 1\nMIN_PG_VERSION=16  # comment\n"),
        Some(16)
    );
    for bad in [
        "",
        "# MIN_PG_VERSION = 13",
        "    MIN_PG_VERSION = 13",
        "MIN_PG_VERSION == 13",
        "MIN_PG_VERSION = get()",
        "MIN_PG_VERSION = 13.5",
        "MIN_PG_VERSION = 1234",
    ] {
        assert_eq!(min_pg_from_release_py(bad), None, "{bad:?}");
    }
}

#[test]
fn builtin_requirements_cover_odoo_19_and_20() {
    let r = Requirement::builtin();
    assert_eq!(r.len(), 2);
    assert_eq!((r[0].label.as_str(), r[0].min_pg), ("Odoo 19.0", 13));
    assert_eq!((r[1].label.as_str(), r[1].min_pg), ("Odoo 20.0", 16));
    assert_eq!(r[1].python.min, (3, 12));
}

#[test]
fn source_requirements_come_from_release_py() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::create_dir_all(dir.path().join("odoo")).expect("mkdir");
    fs::write(
        dir.path().join("odoo/release.py"),
        "version_info = (19, 0, 0, 'final', 0, '')\nMIN_PY_VERSION = (3, 11)\nMAX_PY_VERSION = (3, 13)\nMIN_PG_VERSION = 14\n",
    )
    .expect("write");
    let r = Requirement::for_source("odoo-19.0", dir.path(), v(19));
    assert_eq!(r.label, "Odoo 19.0 (odoo-19.0)");
    assert_eq!(r.min_pg, 14);
    assert_eq!((r.python.min, r.python.max), ((3, 11), (3, 13)));

    let missing = Requirement::for_source("x", &dir.path().join("nope"), v(20));
    assert_eq!(missing.min_pg, 16);
}

#[test]
fn python_is_judged_per_requirement() {
    let reqs = Requirement::builtin();
    let found = |list: Vec<Interpreter>| Discovery {
        interpreters: list,
        ..Discovery::default()
    };

    let none = python_check(&found(vec![]), &reqs, Some(PackageManager::Apt), false);
    assert_eq!(none.status, Status::Error);
    let fix = none.fix.expect("fix");
    assert!(fix.commands[0].contains("python3-venv"), "{fix:?}");

    // 3.11 fits Odoo 19 only.
    let some = python_check(
        &found(vec![interpreter("/usr/bin/python3.11", 11, "cpython")]),
        &reqs,
        None,
        true,
    );
    assert_eq!(some.status, Status::Warning);
    assert!(some.summary.contains("Odoo 20.0"), "{}", some.summary);
    assert_eq!(some.fix.expect("fix").commands, ["uv python install 3.14"]);
    assert!(some.details[0].starts_with("Odoo 19.0: Python 3.11.1"));
    assert!(some.details[1].starts_with("Odoo 20.0: needs Python 3.12"));

    let all = python_check(
        &found(vec![
            interpreter("/usr/bin/python3.12", 12, "cpython"),
            interpreter("/usr/bin/pypy3", 12, "pypy"),
        ]),
        &reqs,
        None,
        false,
    );
    assert_eq!(all.status, Status::Ok, "{all:?}");
    assert!(all.fix.is_none());
    assert!(all.details.iter().any(|d| d.ends_with("pypy: not used")));

    // Only a too-new Python: an error, and the fix says why.
    let new = python_check(
        &found(vec![interpreter("/usr/bin/python3.15", 15, "cpython")]),
        &reqs,
        Some(PackageManager::Brew),
        false,
    );
    assert_eq!(new.status, Status::Error);
    let fix = new.fix.expect("fix");
    assert!(fix.text.contains("Newer versions"), "{fix:?}");
    assert_eq!(fix.commands, ["brew install python@3.14"]);
}

#[tokio::test]
async fn missing_everything_is_reported_with_fixes() {
    let report = run(&empty_options()).await;
    let ids: Vec<CheckId> = report.checks.iter().map(|c| c.id).collect();
    assert_eq!(
        ids,
        [
            CheckId::Postgresql,
            CheckId::Python,
            CheckId::Wkhtmltopdf,
            CheckId::Git,
            CheckId::Uv
        ]
    );
    assert_eq!(report.worst(), Status::Error);
    let pg = by_id(&report, CheckId::Postgresql);
    assert_eq!(pg.status, Status::Error);
    assert!(pg.fix.as_ref().is_some_and(|f| !f.commands.is_empty()));
    assert_eq!(by_id(&report, CheckId::Python).status, Status::Error);
    let wk = by_id(&report, CheckId::Wkhtmltopdf);
    assert_eq!(wk.status, Status::Warning);
    assert_eq!(
        wk.fix.as_ref().map(|f| f.commands.clone()),
        Some(vec!["sudo apt install wkhtmltopdf".to_owned()])
    );
    let git = by_id(&report, CheckId::Git);
    assert_eq!(git.status, Status::Warning);
    assert_eq!(
        git.fix.as_ref().map(|f| f.commands.clone()),
        Some(vec!["sudo apt install git".to_owned()])
    );
    let uv = by_id(&report, CheckId::Uv);
    assert_eq!(uv.status, Status::Info);
    assert!(uv.fix.as_ref().is_some_and(|f| f.url.is_some()));
}

#[tokio::test]
async fn tools_are_found_and_judged_by_their_version_output() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bin = dir.path().join("bin");
    fs::create_dir(&bin).expect("mkdir");
    script(&bin, "git", "echo 'git version 2.47.1'");
    script(&bin, "uv", "echo 'uv 0.9.5 (d5f39331a 2025-10-21)'");
    script(&bin, "wkhtmltopdf", "echo 'wkhtmltopdf 0.12.6'");
    let mut opts = empty_options();
    opts.path_var = Some(bin.clone().into_os_string());

    let report = run(&opts).await;
    let git = by_id(&report, CheckId::Git);
    assert_eq!(git.status, Status::Ok, "{git:?}");
    assert_eq!(git.version.as_deref(), Some("2.47.1"));
    assert_eq!(
        git.location.as_deref(),
        Some(bin.join("git").to_str().expect("utf-8"))
    );
    assert!(git.fix.is_none());
    let uv = by_id(&report, CheckId::Uv);
    assert_eq!(
        (uv.status, uv.version.as_deref()),
        (Status::Ok, Some("0.9.5"))
    );
    let wk = by_id(&report, CheckId::Wkhtmltopdf);
    assert_eq!(wk.status, Status::Warning, "unpatched Qt: {wk:?}");
    assert!(wk.summary.contains("patched Qt"));

    script(
        &bin,
        "wkhtmltopdf",
        "echo 'wkhtmltopdf 0.12.6.1 (with patched qt)'",
    );
    script(&bin, "git", "echo broken >&2; exit 3");
    let report = run(&opts).await;
    let wk = by_id(&report, CheckId::Wkhtmltopdf);
    assert_eq!(
        (wk.status, wk.version.as_deref()),
        (Status::Ok, Some("0.12.6.1"))
    );
    assert!(wk.fix.is_none());
    let git = by_id(&report, CheckId::Git);
    assert_eq!(git.status, Status::Warning);
    assert!(git.summary.contains("does not run"), "{git:?}");
    assert!(git.details[0].contains("broken"), "{git:?}");

    script(&bin, "wkhtmltopdf", "echo 'wkhtmltopdf 0.11.0 rc1'");
    let report = run(&opts).await;
    let wk = by_id(&report, CheckId::Wkhtmltopdf);
    assert_eq!(wk.status, Status::Warning);
    assert!(wk.summary.contains("needs 0.12"), "{wk:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn postgres_is_checked_against_a_real_server() {
    let Some(pg) = TempPg::start() else {
        eprintln!("skipped: no PostgreSQL server binaries");
        return;
    };
    let timeout = Duration::from_secs(10);
    let reqs = Requirement::builtin();
    let pm = Some(PackageManager::Apt);
    let check = |user: Option<&'static str>| {
        let target = pg.target(user);
        let reqs = reqs.clone();
        async move { postgres::check(&target, None, &reqs, pm, timeout).await }
    };

    // The OS user is the cluster's superuser.
    let me = check(None).await;
    assert_eq!(me.id, CheckId::Postgresql);
    assert!(me.version.is_some(), "{me:?}");
    assert!(
        me.location
            .as_deref()
            .is_some_and(|l| l.starts_with("Unix socket in ")),
        "{me:?}"
    );
    assert!(
        me.details.iter().any(|d| d.ends_with("is a superuser")),
        "{me:?}"
    );
    if me.status == Status::Ok {
        assert!(me.fix.is_none());
    } else {
        // An old server on the test machine.
        assert!(me.summary.contains("too old"), "{me:?}");
    }

    pg.sql("CREATE ROLE plain LOGIN; CREATE ROLE maker LOGIN CREATEDB; CREATE ROLE pw LOGIN PASSWORD 'secret'")
        .await;

    let plain = check(Some("plain")).await;
    assert!(plain.status >= Status::Warning, "{plain:?}");
    assert!(
        plain
            .details
            .iter()
            .any(|d| d.contains("may not create databases"))
    );
    if !plain.summary.contains("too old") {
        let fix = plain.fix.expect("fix");
        assert_eq!(
            fix.commands,
            ["sudo -u postgres psql -c 'ALTER ROLE \"plain\" CREATEDB'"]
        );
    }

    let maker = check(Some("maker")).await;
    assert!(
        maker
            .details
            .iter()
            .any(|d| d.ends_with("may create databases")),
        "{maker:?}"
    );

    let missing = check(Some("nobody_here")).await;
    assert_eq!(missing.status, Status::Error, "{missing:?}");
    assert!(
        missing.summary.contains("no role nobody_here"),
        "{missing:?}"
    );
    assert_eq!(
        missing.fix.expect("fix").commands,
        ["sudo -u postgres createuser --createdb nobody_here"]
    );

    let pw = check(Some("pw")).await;
    assert_eq!(pw.status, Status::Warning, "{pw:?}");
    assert!(pw.summary.contains("asks role pw for a password"), "{pw:?}");
    assert!(pw.version.is_none());
}

#[tokio::test]
async fn a_socket_without_a_server_is_unreachable() {
    let dir = tempfile::tempdir().expect("tempdir");
    // A stale socket file: exists, nothing listens.
    let listener =
        std::os::unix::net::UnixListener::bind(dir.path().join(".s.PGSQL.5432")).expect("bind");
    drop(listener);
    let target = PgTarget {
        socket_dirs: vec![dir.path().to_path_buf(), dir.path().join("missing")],
        port: 5432,
        ..PgTarget::nowhere()
    };
    let c = postgres::check(
        &target,
        None,
        &Requirement::builtin(),
        Some(PackageManager::Dnf),
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(c.status, Status::Error, "{c:?}");
    assert!(
        c.details.iter().any(|d| d.contains("connection refused")),
        "{c:?}"
    );
    // Installed or not (depends on the machine), the fix starts the server.
    assert!(
        c.fix
            .expect("fix")
            .commands
            .contains(&"sudo systemctl enable --now postgresql".to_owned())
    );
}
