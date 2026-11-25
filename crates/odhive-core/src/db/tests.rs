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
