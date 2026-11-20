use std::os::unix::fs::PermissionsExt;

use super::*;

fn v(s: &str) -> OdooVersion {
    s.parse().expect("version")
}

fn opt<'a>(schema: &'a OptionSchema, key: &str) -> &'a OdooOption {
    schema
        .get(key)
        .unwrap_or_else(|| panic!("missing option {key}"))
}

#[test]
fn bundled_schemas_parse_and_match_odoo() {
    for (version, count) in [("19.0", 89), ("20.0", 92)] {
        let s = bundled(v(version));
        assert_eq!(s.series, version);
        assert_eq!(s.options.len(), count, "{version}");

        let port = opt(&s, "http_port");
        assert_eq!(port.kind, OptionKind::Int);
        assert_eq!(port.flags, vec!["-p", "--http-port"]);
        assert_eq!(port.default, Some(Value::from(8069)));
        assert_eq!(port.metavar.as_deref(), Some("PORT"));
        assert_eq!(port.group.as_deref(), Some("HTTP Service Configuration"));

        // Config keys that differ from the flag name.
        assert_eq!(s.by_flag("--db-filter").expect("flag").key, "dbfilter");
        let http = opt(&s, "http_enable");
        assert_eq!(http.flags, vec!["--no-http"]);
        assert_eq!(http.kind, OptionKind::Bool);
        assert_eq!(http.default, Some(Value::Bool(true)));
        assert_eq!(http.metavar, None);

        // Two flags, one key.
        let demo = opt(&s, "with_demo");
        assert_eq!(demo.flags, vec!["--with-demo", "--without-demo"]);
        assert_eq!(demo.kind, OptionKind::Bool);
        let handler = opt(&s, "log_handler");
        assert_eq!(
            handler.flags,
            vec!["--log-handler", "--log-web", "--log-sql"]
        );
        assert_eq!(handler.kind, OptionKind::List);

        // File-only and secret options.
        let admin = opt(&s, "admin_passwd");
        assert!(admin.flags.is_empty() && admin.group.is_none() && admin.secret);
        assert!(opt(&s, "db_password").secret);
        assert!(opt(&s, "smtp_password").secret);
        assert!(!port.secret);

        // Options that cannot go in odoo.conf.
        assert!(!opt(&s, "config").file_loadable);
        assert!(!opt(&s, "init").file_loadable);

        assert_eq!(opt(&s, "addons_path").kind, OptionKind::List);
        assert_eq!(opt(&s, "data_dir").kind, OptionKind::Path);
        assert_eq!(opt(&s, "transient_age_limit").kind, OptionKind::Float);
        match &opt(&s, "log_level").kind {
            OptionKind::Choice { choices } => assert!(choices.iter().any(|c| c == "debug")),
            other => panic!("log_level: {other:?}"),
        }
        assert_eq!(s.groups().first(), Some(&None));
        assert!(s.groups().contains(&Some("Common options")));
    }
    let s20 = bundled(v("20.0"));
    assert!(s20.get("gevent_workers").is_some());
    assert!(bundled(v("19.0")).get("gevent_workers").is_none());
    assert_eq!(bundled(v("saas~19.2")).series, "19.0");
}

#[test]
fn schema_serializes_flat() {
    let s = bundled(v("19.0"));
    let json = serde_json::to_value(s.get("log_level").expect("opt")).expect("json");
    assert_eq!(json["type"], "choice");
    assert!(json["choices"].is_array());
    let back: OptionSchema =
        serde_json::from_str(&serde_json::to_string(&s).expect("ser")).expect("de");
    assert_eq!(back, s);
}

#[test]
fn rejects_unusable_raw_output() {
    for text in [
        "",
        "not json",
        r#"{"format": 2, "series": "19.0", "options": []}"#,
        r#"{"format": 1, "series": "19.0", "options": []}"#,
    ] {
        assert!(OptionSchema::from_raw_json(text).is_err(), "{text:?}");
    }
    let bad_key = r#"{"format": 1, "series": "19.0", "options": [{"dest": "../x", "flags": [],
        "type": null, "action": "store", "default": null, "help": null, "metavar": null,
        "choices": null, "group": null, "file_loadable": true, "file_exportable": true}]}"#;
    assert!(matches!(
        OptionSchema::from_raw_json(bad_key),
        Err(SchemaError::Invalid(_))
    ));
}

