use std::time::Duration;

use super::*;
use crate::conf::OdooOptions;
use crate::envcheck::Status;
use crate::project::EnvVars;
use crate::testpg::TempPg;

const TIMEOUT: Duration = Duration::from_secs(10);

fn options(pairs: &[(&str, &str)]) -> OdooOptions {
    OdooOptions(
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
    )
}

/// A requirement no real server meets.
fn impossible() -> Requirement {
    let mut r = Requirement::builtin().remove(0);
    r.min_pg = 999;
    r
}

#[tokio::test(flavor = "multi_thread")]
async fn project_settings_reach_a_real_server() {
    let Some(pg) = TempPg::start() else {
        eprintln!("skipped: no PostgreSQL server binaries");
        return;
    };
    pg.sql(
        "CREATE ROLE pw LOGIN PASSWORD 'secret'; \
         CREATE ROLE maker LOGIN CREATEDB",
    )
    .await;
    let socket = pg.socket_dir.to_str().expect("utf-8 path");
    let project = |pairs: &[(&str, &str)]| {
        let mut all = vec![("db_host", socket), ("db_port", "5432")];
        all.extend_from_slice(pairs);
        PgTarget::for_project(&options(&all), &EnvVars::default())
    };

    // The project's password is sent.
    let ok = health(
        &project(&[("db_user", "pw"), ("db_password", "secret")]),
        None,
        None,
        TIMEOUT,
    )
    .await;
    let server = ok.server.as_ref().expect("logged in");
    assert_eq!(server.role, "pw");
    assert!(!server.superuser);
    assert!(!ok.can_create_db());
    assert!(server.version_num >= 100_000, "{server:?}");
    assert_eq!(ok.check.status, Status::Warning, "{:?}", ok.check);
    assert!(
        ok.check
            .details
            .iter()
            .any(|d| d.contains("may not create databases"))
    );
    assert!(ok.settings.password_set);
    assert_eq!(ok.settings.user, "pw");
    assert_eq!(ok.settings.socket_dirs, [socket]);
    assert!(!format!("{ok:?}").contains("secret"));
    assert!(!serde_json::to_string(&ok).unwrap().contains("secret"));

    let wrong = health(
        &project(&[("db_user", "pw"), ("db_password", "nope")]),
        None,
        None,
        TIMEOUT,
    )
    .await;
    assert_eq!(wrong.check.status, Status::Error, "{:?}", wrong.check);
    assert!(
        wrong
            .check
            .summary
            .contains("rejected the password for role pw")
    );
    assert!(wrong.server.is_none());

    let missing = health(&project(&[("db_user", "pw")]), None, None, TIMEOUT).await;
    assert_eq!(missing.check.status, Status::Warning, "{:?}", missing.check);
    assert!(
        missing
            .check
            .summary
            .contains("asks role pw for a password")
    );
    assert!(!missing.settings.password_set);

    // Trusted role with CREATEDB: a password set anyway is simply not asked for.
    let maker = health(
        &project(&[("db_user", "maker"), ("db_password", "unused")]),
        None,
        None,
        TIMEOUT,
    )
    .await;
    assert!(maker.can_create_db(), "{maker:?}");
    assert_eq!(maker.check.status, Status::Ok, "{:?}", maker.check);

    let old = health(
        &project(&[("db_user", "maker")]),
        Some(&impossible()),
        None,
        TIMEOUT,
    )
    .await;
    assert_eq!(old.check.status, Status::Error, "{:?}", old.check);
    assert!(old.check.summary.contains("too old"));
    assert!(old.server.is_some());
}

#[tokio::test]
async fn tls_only_modes_are_refused_without_connecting() {
    for mode in ["require", "verify-ca", "verify-full"] {
        let t = PgTarget::for_project(
            &options(&[("db_host", "127.0.0.1"), ("db_sslmode", mode)]),
            &EnvVars::default(),
        );
        let h = health(&t, None, None, TIMEOUT).await;
        assert_eq!(h.check.status, Status::Error);
        assert!(h.check.summary.contains(mode), "{:?}", h.check);
        assert!(h.check.fix.is_some());
        assert!(h.server.is_none());
    }
}

#[tokio::test]
async fn a_closed_port_is_unreachable() {
    // Bound then dropped: nothing listens there.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .expect("bind")
        .port()
        .to_string();
    let t = PgTarget::for_project(
        &options(&[("db_host", "127.0.0.1"), ("db_port", &port)]),
        &EnvVars::default(),
    );
    let h = health(&t, None, None, Duration::from_secs(5)).await;
    assert_eq!(h.check.status, Status::Error, "{:?}", h.check);
    assert!(h.check.location.is_none());
    assert!(
        h.check
            .details
            .iter()
            .any(|d| d.starts_with(&format!("127.0.0.1:{port}"))),
        "{:?}",
        h.check
    );
    assert!(h.server.is_none());
    assert!(!h.can_create_db());
}

