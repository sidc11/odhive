//! The OpenAPI document committed for the web UI (`web/src/lib/api/openapi.json`,
//! source of its TypeScript types) must match the server.
//!
//! After changing the API run `pnpm --dir web api:gen` (or this test with
//! `ODHIVE_UPDATE_OPENAPI=1`, then `pnpm --dir web api:types`).

use std::path::PathBuf;

#[test]
fn committed_document_is_up_to_date() {
    let file = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../web/src/lib/api/openapi.json");
    let current = odhive_server::api::openapi()
        .to_pretty_json()
        .expect("serializes")
        + "\n";
    if std::env::var_os("ODHIVE_UPDATE_OPENAPI").is_some() {
        std::fs::write(&file, &current).expect("write");
        return;
    }
    let committed = std::fs::read_to_string(&file).unwrap_or_default();
    assert!(
        committed == current,
        "{} is out of date; run `pnpm --dir web api:gen`",
        file.display()
    );
}

#[test]
fn every_operation_is_documented() {
    let doc = serde_json::to_value(odhive_server::api::openapi()).expect("json");
    let mut ids = Vec::new();
    for (path, item) in doc["paths"].as_object().expect("paths") {
        assert!(path.starts_with("/api/v1/"), "{path}");
        for (method, op) in item.as_object().expect("item") {
            let id = op["operationId"].as_str().expect("operationId");
            ids.push(id.to_owned());
            assert!(op["summary"].is_string(), "{method} {path}: no summary");
            for (code, resp) in op["responses"].as_object().expect("responses") {
                assert!(
                    resp["description"].is_string(),
                    "{method} {path} {code}: no description"
                );
            }
        }
    }
    let n = ids.len();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), n, "operation ids must be unique");
}