// --- introspection against a fake Odoo tree, using the system python3 ---

const FAKE_TOOLS: &str = r#"import optparse

class _Config:
    pass

config = _Config()
parser = optparse.OptionParser()
o = optparse.Option("--admin", dest="admin_passwd", help=optparse.SUPPRESS_HELP)
o.my_default = "admin"
parser.add_option(o)
group = optparse.OptionGroup(parser, "HTTP Service Configuration")
o = optparse.Option("-p", "--http-port", dest="http_port", type="int", metavar="PORT", help="port")
o.my_default = 8069
group.add_option(o)
group.add_option("--no-http", dest="http_enable", action="store_false", help="no http")
parser.add_option_group(group)
config.parser = parser
"#;

fn fake_odoo(dir: &Path, series: &str) -> PathBuf {
    fs::create_dir_all(dir.join("odoo/tools")).expect("mkdir");
    fs::write(dir.join("odoo/__init__.py"), "").expect("write");
    fs::write(
        dir.join("odoo/release.py"),
        format!("version_info = (19, 0, 0, 'final', 0, '')\nseries = {series:?}\n"),
    )
    .expect("write");
    fs::write(dir.join("odoo/tools/__init__.py"), FAKE_TOOLS).expect("write");
    fs::write(dir.join("odoo/tools/config.py"), "# fake\n").expect("write");
    fs::canonicalize(dir).expect("canonicalize")
}

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, body).expect("write");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

fn python() -> PathBuf {
    PathBuf::from("python3")
}

fn entry(path: &Path) -> SourceEntry {
    SourceEntry {
        id: "odoo-19.0".into(),
        edition: Edition::Community,
        odoo_version: v("19.0"),
        path: path.to_path_buf(),
    }
}

#[tokio::test]
async fn introspects_a_real_python_run() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = fake_odoo(&dir.path().join("odoo"), "19.0");
    let s = introspect(&python(), &src, DEFAULT_TIMEOUT)
        .await
        .expect("introspect");
    assert_eq!(s.series, "19.0");
    assert_eq!(s.options.len(), 3);
    assert_eq!(opt(&s, "http_port").default, Some(Value::from(8069)));
    assert_eq!(opt(&s, "http_enable").kind, OptionKind::Bool);
    let admin = opt(&s, "admin_passwd");
    assert!(admin.help.is_none() && admin.group.is_none() && admin.secret);
    // -B: nothing written into the source tree.
    assert!(!src.join("odoo/__pycache__").exists());
}

#[tokio::test]
async fn introspection_ignores_python_env_vars() {
    // -I must make PYTHONPATH irrelevant: a planted `json` module would otherwise
    // shadow the standard library one imported by the script.
    let dir = tempfile::tempdir().expect("tempdir");
    let src = fake_odoo(&dir.path().join("odoo"), "19.0");
    let evil = dir.path().join("evil");
    fs::create_dir_all(&evil).expect("mkdir");
    fs::write(evil.join("json.py"), "raise SystemExit('planted')\n").expect("write");
    let without_isolation = tokio::process::Command::new(python())
        .args(["-B", "-c", INTROSPECT_PY])
        .arg(&src)
        .env("PYTHONPATH", &evil)
        .output()
        .await
        .expect("run");
    assert!(
        !without_isolation.status.success(),
        "planted module not picked up"
    );
    let out = tokio::process::Command::new(python())
        .args(["-I", "-B", "-c", INTROSPECT_PY])
        .arg(&src)
        .env("PYTHONPATH", &evil)
        .output()
        .await
        .expect("run");
    assert!(out.status.success(), "{out:?}");
}