/// Tables an Odoo database has, as far as listing looks.
const FAKE_ODOO: &str = "SET ROLE dev; CREATE TABLE ir_module_module (name varchar, state varchar, latest_version varchar, write_date timestamp); \
     INSERT INTO ir_module_module VALUES \
       ('base', 'installed', '19.0.1.3', '2026-01-02 03:04:05'), \
       ('sale', 'uninstalled', NULL, '2026-01-01 00:00:00'); \
     CREATE TABLE res_users (write_date timestamp); \
     INSERT INTO res_users VALUES ('2026-02-01 10:00:00'); \
     CREATE TABLE res_users_log (create_date timestamp); \
     INSERT INTO res_users_log VALUES ('2026-03-01 12:30:00')";

async fn sql_in(pg: &TempPg, db: &str, sql: &str) {
    let (client, conn) = tokio_postgres::Config::new()
        .host_path(&pg.socket_dir)
        .port(5432)
        .dbname(db)
        .connect(tokio_postgres::NoTls)
        .await
        .expect("connect");
    tokio::spawn(conn);
    client.batch_execute(sql).await.expect("sql");
}

#[tokio::test(flavor = "multi_thread")]
async fn lists_what_odoo_would_offer() {
    let Some(pg) = TempPg::start() else {
        eprintln!("skipped: no PostgreSQL server binaries");
        return;
    };
    // CREATE DATABASE cannot share a transaction: one statement each.
    for stmt in [
        "CREATE ROLE dev LOGIN CREATEDB",
        "CREATE ROLE other LOGIN",
        "CREATE DATABASE acme_main OWNER dev",
        "CREATE DATABASE acme_test OWNER dev",
        "CREATE DATABASE plain OWNER dev",
        "CREATE DATABASE \"odd name;--\" OWNER dev",
        "CREATE DATABASE theirs OWNER other",
        "CREATE DATABASE tmpl OWNER dev IS_TEMPLATE true",
        "CREATE DATABASE closed OWNER dev ALLOW_CONNECTIONS false",
        "CREATE DATABASE locked OWNER dev",
        "REVOKE CONNECT ON DATABASE locked FROM PUBLIC, dev",
    ] {
        pg.sql(stmt).await;
    }
    sql_in(&pg, "acme_main", FAKE_ODOO).await;
    // Half-made: modules table only, no base yet.
    sql_in(
        &pg,
        "acme_test",
        "SET ROLE dev; CREATE TABLE ir_module_module (name varchar, state varchar, latest_version varchar, write_date timestamp)",
    )
    .await;

    let target = pg.target(Some("dev"));
    let uses = [
        ("main".to_owned(), "acme_main".to_owned()),
        ("staging".to_owned(), "acme_main".to_owned()),
        ("gone".to_owned(), "dropped_db".to_owned()),
    ];
    let all = list(&target, None, &uses, None, TIMEOUT).await;
    assert!(all.error.is_none(), "{:?}", all.error);
    let names: Vec<&str> = all.databases.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(
        names,
        ["acme_main", "acme_test", "locked", "odd name;--", "plain"]
    );
    assert_eq!(all.hidden, 0);
    assert_eq!(all.settings.user, "dev");

    let by = |n: &str| all.databases.iter().find(|d| d.name == n).expect(n);
    let main = by("acme_main");
    assert!(
        main.odoo && main.valid_name && main.error.is_none(),
        "{main:?}"
    );
    assert_eq!(main.odoo_version.as_deref(), Some("19.0.1.3"));
    assert_eq!(main.odoo_series.as_deref(), Some("19.0"));
    assert_eq!(main.last_modified.as_deref(), Some("2026-03-01T12:30:00Z"));
    assert!(main.size_bytes.is_some_and(|s| s > 0));
    assert_eq!(main.instances, ["main", "staging"]);
    assert!(by("plain").instances.is_empty());

    let half = by("acme_test");
    assert!(!half.odoo && half.odoo_version.is_none() && half.error.is_none());
    assert_eq!(half.last_modified, None);

    let plain = by("plain");
    assert!(!plain.odoo && plain.error.is_none(), "{plain:?}");

    let odd = by("odd name;--");
    assert!(!odd.valid_name);
    assert!(odd.error.is_none(), "{odd:?}");

    // A table the role may not read.
    sql_in(
        &pg,
        "plain",
        "CREATE TABLE ir_module_module (name varchar, state varchar, latest_version varchar, write_date timestamp); REVOKE ALL ON ir_module_module FROM PUBLIC",
    )
    .await;
    let again = list(&target, Some("^plain$"), &[], None, TIMEOUT).await;
    let err = again.databases[0].error.as_deref().expect("error");
    assert!(err.contains("permission denied"), "{err}");

    let locked = by("locked");
    assert_eq!(locked.size_bytes, None);
    assert!(locked.error.is_some());

    let filtered = list(&target, Some("acme"), &[], None, TIMEOUT).await;
    let names: Vec<&str> = filtered.databases.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, ["acme_main", "acme_test"]);
    assert_eq!(filtered.hidden, 3);
    assert_eq!(filtered.filter.as_deref(), Some("acme"));
    assert!(filtered.filter_note.is_none());

    let by_host = list(&target, Some("^%d$"), &[], None, TIMEOUT).await;
    assert_eq!(by_host.databases.len(), 5);
    assert!(by_host.filter_note.is_some());

    // A role owning nothing sees nothing.
    let none = list(
        &pg.target(Some("other")),
        Some("^nomatch$"),
        &[],
        None,
        TIMEOUT,
    )
    .await;
    assert!(none.error.is_none(), "{:?}", none.error);
    assert!(none.databases.is_empty());
    assert_eq!(none.hidden, 1);
}

