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

// ---- restore ----

/// A source database that looks like Odoo 19 to OdHive: `base` installed,
/// a table with rows.
async fn odoo_like_db(pg: &TempPg, name: &str) {
    pg.sql(&format!("CREATE DATABASE {name}")).await;
    sql_in(
        pg,
        name,
        "CREATE TABLE ir_module_module (name text PRIMARY KEY, state text, \
             latest_version text, write_date timestamp DEFAULT now()); \
         INSERT INTO ir_module_module (name, state, latest_version) \
             VALUES ('base', 'installed', '19.0.1.3'); \
         CREATE TABLE ir_config_parameter (key text PRIMARY KEY, value text); \
         CREATE TABLE res_partner (id serial PRIMARY KEY, name text); \
         INSERT INTO res_partner (name) VALUES ('Azure'), ('Deco')",
    )
    .await;
}

/// `pg_dump` of `db` in `format` (`p`, `c` or `t`) to `out`.
fn pg_dump(pg: &TempPg, db: &str, format: &str, out: &std::path::Path) {
    let bin = TempPg::bin_dir().expect("pg bin");
    let done = std::process::Command::new(bin.join("pg_dump"))
        .args(["--no-owner", &format!("--format={format}"), "-f"])
        .arg(out)
        .arg(db)
        .env("PGHOST", &pg.socket_dir)
        .env("PGPORT", "5432")
        .output()
        .expect("pg_dump");
    assert!(done.status.success(), "pg_dump: {done:?}");
}

/// An Odoo backup zip of `db`: `dump.sql`, `manifest.json`, a filestore.
fn backup_zip(pg: &TempPg, db: &str, version: &str, out: &std::path::Path, dir: &std::path::Path) {
    use std::io::Write;
    use zip::write::SimpleFileOptions;
    let sql = dir.join(format!("{db}.plain.sql"));
    pg_dump(pg, db, "p", &sql);
    let mut w = zip::ZipWriter::new(std::fs::File::create(out).expect("create"));
    let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    w.start_file("dump.sql", opts).expect("start");
    w.write_all(&std::fs::read(&sql).expect("read"))
        .expect("write");
    w.start_file("manifest.json", opts).expect("start");
    write!(
        w,
        r#"{{"odoo_dump": "1", "db_name": "{db}", "version": "{version}", "pg_version": "18.0"}}"#
    )
    .expect("write");
    w.start_file("filestore/ab/ab12cd", opts).expect("start");
    w.write_all(b"attachment bytes").expect("write");
    w.finish().expect("finish");
}

struct RestoreSetup {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
    odoo: OdooCommand,
    tools: PgTools,
    target: PgTarget,
}

impl RestoreSetup {
    async fn new(pg: &TempPg, extra: &[(&str, &str)]) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        let odoo = fake_odoo(pg, &root, extra);
        std::fs::write(
            &odoo.conf,
            format!("[options]\ndata_dir = {}\n", root.join("data").display()),
        )
        .expect("conf");
        let bin = TempPg::bin_dir().expect("pg bin");
        let tools = PgTools::find(None, Some(bin.as_os_str()), TIMEOUT)
            .await
            .expect("tools");
        Self {
            _dir: dir,
            root,
            odoo,
            tools,
            target: pg.target(None),
        }
    }

    fn options(&self, series: &str) -> RestoreOptions {
        let staging = self
            .root
            .join("data")
            .join(format!(".staging-{}", rand_suffix()));
        std::fs::create_dir_all(&staging).expect("staging");
        RestoreOptions {
            filestore_root: self.root.join("data").join("filestore"),
            staging,
            series: Some(series.into()),
            template: "template0".into(),
            limits: archive::ExtractLimits::default(),
            timeout: TIMEOUT,
        }
    }

    async fn restore(
        &self,
        dump: &std::path::Path,
        req: &RestoreRequest,
        series: &str,
    ) -> (Result<RestoredDatabase, RestoreError>, Vec<RestoreEvent>) {
        let mut events = Vec::new();
        let r = restore(
            &self.target,
            &self.odoo,
            &self.tools,
            dump,
            req,
            &self.options(series),
            |e| events.push(e),
        )
        .await;
        (r, events)
    }
}

fn rand_suffix() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

