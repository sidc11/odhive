use std::path::PathBuf;

use super::*;
use crate::logs::parse::parse_all;
use crate::requirements::Entry as ReqEntry;

fn plan_of(lines: &[(&str, Origin)]) -> InstallPlan {
    InstallPlan {
        requirements: lines
            .iter()
            .map(|(text, origin)| ReqEntry {
                requirement: spec::parse_requirement(text).expect("valid"),
                origin: origin.clone(),
            })
            .collect(),
        ..InstallPlan::default()
    }
}

fn odoo_req(line: usize) -> Origin {
    Origin::File {
        path: PathBuf::from("/src/odoo/requirements.txt"),
        line,
    }
}

#[test]
fn table_is_sorted_unique_and_valid() {
    for pair in IMPORT_TABLE.windows(2) {
        assert!(pair[0].0 < pair[1].0, "not sorted: {pair:?}");
    }
    for (module, package) in IMPORT_TABLE {
        assert!(is_identifier(module), "{module}");
        let req = spec::parse_requirement(package).expect(package);
        assert_eq!(req.as_str(), *package, "a bare name");
        assert!(!ODOO_NAMES.contains(module));
    }
    assert_eq!(table_package("PIL"), Some("Pillow"));
    assert_eq!(table_package("ldap"), Some("python-ldap"));
    assert_eq!(table_package("pil"), None, "case matters for imports");
}

#[test]
fn exception_lines_name_the_top_level_module() {
    for (line, want) in [
        ("ModuleNotFoundError: No module named 'ldap'", Some("ldap")),
        (
            "ModuleNotFoundError: No module named 'PIL.Image'",
            Some("PIL"),
        ),
        (
            "  ModuleNotFoundError: No module named 'a_b2' ",
            Some("a_b2"),
        ),
        ("ImportError: No module named yaml", Some("yaml")),
        ("ImportError: No module named 'yaml'", Some("yaml")),
        ("ImportError: cannot import name 'x' from 'y'", None),
        ("ModuleNotFoundError: No module named ''", None),
        ("ModuleNotFoundError: No module named 'a b'", None),
        ("ModuleNotFoundError: No module named 'x; rm -rf'", None),
        ("ModuleNotFoundError: No module named '-e'", None),
        ("ModuleNotFoundError: No module named 'énorme'", None),
        ("ModuleNotFoundError: No module named 'unterminated", None),
        ("ValueError: No module named 'x'", None),
        ("KeyError: 'ModuleNotFoundError: No module named x'", None),
    ] {
        assert_eq!(missing_in_exception(line), want, "{line}");
    }
    let long = format!("ModuleNotFoundError: No module named '{}'", "a".repeat(200));
    assert_eq!(missing_in_exception(&long), None);
}

#[test]
fn detection_uses_error_entries_only() {
    let tb = "Traceback (most recent call last):\n  File \"/x.py\", line 1, in <module>\n    import ldap\nModuleNotFoundError: No module named 'ldap'";
    let text = format!(
        "2026-11-22 06:27:41,016 6340 CRITICAL db odoo.service.server: Failed to initialize database `db`.\n{tb}\n\
         2026-11-22 06:27:41,017 6340 WARNING db odoo.addons.x: optional\n{tb}\n\
         2026-11-22 06:27:41,018 6340 ERROR db odoo.http: request\n{tb}\n"
    );
    let entries = parse_all(&text);
    let found: Vec<_> = entries.iter().map(missing_module).collect();
    assert_eq!(found, [Some("ldap".into()), None, Some("ldap".into())]);

    // Output before Odoo's logging is set up has no header.
    let raw = parse_all(&format!("{tb}\n"));
    assert_eq!(missing_module(&raw[0]).as_deref(), Some("ldap"));
}

#[test]
fn detection_in_real_odoo_logs() {
    for name in ["odoo-19-import.log", "odoo-20-import.log"] {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src/logs/parse/fixtures")
            .join(name);
        let text = std::fs::read_to_string(&path).expect("fixture");
        let found: Vec<String> = parse_all(&text).iter().filter_map(missing_module).collect();
        assert_eq!(found, ["ldap_missing_pkg"], "{name}");
    }
    for name in ["odoo-19-serve.log", "odoo-20-view.log", "odoo-19-nodb.log"] {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src/logs/parse/fixtures")
            .join(name);
        let text = std::fs::read_to_string(&path).expect("fixture");
        assert!(parse_all(&text).iter().all(|e| missing_module(e).is_none()));
    }
}

#[test]
fn normalize_follows_pep_503() {
    assert_eq!(normalize("Python_LDAP"), "python-ldap");
    assert_eq!(normalize("pdfminer.six"), "pdfminer-six");
    assert_eq!(normalize("a--_.b"), "a-b");
    assert_eq!(normalize("Pillow"), "pillow");
}

