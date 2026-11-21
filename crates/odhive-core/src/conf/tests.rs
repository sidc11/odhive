use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::*;
use crate::config::Edition;
use crate::schema::bundled;

const SAVED_19: &str = include_str!("fixtures/odoo-19-saved.conf");
const SAVED_20: &str = include_str!("fixtures/odoo-20-saved.conf");
const LEGACY: &str = include_str!("fixtures/dev-legacy.conf");

fn schema(v: &str) -> OptionSchema {
    bundled(v.parse().expect("version"))
}

fn source(id: &str, edition: Edition, path: &str) -> SourceEntry {
    SourceEntry {
        id: id.into(),
        edition,
        odoo_version: "19.0".parse().expect("version"),
        path: path.into(),
    }
}

fn community() -> SourceEntry {
    source("odoo-19.0", Edition::Community, "/opt/odoo")
}

fn enterprise() -> SourceEntry {
    source("enterprise-19.0", Edition::Enterprise, "/opt/enterprise")
}

fn opts(pairs: &[(&str, &str)]) -> OdooOptions {
    OdooOptions(
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
    )
}

/// Our parser and Python's `configparser.RawConfigParser` agree.
#[test]
fn parser_matches_python_configparser() {
    let cases = [
        SAVED_19,
        SAVED_20,
        LEGACY,
        "[options]\na = 1\n\n  more\n\nb = 2\n",
        "[options]\na = 1\n# c\n  b = 2\n",
        "[options]\n  a = 1\n  b = 2\n    c\n",
        "[DEFAULT]\nx = 1\n[options]\nKey: V a l\n[other]\ny = %(x)s\\n\n",
        "[options]\nempty =\nquoted = \"q\" ; not a comment\n",
    ];
    let script = r#"
import configparser, json, sys
p = configparser.RawConfigParser()
p.read_string(sys.stdin.read())
json.dump([[s, list(p.items(s))] for s in p.sections()], sys.stdout)
"#;
    for text in cases {
        let mut child = std::process::Command::new("python3")
            .args(["-I", "-c", script])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("python3");
        std::io::Write::write_all(child.stdin.as_mut().expect("stdin"), text.as_bytes())
            .expect("write");
        let out = child.wait_with_output().expect("wait");
        assert!(out.status.success(), "python failed on {text:?}");
        let python: Vec<(String, Vec<(String, String)>)> =
            serde_json::from_slice(&out.stdout).expect("json");
        assert_eq!(ini::parse(text).expect("parse"), python, "{text:?}");
    }
}

#[test]
fn imports_odoo_saved_files() {
    for (text, v) in [(SAVED_19, "19.0"), (SAVED_20, "20.0")] {
        let s = schema(v);
        let imp = import_conf(text, &s, &[]).expect("import");
        assert_eq!(imp.instance.http_port, Some(8069));
        assert_eq!(imp.instance.gevent_port, Some(8072));
        assert_eq!(imp.instance.workers, Some(0));
        assert_eq!(imp.instance.db, None);
        assert!(imp.addons_paths.is_empty());
        // `--save` lists every option; only values that differ from the
        // declared defaults (computed at runtime by Odoo) survive.
        assert!(imp.options.0.len() < 15, "{:?}", imp.options.0.keys());
        assert!(
            imp.options.0.values().all(|v| v != "None"),
            "{:?}",
            imp.options
        );
        for k in imp.options.0.keys() {
            assert!(!is_managed_key(k), "{k}");
            assert!(s.get(k).is_some(), "{k} unknown");
        }
        assert!(imp.options.get("admin_passwd").is_none(), "default dropped");
        // Odoo 20 added a [colors] section that Odoo itself reads; keep it.
        if v == "20.0" {
            assert_eq!(imp.sections["colors"].get("sql"), Some("auto"));
        } else {
            assert!(imp.sections.is_empty());
        }
    }
}