#[tokio::test]
async fn listing_an_unreachable_server_is_a_finding() {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .expect("bind")
        .port()
        .to_string();
    let t = PgTarget::for_project(
        &options(&[
            ("db_host", "127.0.0.1"),
            ("db_port", &port),
            ("db_password", "s3cret"),
        ]),
        &EnvVars::default(),
    );
    let l = list(&t, Some("x"), &[], None, Duration::from_secs(5)).await;
    let err = l.error.as_ref().expect("error");
    assert_eq!(err.status, Status::Error);
    assert!(l.databases.is_empty());
    assert!(!serde_json::to_string(&l).unwrap().contains("s3cret"));

    let tls = PgTarget::for_project(
        &options(&[("db_host", "127.0.0.1"), ("db_sslmode", "require")]),
        &EnvVars::default(),
    );
    let l = list(&tls, None, &[], None, TIMEOUT).await;
    assert!(l.error.expect("error").summary.contains("require"));
}

/// The `fake-odoo-bin` fixture, run by python3, pointed at `pg` through
/// libpq variables in the project env (like a real project's `[env]`).
fn fake_odoo(pg: &TempPg, dir: &std::path::Path, extra: &[(&str, &str)]) -> OdooCommand {
    let path = std::env::var_os("PATH").expect("PATH");
    let python = crate::python::find_executable(&path, "python3").expect("python3");
    let conf = dir.join("odoo.conf");
    std::fs::write(&conf, "[options]\n").expect("conf");
    let bin = TempPg::bin_dir().expect("pg bin");
    let mut env = EnvVars::default();
    for (k, v) in [
        ("PGHOST", pg.socket_dir.to_str().expect("utf-8")),
        ("PGPORT", "5432"),
        ("FAKE_PG_BIN", bin.to_str().expect("utf-8")),
    ]
    .iter()
    .chain(extra)
    {
        env.0.insert((*k).to_owned(), (*v).to_owned());
    }
    OdooCommand {
        python,
        odoo_bin: std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/fake-odoo-bin"),
        conf,
        cwd: dir.to_path_buf(),
        env,
    }
}