#[test]
fn listed_packages_use_the_project_lines() {
    let trusted = Trusted::from_plan(&plan_of(&[
        ("python-ldap==3.4.0 ; python_version < '3.13'", odoo_req(30)),
        (
            "python-ldap==3.4.4 ; python_version >= '3.13'",
            odoo_req(31),
        ),
        ("Pillow==10.2.0", odoo_req(20)),
        (
            "ldap_missing_pkg>=1",
            Origin::Manifest {
                path: PathBuf::from("/addons/x/__manifest__.py"),
                module: "x".into(),
            },
        ),
    ]));
    assert_eq!(trusted.len(), 3);

    let c = resolve("ldap", &trusted).expect("ldap");
    assert_eq!(c.package, "python-ldap");
    assert_eq!(
        c.requirements
            .iter()
            .map(Requirement::as_str)
            .collect::<Vec<_>>(),
        [
            "python-ldap==3.4.0 ; python_version < '3.13'",
            "python-ldap==3.4.4 ; python_version >= '3.13'"
        ]
    );
    assert_eq!(
        c.trust,
        Some(Trust::Listed {
            origin: odoo_req(30)
        })
    );

    // Matched by normalised name; the import name itself is the package.
    let c = resolve("ldap_missing_pkg", &trusted).expect("manifest");
    assert!(matches!(
        c.trust,
        Some(Trust::Listed {
            origin: Origin::Manifest { .. }
        })
    ));
    assert_eq!(c.requirements[0].as_str(), "ldap_missing_pkg>=1");

    let c = resolve("PIL", &trusted).expect("PIL");
    assert_eq!(c.requirements[0].as_str(), "Pillow==10.2.0");
}

#[test]
fn table_packages_are_trusted_without_a_listing() {
    let c = resolve("yaml", &Trusted::builtin()).expect("yaml");
    assert_eq!(c.package, "PyYAML");
    assert_eq!(c.trust, Some(Trust::Table));
    assert_eq!(c.requirements[0].as_str(), "PyYAML");
    assert_eq!(decide(AutofixMode::Auto, &c), Action::Install);
}

#[test]
fn unknown_packages_are_never_installed_without_confirmation() {
    let c = resolve("totally_unknown_mod", &Trusted::builtin()).expect("resolves");
    assert_eq!(c.package, "totally_unknown_mod");
    assert!(!c.trusted());
    assert_eq!(decide(AutofixMode::Auto, &c), Action::Confirm);
    assert_eq!(decide(AutofixMode::Ask, &c), Action::Confirm);
    assert_eq!(decide(AutofixMode::Off, &c), Action::Report);

    let trusted = resolve("PIL", &Trusted::builtin()).expect("PIL");
    assert_eq!(decide(AutofixMode::Ask, &trusted), Action::Confirm);
    assert_eq!(decide(AutofixMode::Off, &trusted), Action::Report);
}

#[test]
fn odoo_names_and_bad_names_are_refused() {
    let t = Trusted::builtin();
    assert_eq!(resolve("odoo", &t), Err(Refusal::PartOfOdoo));
    assert_eq!(resolve("openerp", &t), Err(Refusal::PartOfOdoo));
    assert_eq!(resolve("a-b", &t), Err(Refusal::InvalidName));
    assert_eq!(resolve("", &t), Err(Refusal::InvalidName));
    assert_eq!(resolve("x y", &t), Err(Refusal::InvalidName));
    // A listing cannot make an Odoo name installable.
    let listed = Trusted::from_plan(&plan_of(&[("odoo", odoo_req(1))]));
    assert_eq!(resolve("odoo", &listed), Err(Refusal::PartOfOdoo));
}

#[test]
fn retry_limit_stops_after_three_attempts() {
    let mut a = Attempts::new();
    assert_eq!(a.begin("one"), Ok(1));
    assert_eq!(a.begin("two"), Ok(2));
    assert_eq!(a.begin("three"), Ok(3));
    assert_eq!(a.check("four"), Err(GiveUp::Limit { max: MAX_ATTEMPTS }));
    assert_eq!(a.begin("four"), Err(GiveUp::Limit { max: MAX_ATTEMPTS }));
    assert_eq!(a.used(), 3);
}

#[test]
fn a_package_is_installed_once_per_start() {
    let mut a = Attempts::new();
    assert_eq!(a.begin("python-ldap"), Ok(1));
    assert_eq!(
        a.begin("Python_LDAP"),
        Err(GiveUp::StillMissing {
            package: "Python_LDAP".into()
        })
    );
    assert_eq!(a.used(), 1, "a refused attempt is not counted");
}

#[test]
fn serializes_for_the_api() {
    let c = resolve("ldap", &Trusted::builtin()).expect("ldap");
    let v = serde_json::to_value(&c).expect("json");
    assert_eq!(
        v,
        serde_json::json!({
            "module": "ldap",
            "package": "python-ldap",
            "requirements": ["python-ldap"],
            "trust": {"source": "table"}
        })
    );
    let g = serde_json::to_value(GiveUp::Refused {
        refusal: Refusal::PartOfOdoo,
    })
    .expect("json");
    assert_eq!(
        g,
        serde_json::json!({"reason": "refused", "refusal": "part_of_odoo"})
    );
}