#[test]
fn imports_a_legacy_dev_config() {
    let s = schema("19.0");
    let addons = ConfContext::source_addons(&community(), Some(&enterprise()));
    let imp = import_conf(LEGACY, &s, &addons).expect("import");

    assert_eq!(
        imp.addons_paths,
        vec![PathBuf::from("/home/dev/acme/addons")]
    );
    assert_eq!(
        imp.instance,
        InstanceHints {
            http_port: Some(8070),
            gevent_port: Some(8073),
            db: Some("acme".into()),
            workers: Some(2),
        }
    );
    let o = &imp.options;
    assert_eq!(o.get("admin_passwd"), Some("sup3r-s3cret"));
    assert_eq!(o.get("db_password"), Some("odoo-db-pass"));
    assert_eq!(o.get("dbfilter"), Some("^acme$"));
    assert_eq!(o.get("log_level"), Some("debug"));
    assert_eq!(o.get("list_db"), Some("False"));
    assert_eq!(o.get("proxy_mode"), Some("yes"));
    assert_eq!(o.get("server_wide_modules"), Some("base,web,queue_job"));
    // `without_demo = all` → no demo data, which is the default → dropped.
    assert_eq!(o.get("with_demo"), None);
    // "False" on a non-boolean option is ignored, like Odoo does.
    assert_eq!(o.get("smtp_server"), None);
    for k in [
        "logfile",
        "data_dir",
        "config",
        "http_port",
        "db_name",
        "workers",
    ] {
        assert_eq!(o.get(k), None, "{k}");
    }
    assert_eq!(imp.sections["queue_job"].get("channels"), Some("root:2"));

    let w = imp.warnings.join("\n");
    assert!(w.contains("logfile"), "{w}");
    assert!(w.contains("data_dir"), "{w}");
    assert!(w.contains("config: command-line only"), "{w}");
    assert!(w.contains("without_demo"), "{w}");
    // Warnings never leak secrets.
    assert!(
        !w.contains("sup3r-s3cret") && !w.contains("odoo-db-pass"),
        "{w}"
    );
}

#[test]
fn import_export_import_is_equivalent() {
    let s = schema("19.0");
    let (c, e) = (community(), enterprise());
    let addons = ConfContext::source_addons(&c, Some(&e));
    let first = import_conf(LEGACY, &s, &addons).expect("import");

    let mut project = ProjectConfig::new("acme", "odoo-19.0");
    project.addons_paths = first.addons_paths.clone();
    project.odoo_options = first.options.clone();
    project.odoo_sections = first.sections.clone();
    project.validate().expect("imported model is valid");
    let ctx = ConfContext::for_project(&project, Path::new("/w/projects/acme"), &c, Some(&e));
    let text = render(
        &ctx,
        &project.odoo_options,
        &project.odoo_sections,
        Some(&s),
        false,
    )
    .expect("render");

    let second = import_conf(&text, &s, &addons).expect("re-import");
    assert_eq!(second.options, first.options);
    assert_eq!(second.sections, first.sections);
    assert_eq!(second.addons_paths, first.addons_paths);
    assert!(
        second.warnings.iter().all(|w| !w.contains("skipped")),
        "{:?}",
        second.warnings
    );
    // Rendering is deterministic.
    assert_eq!(
        render(
            &ctx,
            &project.odoo_options,
            &project.odoo_sections,
            Some(&s),
            false
        )
        .expect("render"),
        text
    );
}

#[test]
fn renders_in_stable_order_and_masks_secrets() {
    let s = schema("19.0");
    let ctx = ConfContext {
        addons_paths: vec!["/opt/odoo/addons".into(), "/w/p/addons".into()],
        data_dir: "/w/p/data".into(),
    };
    let options = opts(&[
        ("zz_module_setting", "1"),
        ("log_level", "debug"),
        ("admin_passwd", "pw"),
        ("dbfilter", "^%d$"),
        ("aa_unknown", "x"),
    ]);
    let mut sections = BTreeMap::new();
    sections.insert("queue_job".to_owned(), opts(&[("channels", "root:2")]));

    let text = render(&ctx, &options, &sections, Some(&s), false).expect("render");
    assert_eq!(
        text,
        format!(
            "{HEADER}[options]\n\
             addons_path = /opt/odoo/addons,/w/p/addons\n\
             data_dir = /w/p/data\n\
             admin_passwd = pw\n\
             dbfilter = ^%d$\n\
             log_level = debug\n\
             aa_unknown = x\n\
             zz_module_setting = 1\n\n\
             [queue_job]\n\
             channels = root:2\n\n"
        )
    );
    let preview = render(&ctx, &options, &sections, Some(&s), true).expect("render");
    assert!(preview.contains("admin_passwd = ********"));
    assert!(!preview.contains("= pw"));
    assert!(format!("{options:?}").contains("\"admin_passwd\": \"********\""));
}