#[tokio::test]
async fn reports_failures_and_timeouts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = fake_odoo(&dir.path().join("odoo"), "19.0");

    let missing = dir.path().join("no-python");
    assert!(matches!(
        introspect(&missing, &src, DEFAULT_TIMEOUT).await,
        Err(SchemaError::Spawn { .. })
    ));

    let empty = dir.path().join("empty");
    fs::create_dir(&empty).expect("mkdir");
    match introspect(&python(), &empty, DEFAULT_TIMEOUT).await {
        Err(SchemaError::Failed { stderr, .. }) => assert!(stderr.contains("odoo"), "{stderr}"),
        other => panic!("expected Failed, got {other:?}"),
    }

    let slow = script(dir.path(), "slow-python", "#!/bin/sh\nsleep 5\n");
    assert!(matches!(
        introspect(&slow, &src, Duration::from_millis(200)).await,
        Err(SchemaError::Timeout(_))
    ));

    let garbage = script(dir.path(), "garbage-python", "#!/bin/sh\necho '{nope'\n");
    assert!(matches!(
        introspect(&garbage, &src, DEFAULT_TIMEOUT).await,
        Err(SchemaError::Json(_))
    ));
}

#[tokio::test]
async fn caches_until_the_tree_changes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = fake_odoo(&dir.path().join("odoo"), "19.0");
    let cache = dir.path().join("cache");
    let source = entry(&src);

    let first = load_schema(&source, Some(&python()), &cache, DEFAULT_TIMEOUT)
        .await
        .expect("load");
    assert_eq!(first.origin, SchemaOrigin::Introspected);
    let file = cache.join("odoo-19.0.json");
    let mode = fs::metadata(&file).expect("cache").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);

    // No Python needed while the tree is unchanged.
    let second = load_schema(&source, None, &cache, DEFAULT_TIMEOUT)
        .await
        .expect("load");
    assert_eq!(second.origin, SchemaOrigin::Cached);
    assert_eq!(second.schema, first.schema);

    // Editing config.py invalidates the cache.
    fs::write(src.join("odoo/tools/config.py"), "# changed, longer\n").expect("write");
    let third = load_schema(&source, Some(&python()), &cache, DEFAULT_TIMEOUT)
        .await
        .expect("load");
    assert_eq!(third.origin, SchemaOrigin::Introspected);

    // A corrupt cache is ignored, not an error.
    fs::write(&file, "{garbage").expect("write");
    let fourth = load_schema(&source, Some(&python()), &cache, DEFAULT_TIMEOUT)
        .await
        .expect("load");
    assert_eq!(fourth.origin, SchemaOrigin::Introspected);
}

#[tokio::test]
async fn falls_back_to_bundled() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = fake_odoo(&dir.path().join("odoo"), "19.0");
    let cache = dir.path().join("cache");
    let source = entry(&src);

    let no_venv = load_schema(&source, None, &cache, DEFAULT_TIMEOUT)
        .await
        .expect("load");
    assert!(matches!(no_venv.origin, SchemaOrigin::Bundled { .. }));
    assert_eq!(no_venv.schema, bundled(v("19.0")));

    let broken = script(dir.path(), "broken-python", "#!/bin/sh\nexit 3\n");
    let failed = load_schema(&source, Some(&broken), &cache, DEFAULT_TIMEOUT)
        .await
        .expect("load");
    assert!(matches!(failed.origin, SchemaOrigin::Bundled { .. }));
    assert!(!cache.exists(), "failures are not cached");

    // Tree claims a different series than the registry.
    let other = fake_odoo(&dir.path().join("odoo20"), "20.0");
    let mismatch = load_schema(&entry(&other), Some(&python()), &cache, DEFAULT_TIMEOUT)
        .await
        .expect("load");
    match mismatch.origin {
        SchemaOrigin::Bundled { reason } => assert!(reason.contains("20.0"), "{reason}"),
        o => panic!("expected Bundled, got {o:?}"),
    }

    let mut ent = entry(&src);
    ent.edition = Edition::Enterprise;
    assert!(matches!(
        load_schema(&ent, None, &cache, DEFAULT_TIMEOUT).await,
        Err(SchemaError::NotCommunity(_))
    ));
}

#[test]
fn cache_path_rejects_unsafe_ids() {
    let dir = Path::new("/c");
    assert_eq!(
        cache_path(dir, "odoo-19.0"),
        Some(PathBuf::from("/c/odoo-19.0.json"))
    );
    for bad in ["", ".hidden", "../x", "a/b", "a b"] {
        assert_eq!(cache_path(dir, bad), None, "{bad:?}");
    }
}