async fn query_db(pg: &TempPg, db: &str, sql: &str) -> Vec<String> {
    let (client, conn) = tokio_postgres::Config::new()
        .host_path(&pg.socket_dir)
        .port(5432)
        .dbname(db)
        .connect(tokio_postgres::NoTls)
        .await
        .expect("connect");
    tokio::spawn(conn);
    client
        .query(sql, &[])
        .await
        .expect("query")
        .iter()
        .map(|r| r.get(0))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_database_is_created_with_its_settings_and_modules() {
    let Some(pg) = TempPg::start() else {
        eprintln!("skipped: no PostgreSQL server binaries");
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let odoo = fake_odoo(&pg, dir.path(), &[]);
    let target = pg.target(None);
    let mut new = NewDatabase::new("shop_main");
    new.country = Some("be".into());
    new.demo = true;
    new.login = "boss@example.com".into();
    new.password = "Very-s3cret".into();
    new.modules = vec!["sale".into(), "stock".into()];
    let mut events = Vec::new();
    let made = create(&target, &odoo, &new, TIMEOUT, |e| events.push(e))
        .await
        .expect("create");
    assert_eq!(made.name, "shop_main");
    assert_eq!(made.country.as_deref(), Some("BE"));
    assert_eq!(made.modules, ["sale", "stock"]);

    let steps: Vec<CreateStep> = events
        .iter()
        .filter_map(|e| match e {
            CreateEvent::Step { step } => Some(*step),
            _ => None,
        })
        .collect();
    assert_eq!(
        steps,
        [
            CreateStep::Initialize,
            CreateStep::InstallModules,
            CreateStep::Finalize
        ]
    );
    assert!(events.contains(&CreateEvent::Module {
        name: "stock".into(),
        index: 2,
        total: 2
    }));
    for e in &events {
        if let CreateEvent::Output { line } = e {
            assert!(!line.contains("Very-s3cret"), "{line}");
        }
    }

    let settings = query_db(
        &pg,
        "shop_main",
        "SELECT key || '=' || value FROM odhive_fake ORDER BY key",
    )
    .await;
    assert_eq!(
        settings,
        [
            "country=BE",
            "demo=True",
            "lang=en_US",
            "login=boss@example.com",
            "password=Very-s3cret"
        ]
    );
    let listed = list(&target, None, &[], None, TIMEOUT).await;
    let db = listed
        .databases
        .iter()
        .find(|d| d.name == "shop_main")
        .expect("listed");
    assert_eq!(db.odoo_series.as_deref(), Some("19.0"));

    // The name is taken now.
    let again = create(&target, &odoo, &new, TIMEOUT, |_| {}).await;
    assert!(
        matches!(again, Err(CreateError::Exists(ref n)) if n == "shop_main"),
        "{again:?}"
    );
    assert!(exists(&target, "shop_main", TIMEOUT).await.expect("exists"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_create_removes_the_database() {
    let Some(pg) = TempPg::start() else {
        eprintln!("skipped: no PostgreSQL server binaries");
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let target = pg.target(None);

    // Odoo's `db init` exits 0 when initializing fails: the last step notices.
    let broken = fake_odoo(&pg, dir.path(), &[("FAKE_INIT_BROKEN", "1")]);
    let mut events = Vec::new();
    let err = create(
        &target,
        &broken,
        &NewDatabase::new("broken"),
        TIMEOUT,
        |e| {
            events.push(e);
        },
    )
    .await
    .expect_err("broken init");
    match &err {
        CreateError::Failed { step, tail, .. } => {
            assert_eq!(*step, CreateStep::Finalize);
            assert!(tail.contains("not as asked"), "{tail}");
        }
        other => panic!("{other:?}"),
    }
    assert!(events.contains(&CreateEvent::Step {
        step: CreateStep::Cleanup
    }));
    assert!(!exists(&target, "broken", TIMEOUT).await.expect("exists"));

    // A module Odoo does not know is ignored by `-i`: the last step notices.
    let odoo = fake_odoo(&pg, dir.path(), &[]);
    let mut new = NewDatabase::new("partial");
    new.modules = vec!["sale".into(), "missing_mod".into()];
    let err = create(&target, &odoo, &new, TIMEOUT, |_| {})
        .await
        .expect_err("unknown module");
    assert!(err.to_string().contains("missing_mod"), "{err}");
    assert!(!exists(&target, "partial", TIMEOUT).await.expect("exists"));

    // Bad input never reaches the server.
    let mut bad = NewDatabase::new("x;DROP DATABASE postgres");
    assert!(matches!(
        create(&PgTarget::nowhere(), &odoo, &bad, TIMEOUT, |_| {}).await,
        Err(CreateError::Name(_))
    ));
    bad.name = "fine".into();
    bad.modules = vec!["../evil".into()];
    assert!(matches!(
        create(&PgTarget::nowhere(), &odoo, &bad, TIMEOUT, |_| {}).await,
        Err(CreateError::Invalid {
            field: "modules",
            ..
        })
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_create_removes_the_database() {
    let Some(pg) = TempPg::start() else {
        eprintln!("skipped: no PostgreSQL server binaries");
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let target = pg.target(None);
    let odoo = fake_odoo(&pg, dir.path(), &[("FAKE_INSTALL_DELAY", "30")]);
    let mut new = NewDatabase::new("cancelled");
    new.modules = vec!["sale".into(), "stock".into()];
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    {
        let work = create(&target, &odoo, &new, TIMEOUT, move |e| {
            let _ = tx.send(e);
        });
        tokio::pin!(work);
        loop {
            tokio::select! {
                r = &mut work => panic!("finished: {r:?}"),
                Some(e) = rx.recv() => if matches!(e, CreateEvent::Module { .. }) { break },
            }
        }
        assert!(exists(&target, "cancelled", TIMEOUT).await.expect("exists"));
        // `work` is dropped here, as a cancelled job's work is.
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while exists(&target, "cancelled", TIMEOUT).await.expect("exists") {
        assert!(
            std::time::Instant::now() < deadline,
            "the cancelled database is still there"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