async fn param(pg: &TempPg, db: &str, key: &str) -> Vec<String> {
    query_db(
        pg,
        db,
        &format!("SELECT value FROM ir_config_parameter WHERE key = '{key}'"),
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_backup_zip_is_restored_neutralized_with_its_filestore() {
    let Some(pg) = TempPg::start() else {
        eprintln!("skipped: no PostgreSQL server binaries");
        return;
    };
    let s = RestoreSetup::new(&pg, &[]).await;
    odoo_like_db(&pg, "prod").await;
    let zip = s.root.join("prod.zip");
    backup_zip(&pg, "prod", "19.0", &zip, &s.root);

    let (r, events) = s
        .restore(&zip, &RestoreRequest::new("prod_copy"), "19.0")
        .await;
    if !s.tools.psql_can_restrict() {
        assert!(matches!(r, Err(RestoreError::PsqlTooOld { .. })), "{r:?}");
        return;
    }
    let done = r.expect("restored");
    assert_eq!(done.format, archive::DumpFormat::Zip);
    assert_eq!(done.odoo_version.as_deref(), Some("19.0.1.3"));
    assert!(done.neutralized && done.new_uuid);
    assert_eq!(done.filestore_files, 1);
    assert_eq!(done.source_name.as_deref(), Some("prod"));
    let steps: Vec<RestoreStep> = events
        .iter()
        .filter_map(|e| match e {
            RestoreEvent::Step { step } => Some(*step),
            _ => None,
        })
        .collect();
    assert_eq!(
        steps,
        [
            RestoreStep::Inspect,
            RestoreStep::Create,
            RestoreStep::Load,
            RestoreStep::Check,
            RestoreStep::Neutralize,
            RestoreStep::Filestore,
            RestoreStep::Finalize
        ]
    );
    assert_eq!(
        query_db(&pg, "prod_copy", "SELECT name FROM res_partner ORDER BY id").await,
        ["Azure", "Deco"]
    );
    assert_eq!(param(&pg, "prod_copy", NEUTRALIZED_KEY).await, ["true"]);
    assert_eq!(param(&pg, "prod_copy", "database.uuid").await.len(), 1);
    let file = s.root.join("data/filestore/prod_copy/ab/ab12cd");
    assert_eq!(
        std::fs::read(&file).expect("filestore"),
        b"attachment bytes"
    );
    // Collation as Odoo creates it.
    assert_eq!(
        query_db(
            &pg,
            "postgres",
            "SELECT datcollate::text FROM pg_database WHERE datname = 'prod_copy'"
        )
        .await,
        ["C"]
    );

    // Again under the same name: refused before touching anything.
    let (r, _) = s
        .restore(&zip, &RestoreRequest::new("prod_copy"), "19.0")
        .await;
    assert!(matches!(r, Err(RestoreError::Exists(_))), "{r:?}");
    // A leftover filestore folder blocks a name too.
    std::fs::create_dir_all(s.root.join("data/filestore/leftover")).expect("mkdir");
    let (r, _) = s
        .restore(&zip, &RestoreRequest::new("leftover"), "19.0")
        .await;
    assert!(matches!(r, Err(RestoreError::FilestoreExists(_))), "{r:?}");
    assert!(
        !exists(&s.target, "leftover", TIMEOUT)
            .await
            .expect("exists")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn custom_and_tar_archives_and_sql_scripts_are_restored() {
    let Some(pg) = TempPg::start() else {
        eprintln!("skipped: no PostgreSQL server binaries");
        return;
    };
    let s = RestoreSetup::new(&pg, &[]).await;
    odoo_like_db(&pg, "prod").await;
    for (format, file, want) in [
        ("c", "prod.dump", archive::DumpFormat::Custom),
        ("t", "prod.tar", archive::DumpFormat::Tar),
        ("p", "prod.sql", archive::DumpFormat::Sql),
    ] {
        let dump = s.root.join(file);
        pg_dump(&pg, "prod", format, &dump);
        let name = format!("from_{format}");
        let req = RestoreRequest {
            name: name.clone(),
            neutralize: false,
            copy: false,
        };
        let (r, _) = s.restore(&dump, &req, "19.0").await;
        if want == archive::DumpFormat::Sql && !s.tools.psql_can_restrict() {
            assert!(matches!(r, Err(RestoreError::PsqlTooOld { .. })), "{r:?}");
            continue;
        }
        let done = r.unwrap_or_else(|e| panic!("{file}: {e}"));
        assert_eq!(done.format, want);
        assert!(!done.neutralized && !done.new_uuid);
        assert_eq!(
            query_db(&pg, &name, "SELECT count(*)::text FROM res_partner").await,
            ["2"],
            "{file}"
        );
        // Neither neutralized nor given a new UUID.
        assert!(
            query_db(
                &pg,
                &name,
                "SELECT key FROM ir_config_parameter WHERE key IN \
                 ('database.is_neutralized', 'database.uuid')"
            )
            .await
            .is_empty()
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_restores_remove_the_database_and_filestore() {
    let Some(pg) = TempPg::start() else {
        eprintln!("skipped: no PostgreSQL server binaries");
        return;
    };
    odoo_like_db(&pg, "prod").await;
    let s = RestoreSetup::new(&pg, &[]).await;
    if !s.tools.psql_can_restrict() {
        eprintln!("skipped: psql cannot restrict meta-commands");
        return;
    }
    let zip = s.root.join("prod.zip");
    backup_zip(&pg, "prod", "19.0", &zip, &s.root);

    // Another series, from the manifest: refused before creating anything.
    let (r, events) = s.restore(&zip, &RestoreRequest::new("other"), "20.0").await;
    assert!(
        matches!(&r, Err(RestoreError::VersionMismatch { dump, project }) if dump == "19.0" && project == "20.0"),
        "{r:?}"
    );
    assert!(!events.contains(&RestoreEvent::Step {
        step: RestoreStep::Create
    }));
    assert!(!exists(&s.target, "other", TIMEOUT).await.expect("exists"));
    // From the database itself when there is no manifest.
    let custom = s.root.join("prod.dump");
    pg_dump(&pg, "prod", "c", &custom);
    let (r, _) = s
        .restore(&custom, &RestoreRequest::new("other"), "20.0")
        .await;
    assert!(
        matches!(r, Err(RestoreError::VersionMismatch { .. })),
        "{r:?}"
    );
    assert!(!exists(&s.target, "other", TIMEOUT).await.expect("exists"));

    // The registry does not load: database and filestore removed.
    let broken = RestoreSetup::new(&pg, &[("FAKE_SHELL_BROKEN", "1")]).await;
    let zip2 = broken.root.join("prod.zip");
    std::fs::copy(&zip, &zip2).expect("copy");
    let (r, events) = broken
        .restore(&zip2, &RestoreRequest::new("broken"), "19.0")
        .await;
    assert!(
        matches!(
            r,
            Err(RestoreError::Failed {
                step: RestoreStep::Finalize,
                ..
            })
        ),
        "{r:?}"
    );
    assert!(events.contains(&RestoreEvent::Step {
        step: RestoreStep::Cleanup
    }));
    assert!(
        !exists(&broken.target, "broken", TIMEOUT)
            .await
            .expect("exists")
    );
    assert!(!broken.root.join("data/filestore/broken").exists());

    // Neutralization that did not take is a failure, never a silent success.
    let unneutral = RestoreSetup::new(&pg, &[("FAKE_NEUTRALIZE_BROKEN", "1")]).await;
    let (r, _) = unneutral
        .restore(&custom, &RestoreRequest::new("unneutral"), "19.0")
        .await;
    assert!(matches!(r, Err(RestoreError::NotNeutralized)), "{r:?}");
    assert!(
        !exists(&unneutral.target, "unneutral", TIMEOUT)
            .await
            .expect("exists")
    );

    // Not an Odoo database.
    let sql = s.root.join("plain.sql");
    std::fs::write(&sql, "CREATE TABLE t (x int);\n").expect("write");
    let (r, _) = s
        .restore(&sql, &RestoreRequest::new("notodoo"), "19.0")
        .await;
    assert!(matches!(r, Err(RestoreError::NotOdoo)), "{r:?}");
    assert!(!exists(&s.target, "notodoo", TIMEOUT).await.expect("exists"));

    // Bad names never reach a tool.
    for bad in ["a;b", "x y", "x'--", "-x", "a\"b"] {
        let (r, events) = s.restore(&sql, &RestoreRequest::new(bad), "19.0").await;
        assert!(matches!(r, Err(RestoreError::Request(_))), "{bad:?}: {r:?}");
        assert!(events.is_empty());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_script_cannot_run_psql_meta_commands() {
    let Some(pg) = TempPg::start() else {
        eprintln!("skipped: no PostgreSQL server binaries");
        return;
    };
    let s = RestoreSetup::new(&pg, &[]).await;
    if !s.tools.psql_can_restrict() {
        eprintln!("skipped: psql cannot restrict meta-commands");
        return;
    }
    let marker = s.root.join("pwned");
    let sql = s.root.join("evil.sql");
    std::fs::write(
        &sql,
        format!(
            "\\! touch {}\n\\unrestrict guess\n\\! touch {}\n\
             CREATE TABLE ir_module_module (name text, state text, latest_version text, write_date timestamp);\n\
             INSERT INTO ir_module_module VALUES ('base', 'installed', '19.0.1.3', now());\n",
            marker.display(),
            marker.display()
        ),
    )
    .expect("write");
    let req = RestoreRequest {
        neutralize: false,
        ..RestoreRequest::new("evil")
    };
    let (r, _) = s.restore(&sql, &req, "19.0").await;
    assert!(!marker.exists(), "a meta-command ran");
    // The SQL itself still ran.
    r.expect("restored");
}

// ---- delete ----

#[tokio::test(flavor = "multi_thread")]
async fn a_database_is_deleted_with_its_filestore() {
    let Some(pg) = TempPg::start() else {
        eprintln!("skipped: no PostgreSQL server binaries");
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let target = pg.target(None);
    let odoo = fake_odoo(&pg, dir.path(), &[]);
    create(&target, &odoo, &NewDatabase::new("doomed"), TIMEOUT, |_| {})
        .await
        .expect("create");
    // The fake's conf has no data_dir, so Odoo's drop leaves this filestore:
    // OdHive removes it.
    let root = dir.path().join("filestore");
    std::fs::create_dir_all(root.join("doomed/ab")).expect("mkdir");
    std::fs::write(root.join("doomed/ab/cafe"), "x").expect("write");
    // A connection left open (an Odoo instance) does not block the drop.
    let (client, conn) = tokio_postgres::Config::new()
        .host_path(&pg.socket_dir)
        .port(5432)
        .dbname("doomed")
        .connect(tokio_postgres::NoTls)
        .await
        .expect("connect");
    let held = tokio::spawn(conn);

    let mut events = Vec::new();
    let done = delete(&target, &odoo, "doomed", &root, TIMEOUT, |e| events.push(e))
        .await
        .expect("delete");
    assert_eq!(
        done,
        DeletedDatabase {
            name: "doomed".into(),
            filestore_removed: true
        }
    );
    assert!(!exists(&target, "doomed", TIMEOUT).await.expect("exists"));
    assert!(root.join("doomed").symlink_metadata().is_err());
    assert_eq!(
        events
            .iter()
            .filter_map(|e| match e {
                DeleteEvent::Step { step } => Some(*step),
                DeleteEvent::Output { .. } => None,
            })
            .collect::<Vec<_>>(),
        [DeleteStep::Drop, DeleteStep::Check]
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, DeleteEvent::Output { line } if line.contains("DROP DB: doomed")))
    );
    drop(client);
    let _ = held.await;

    // Gone already: nothing to delete.
    assert!(matches!(
        delete(&target, &odoo, "doomed", &root, TIMEOUT, |_| {}).await,
        Err(DeleteError::NotFound(_))
    ));
    // Bad names never reach the server.
    for name in ["x;DROP DATABASE postgres", "a b", "-x", "a'b", ""] {
        assert!(
            matches!(
                delete(&PgTarget::nowhere(), &odoo, name, &root, TIMEOUT, |_| {}).await,
                Err(DeleteError::Name(_))
            ),
            "{name:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_drop_keeps_the_database_and_filestore() {
    let Some(pg) = TempPg::start() else {
        eprintln!("skipped: no PostgreSQL server binaries");
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let target = pg.target(None);
    let odoo = fake_odoo(&pg, dir.path(), &[]);
    create(&target, &odoo, &NewDatabase::new("kept"), TIMEOUT, |_| {})
        .await
        .expect("create");
    let root = dir.path().join("filestore");
    std::fs::create_dir_all(root.join("kept")).expect("mkdir");

    let broken = fake_odoo(&pg, dir.path(), &[("FAKE_DROP_BROKEN", "1")]);
    let err = delete(&target, &broken, "kept", &root, TIMEOUT, |_| {})
        .await
        .expect_err("broken drop");
    match &err {
        DeleteError::Failed { name, tail } => {
            assert_eq!(name, "kept");
            assert!(tail.contains("broken on purpose"), "{tail}");
        }
        other => panic!("{other:?}"),
    }
    assert!(exists(&target, "kept", TIMEOUT).await.expect("exists"));
    assert!(root.join("kept").is_dir());
}

// ---- duplicate ----

/// A fake-Odoo database `name` with a UUID and a filestore under
/// `dir/filestore` (the conf's `data_dir` is `dir`, so Odoo copies it).
async fn duplicate_setup(
    pg: &TempPg,
    dir: &std::path::Path,
    extra: &[(&str, &str)],
    name: &str,
) -> (PgTarget, OdooCommand, std::path::PathBuf) {
    let target = pg.target(None);
    let odoo = fake_odoo(pg, dir, extra);
    std::fs::write(
        &odoo.conf,
        format!("[options]\ndata_dir = {}\n", dir.display()),
    )
    .expect("conf");
    create(&target, &odoo, &NewDatabase::new(name), TIMEOUT, |_| {})
        .await
        .expect("create");
    sql_in(
        pg,
        name,
        "CREATE TABLE ir_config_parameter (key text PRIMARY KEY, value text); \
         INSERT INTO ir_config_parameter VALUES ('database.uuid', 'source-uuid')",
    )
    .await;
    let root = dir.join("filestore");
    std::fs::create_dir_all(root.join(name).join("ab")).expect("mkdir");
    std::fs::write(root.join(name).join("ab/cafe"), "attachment").expect("write");
    (target, odoo, root)
}

fn dup(source: &str, name: &str, neutralize: bool) -> DuplicateRequest {
    DuplicateRequest {
        source: source.into(),
        name: name.into(),
        neutralize,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_database_is_duplicated_with_its_filestore() {
    let Some(pg) = TempPg::start() else {
        eprintln!("skipped: no PostgreSQL server binaries");
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let (target, odoo, root) = duplicate_setup(&pg, dir.path(), &[], "orig").await;
    // A connection left open (an Odoo instance) does not block the copy.
    let (client, conn) = tokio_postgres::Config::new()
        .host_path(&pg.socket_dir)
        .port(5432)
        .dbname("orig")
        .connect(tokio_postgres::NoTls)
        .await
        .expect("connect");
    let held = tokio::spawn(conn);

    let mut events = Vec::new();
    let done = duplicate(
        &target,
        &odoo,
        &dup("orig", "orig_copy", true),
        &root,
        TIMEOUT,
        |e| events.push(e),
    )
    .await
    .expect("duplicate");
    assert_eq!(
        done,
        DuplicatedDatabase {
            name: "orig_copy".into(),
            source: "orig".into(),
            neutralized: true,
            filestore_copied: true,
        }
    );
    assert_eq!(
        events
            .iter()
            .filter_map(|e| match e {
                DuplicateEvent::Step { step } => Some(*step),
                DuplicateEvent::Output { .. } => None,
            })
            .collect::<Vec<_>>(),
        [DuplicateStep::Copy, DuplicateStep::Check]
    );
    assert!(events.iter().any(
        |e| matches!(e, DuplicateEvent::Output { line } if line.contains("Duplicate database"))
    ));
    drop(client);
    let _ = held.await;

    // Same tables and rows, new UUID, neutralized; the source is untouched.
    let counts = "SELECT string_agg(t || '=' || n, ',' ORDER BY t) FROM ( \
                  SELECT 'ir_module_module' t, count(*) n FROM ir_module_module UNION ALL \
                  SELECT 'odhive_fake', count(*) FROM odhive_fake UNION ALL \
                  SELECT 'tables', count(*) FROM pg_tables WHERE schemaname = 'public') c";
    let source_counts = query_db(&pg, "orig", counts).await;
    assert_eq!(query_db(&pg, "orig_copy", counts).await, source_counts);
    assert!(
        source_counts[0].contains("odhive_fake=5"),
        "{source_counts:?}"
    );
    let uuid = param(&pg, "orig_copy", "database.uuid").await;
    assert_eq!(uuid.len(), 1);
    assert_ne!(uuid[0], "source-uuid");
    assert_eq!(
        param(&pg, "orig_copy", "database.is_neutralized").await,
        ["true"]
    );
    assert_eq!(param(&pg, "orig", "database.uuid").await, ["source-uuid"]);
    assert!(
        param(&pg, "orig", "database.is_neutralized")
            .await
            .is_empty()
    );
    assert_eq!(
        std::fs::read_to_string(root.join("orig_copy/ab/cafe")).expect("copied"),
        "attachment"
    );
    assert!(root.join("orig/ab/cafe").is_file());

    // Refused before anything runs.
    assert!(matches!(
        duplicate(
            &target,
            &odoo,
            &dup("orig", "orig_copy", false),
            &root,
            TIMEOUT,
            |_| {}
        )
        .await,
        Err(DuplicateError::Exists(_))
    ));
    assert!(matches!(
        duplicate(
            &target,
            &odoo,
            &dup("gone", "gone_copy", false),
            &root,
            TIMEOUT,
            |_| {}
        )
        .await,
        Err(DuplicateError::NotFound(_))
    ));
    std::fs::create_dir_all(root.join("orig_two")).expect("mkdir");
    assert!(matches!(
        duplicate(
            &target,
            &odoo,
            &dup("orig", "orig_two", false),
            &root,
            TIMEOUT,
            |_| {}
        )
        .await,
        Err(DuplicateError::FilestoreExists(_))
    ));
    assert!(!exists(&target, "orig_two", TIMEOUT).await.expect("exists"));
    for name in ["x;DROP DATABASE postgres", "a b", "-x", "a'b", ""] {
        assert!(
            duplicate(
                &PgTarget::nowhere(),
                &odoo,
                &dup("orig", name, false),
                &root,
                TIMEOUT,
                |_| {}
            )
            .await
            .is_err_and(|e| matches!(e, DuplicateError::Name(_) | DuplicateError::Request(_))),
            "{name:?}"
        );
    }

    // Without neutralization and without a source filestore.
    std::fs::remove_dir_all(root.join("orig")).expect("rm");
    let plain = duplicate(
        &target,
        &odoo,
        &dup("orig", "orig_plain", false),
        &root,
        TIMEOUT,
        |_| {},
    )
    .await
    .expect("duplicate");
    assert!(!plain.neutralized && !plain.filestore_copied);
    assert!(
        param(&pg, "orig_plain", "database.is_neutralized")
            .await
            .is_empty()
    );
    assert!(root.join("orig_plain").symlink_metadata().is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_duplicates_remove_the_copy() {
    let Some(pg) = TempPg::start() else {
        eprintln!("skipped: no PostgreSQL server binaries");
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let (target, _, root) = duplicate_setup(&pg, dir.path(), &[], "src").await;
    for (flag, step, why) in [
        (
            "FAKE_DUPLICATE_BROKEN",
            DuplicateStep::Copy,
            "broken on purpose",
        ),
        (
            "FAKE_DUPLICATE_SAME_UUID",
            DuplicateStep::Check,
            "kept the source's database UUID",
        ),
    ] {
        let odoo = fake_odoo(&pg, dir.path(), &[(flag, "1")]);
        std::fs::write(
            &odoo.conf,
            format!("[options]\ndata_dir = {}\n", dir.path().display()),
        )
        .expect("conf");
        let mut steps = Vec::new();
        let err = duplicate(
            &target,
            &odoo,
            &dup("src", "bad_copy", false),
            &root,
            TIMEOUT,
            |e| {
                if let DuplicateEvent::Step { step } = e {
                    steps.push(step);
                }
            },
        )
        .await
        .expect_err(flag);
        match &err {
            DuplicateError::Failed { step: s, tail, .. } => {
                assert_eq!(*s, step, "{flag}");
                assert!(tail.contains(why), "{flag}: {tail}");
            }
            other => panic!("{flag}: {other:?}"),
        }
        assert_eq!(steps.last(), Some(&DuplicateStep::Cleanup));
        assert!(
            !exists(&target, "bad_copy", TIMEOUT).await.expect("exists"),
            "{flag}"
        );
        assert!(root.join("bad_copy").symlink_metadata().is_err(), "{flag}");
        assert!(exists(&target, "src", TIMEOUT).await.expect("exists"));
        assert!(root.join("src/ab/cafe").is_file());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_duplicate_removes_the_copy() {
    let Some(pg) = TempPg::start() else {
        eprintln!("skipped: no PostgreSQL server binaries");
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let (target, odoo, root) =
        duplicate_setup(&pg, dir.path(), &[("FAKE_DUPLICATE_DELAY", "30")], "src").await;
    {
        let req = dup("src", "cancelled_copy", false);
        let work = duplicate(&target, &odoo, &req, &root, TIMEOUT, |_| {});
        tokio::pin!(work);
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            tokio::select! {
                r = &mut work => panic!("finished: {r:?}"),
                () = tokio::time::sleep(Duration::from_millis(100)) => {}
            }
            if exists(&target, "cancelled_copy", TIMEOUT)
                .await
                .expect("exists")
            {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "no copy appeared");
        }
        // `work` is dropped here, as a cancelled job's work is.
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while exists(&target, "cancelled_copy", TIMEOUT)
        .await
        .expect("exists")
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the cancelled copy is still there"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(exists(&target, "src", TIMEOUT).await.expect("exists"));
}

/// A fake-made database `name` in `s`'s project, with one filestore file.
async fn backup_source(s: &RestoreSetup, name: &str) {
    create(&s.target, &s.odoo, &NewDatabase::new(name), TIMEOUT, |_| {})
        .await
        .expect("create");
    let fs = s.root.join("data/filestore").join(name).join("ab");
    std::fs::create_dir_all(&fs).expect("mkdir");
    std::fs::write(fs.join("cafe"), "attachment").expect("write");
}

async fn back_up(
    s: &RestoreSetup,
    database: &str,
    format: BackupFormat,
) -> (Result<Backup, BackupError>, Vec<BackupEvent>) {
    let mut events = Vec::new();
    let req = BackupRequest {
        database: database.into(),
        format,
    };
    let r = backup(
        &s.target,
        &s.odoo,
        &s.tools,
        &req,
        &s.root.join("backups"),
        TIMEOUT,
        |e| events.push(e),
    )
    .await;
    (r, events)
}

/// Everything in `backups/`, hidden entries too.
fn backups_dir_entries(s: &RestoreSetup) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(s.root.join("backups"))
        .map(|d| {
            d.map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

#[tokio::test(flavor = "multi_thread")]
async fn a_backup_survives_a_drop_and_restores_neutralized() {
    use std::os::unix::fs::PermissionsExt;
    let Some(pg) = TempPg::start() else {
        eprintln!("skipped: no PostgreSQL server binaries");
        return;
    };
    let s = RestoreSetup::new(&pg, &[]).await;
    backup_source(&s, "shop").await;

    let (r, events) = back_up(&s, "shop", BackupFormat::Zip).await;
    let zip = r.expect("zip backup");
    assert_eq!(zip.format, BackupFormat::Zip);
    assert_eq!(zip.database.as_deref(), Some("shop"));
    assert!(zip.file.starts_with("shop_") && zip.file.ends_with(".zip"));
    let steps: Vec<BackupStep> = events
        .iter()
        .filter_map(|e| match e {
            BackupEvent::Step { step } => Some(*step),
            BackupEvent::Output { .. } => None,
        })
        .collect();
    assert_eq!(steps, [BackupStep::Dump, BackupStep::Check]);
    // Odoo's temporary copy of the filestore went to the private staging
    // folder, not /tmp.
    let staging = s.root.join("backups").join(".odhive-backup-");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, BackupEvent::Output { line }
            if line.contains("temporary folder") && line.contains(staging.to_str().unwrap()))),
        "{events:?}"
    );
    let path = s.root.join("backups").join(&zip.file);
    let meta = std::fs::metadata(&path).expect("backup file");
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    assert_eq!(meta.len(), zip.size);
    let mut names: Vec<String> = zip::ZipArchive::new(std::fs::File::open(&path).expect("open"))
        .expect("zip")
        .file_names()
        .map(|n| n.expect("name").into_owned())
        .collect();
    names.sort();
    assert_eq!(names, ["dump.sql", "filestore/ab/cafe", "manifest.json"]);

    let (r, _) = back_up(&s, "shop", BackupFormat::Dump).await;
    let dump = r.expect("dump backup");
    assert_eq!(
        archive::detect(&s.root.join("backups").join(&dump.file)).expect("detect"),
        archive::DumpFormat::Custom
    );
    // Only the two backups are left: no staging folders.
    let mut want = vec![zip.file.clone(), dump.file.clone()];
    want.sort();
    assert_eq!(backups_dir_entries(&s), want);
    let listed = list_backups(&s.root.join("backups")).expect("list");
    assert_eq!(listed.len(), 2);
    assert!(listed.iter().all(|b| b.database.as_deref() == Some("shop")));

    let filestore_root = s.root.join("data/filestore");
    delete(&s.target, &s.odoo, "shop", &filestore_root, TIMEOUT, |_| {})
        .await
        .expect("drop");
    assert!(!exists(&s.target, "shop", TIMEOUT).await.expect("exists"));

    // The custom dump restores without psql.
    let (r, _) = s
        .restore(
            &s.root.join("backups").join(&dump.file),
            &RestoreRequest::new("shop_from_dump"),
            "19.0",
        )
        .await;
    r.expect("restore of the dump");
    assert_eq!(
        param(&pg, "shop_from_dump", NEUTRALIZED_KEY).await,
        ["true"]
    );

    let (r, _) = s.restore(&path, &RestoreRequest::new("shop"), "19.0").await;
    if !s.tools.psql_can_restrict() {
        assert!(matches!(r, Err(RestoreError::PsqlTooOld { .. })), "{r:?}");
        return;
    }
    let restored = r.expect("restore of the zip");
    assert!(restored.neutralized);
    assert_eq!(restored.filestore_files, 1);
    assert_eq!(param(&pg, "shop", NEUTRALIZED_KEY).await, ["true"]);
    assert_eq!(
        query_db(
            &pg,
            "shop",
            "SELECT value FROM odhive_fake WHERE key = 'login'"
        )
        .await,
        ["admin"]
    );
    assert_eq!(
        std::fs::read_to_string(filestore_root.join("shop/ab/cafe")).expect("filestore"),
        "attachment"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_backups_leave_nothing_behind() {
    let Some(pg) = TempPg::start() else {
        eprintln!("skipped: no PostgreSQL server binaries");
        return;
    };
    let s = RestoreSetup::new(&pg, &[]).await;
    backup_source(&s, "shop").await;
    let (r, _) = back_up(&s, "missing", BackupFormat::Zip).await;
    assert!(matches!(r, Err(BackupError::NotFound(_))), "{r:?}");
    let (r, _) = back_up(&s, "x;DROP DATABASE shop", BackupFormat::Dump).await;
    assert!(matches!(r, Err(BackupError::Name(_))), "{r:?}");

    for (flag, step) in [
        ("FAKE_DUMP_BROKEN", BackupStep::Dump),
        ("FAKE_DUMP_NOT_ZIP", BackupStep::Check),
    ] {
        let broken = RestoreSetup::new(&pg, &[(flag, "1")]).await;
        let (r, _) = back_up(&broken, "shop", BackupFormat::Zip).await;
        assert!(
            matches!(r, Err(BackupError::Failed { step: s, .. }) if s == step),
            "{flag}: {r:?}"
        );
        assert!(backups_dir_entries(&broken).is_empty(), "{flag}");
    }
    assert!(exists(&s.target, "shop", TIMEOUT).await.expect("exists"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_backup_leaves_nothing_behind() {
    let Some(pg) = TempPg::start() else {
        eprintln!("skipped: no PostgreSQL server binaries");
        return;
    };
    let s = RestoreSetup::new(&pg, &[("FAKE_DUMP_DELAY", "30")]).await;
    backup_source(&s, "shop").await;
    {
        let req = BackupRequest {
            database: "shop".into(),
            format: BackupFormat::Zip,
        };
        let dir = s.root.join("backups");
        let mut started = false;
        let work = backup(&s.target, &s.odoo, &s.tools, &req, &dir, TIMEOUT, |e| {
            started |= matches!(e, BackupEvent::Output { .. });
        });
        tokio::pin!(work);
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while !backups_dir_entries(&s)
            .iter()
            .any(|n| n.starts_with(".odhive-backup-"))
        {
            tokio::select! {
                r = &mut work => panic!("finished: {r:?}"),
                () = tokio::time::sleep(Duration::from_millis(100)) => {}
            }
            assert!(std::time::Instant::now() < deadline, "no staging folder");
        }
        // Let odoo-bin start, then drop `work` as a cancelled job does.
        tokio::select! {
            r = &mut work => panic!("finished: {r:?}"),
            () = tokio::time::sleep(Duration::from_millis(500)) => {}
        }
    }
    assert!(backups_dir_entries(&s).is_empty());
}
