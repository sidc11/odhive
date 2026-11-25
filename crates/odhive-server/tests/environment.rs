//! `GET /environment`: the environment check over the API.

use std::time::Duration;

use axum::http::StatusCode;
use odhive_core::envcheck::{CheckOptions, PgTarget};
use odhive_core::python::DiscoverOptions;
use odhive_core::syslib::PackageManager;
use odhive_server::service::ServiceOptions;
use serde_json::Value;

mod common;
use common::{Env, python3};

/// Only the test's `python3`, no tools on `PATH`, no PostgreSQL.
fn isolated() -> ServiceOptions {
    let discover = DiscoverOptions {
        path_var: None,
        uv: None,
        explicit: vec![python3()],
        timeout: Duration::from_secs(30),
    };
    ServiceOptions {
        env: CheckOptions {
            path_var: None,
            discover: discover.clone(),
            postgres: PgTarget::nowhere(),
            requirements: Vec::new(),
            package_manager: Some(PackageManager::Pacman),
            timeout: Duration::from_secs(5),
        },
        discover,
        ..ServiceOptions::default()
    }
}

fn check<'a>(report: &'a Value, id: &str) -> &'a Value {
    report["checks"]
        .as_array()
        .expect("checks")
        .iter()
        .find(|c| c["id"] == id)
        .unwrap_or_else(|| panic!("no {id} check in {report}"))
}

#[tokio::test]
async fn every_check_is_reported_with_fixes() {
    let env = Env::new();
    let api = env.app_with(Default::default(), isolated(), common::PORT);

    let (status, report) = api.get("/api/v1/environment").await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert_eq!(report["package_manager"], "pacman");
    let ids: Vec<&str> = report["checks"]
        .as_array()
        .expect("checks")
        .iter()
        .filter_map(|c| c["id"].as_str())
        .collect();
    assert_eq!(ids, ["postgresql", "python", "wkhtmltopdf", "git", "uv"]);

    let pg = check(&report, "postgresql");
    assert_eq!(pg["status"], "error", "{pg}");
    assert!(
        pg["fix"]["commands"].as_array().is_some_and(|c| c
            .iter()
            .any(|c| c.as_str().is_some_and(|c| c.contains("postgresql")))),
        "{pg}"
    );
    assert_eq!(check(&report, "git")["status"], "warning");
    assert_eq!(check(&report, "uv")["status"], "info");

    // Without sources the Python check judges Odoo 19 and 20.
    let python = check(&report, "python");
    let details = python["details"].as_array().expect("details");
    assert!(
        details[0]
            .as_str()
            .is_some_and(|d| d.starts_with("Odoo 19.0:"))
    );
    assert!(
        details[1]
            .as_str()
            .is_some_and(|d| d.starts_with("Odoo 20.0:"))
    );

    // A registered source replaces them.
    let source = api.add_source(&env.source_dir()).await;
    let (status, report) = api.get("/api/v1/environment").await;
    assert_eq!(status, StatusCode::OK, "{report}");
    let details = check(&report, "python")["details"]
        .as_array()
        .expect("details")
        .clone();
    assert!(
        details[0]
            .as_str()
            .is_some_and(|d| d.contains(&format!("({source}):"))),
        "{details:?}"
    );
    assert!(
        !details
            .iter()
            .any(|d| d.as_str().is_some_and(|d| d.starts_with("Odoo 20.0:")))
    );
}
