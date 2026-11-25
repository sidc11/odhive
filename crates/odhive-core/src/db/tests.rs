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