#[test]
fn addons_path_order_and_bad_paths() {
    let mut p = ProjectConfig::new("acme", "odoo-19.0");
    p.addons_paths = vec!["addons".into(), "/src/oca".into()];
    let ctx = ConfContext::for_project(&p, Path::new("/w/acme"), &community(), Some(&enterprise()));
    assert_eq!(
        ctx.addons_paths,
        vec![
            PathBuf::from("/opt/odoo/addons"),
            PathBuf::from("/opt/enterprise"),
            PathBuf::from("/w/acme/addons"),
            PathBuf::from("/src/oca"),
        ]
    );
    assert_eq!(ctx.data_dir, PathBuf::from("/w/acme/data"));

    let mut bad = ctx;
    bad.addons_paths.push("/a,b".into());
    assert!(matches!(
        render(&bad, &OdooOptions::default(), &BTreeMap::new(), None, false),
        Err(ConfError::BadPath(_))
    ));
}

#[test]
fn set_option_enforces_schema() {
    let s = schema("19.0");
    let mut o = OdooOptions::default();
    set_option(&mut o, &s, "workers_x_module", "anything").expect("unknown keys allowed");
    set_option(&mut o, &s, "limit_time_cpu", "600").expect("int");
    set_option(&mut o, &s, "proxy_mode", "True").expect("bool");
    set_option(&mut o, &s, "log_level", "debug").expect("choice");
    set_option(&mut o, &s, "db_port", "None").expect("None is always allowed");

    let err = |key: &str, value: &str| {
        let mut o = OdooOptions::default();
        set_option(&mut o, &s, key, value).expect_err(key)
    };
    assert!(matches!(
        err("limit_time_cpu", "fast"),
        ConfError::BadValue { .. }
    ));
    assert!(matches!(
        err("transient_age_limit", "x"),
        ConfError::BadValue { .. }
    ));
    assert!(matches!(
        err("proxy_mode", "maybe"),
        ConfError::BadValue { .. }
    ));
    assert!(matches!(
        err("log_level", "loud"),
        ConfError::BadValue { .. }
    ));
    assert!(matches!(err("http_port", "8069"), ConfError::Managed(_)));
    assert!(matches!(err("addons_path", "/x"), ConfError::Managed(_)));
    assert!(matches!(err("init", "base"), ConfError::NotFileLoadable(_)));
    assert!(matches!(err("Admin_Passwd", "x"), ConfError::BadKey(_)));
    assert!(matches!(
        err("dbfilter", "a\nb"),
        ConfError::BadValue { .. }
    ));
    assert!(matches!(err("dbfilter", " a"), ConfError::BadValue { .. }));
}

#[test]
fn import_rejects_broken_files() {
    let s = schema("19.0");
    assert!(matches!(
        import_conf("[other]\na = 1\n", &s, &[]),
        Err(ConfError::NoOptionsSection)
    ));
    assert!(matches!(
        import_conf("[options]\na = 1\na = 2\n", &s, &[]),
        Err(ConfError::Syntax(_))
    ));
    let imp = import_conf(
        "[options]\nlimit_time_cpu = fast\nhttp_port = x\n[bad section!]\nk = v\n",
        &s,
        &[],
    )
    .expect("lenient import");
    assert!(imp.options.is_empty());
    assert_eq!(imp.instance, InstanceHints::default());
    assert_eq!(imp.warnings.len(), 3, "{:?}", imp.warnings);
}

#[test]
fn write_conf_is_private() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut p = ProjectConfig::new("acme", "odoo-19.0");
    p.odoo_options = opts(&[("admin_passwd", "pw")]);
    let ctx = ConfContext::for_project(&p, dir.path(), &community(), None);
    let path = write_conf(&p, dir.path(), &ctx, None).expect("write");
    assert_eq!(path, dir.path().join(CONF_FILE));
    let mode = std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let text = std::fs::read_to_string(&path).expect("read");
    assert!(
        text.contains("admin_passwd = pw"),
        "real file keeps the secret"
    );
}
