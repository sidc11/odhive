use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};

use super::manifest::{Value, manifest_from_value, parse_literal, read_manifest};
use super::spec::{logical_lines, parse_requirement};
use super::*;
use crate::python::find_executable;

fn python3() -> PathBuf {
    let path_var = std::env::var_os("PATH").expect("PATH");
    let p = find_executable(&path_var, "python3").expect("python3 on PATH");
    fs::canonicalize(p).expect("canonicalize")
}

fn script(path: &Path, body: &str) -> PathBuf {
    fs::write(path, format!("#!/bin/sh\n{body}")).expect("write");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod");
    path.to_path_buf()
}

fn write(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("mkdir");
    }
    fs::write(path, text).expect("write");
}

fn events() -> (Arc<Mutex<Vec<InstallEvent>>>, impl FnMut(InstallEvent)) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&log);
    (log, move |e| sink.lock().expect("lock").push(e))
}

// ---- requirement specifiers -------------------------------------------------

#[test]
fn accepts_odoo_style_requirement_lines() {
    let cases = [
        (
            "Babel==2.9.1 ; python_version < '3.11'",
            "Babel==2.9.1 ; python_version < '3.11'",
        ),
        (
            "cryptography==3.4.8; python_version < '3.12'",
            "cryptography==3.4.8 ; python_version < '3.12'",
        ),
        (
            "gevent==24.2.1 ; sys_platform != 'win32' and python_version >= '3.12' and python_version < '3.13'",
            "gevent==24.2.1 ; sys_platform != 'win32' and python_version >= '3.12' and python_version < '3.13'",
        ),
        (
            "cbor2==5.4.2.post1 ; python_version < '3.11'",
            "cbor2==5.4.2.post1 ; python_version < '3.11'",
        ),
        (
            "urllib3==2.0.7  ; python_version >= '3.12'",
            "urllib3==2.0.7 ; python_version >= '3.12'",
        ),
        (
            "requests==2.25.1 ;  python_version < '3.11'",
            "requests==2.25.1 ; python_version < '3.11'",
        ),
        (
            "lxml-html-clean; python_version >= '3.12'",
            "lxml-html-clean ; python_version >= '3.12'",
        ),
        ("pytz", "pytz"),
        ("vobject==0.9.6.1", "vobject==0.9.6.1"),
        ("pdfminer.six>=20221105", "pdfminer.six>=20221105"),
        ("requests[socks] >= 2.0, < 3", "requests[socks]>=2.0,<3"),
        ("foo (>=1.0,<2)", "foo>=1.0,<2"),
        ("zope.interface~=6.0", "zope.interface~=6.0"),
        ("Django==4.2.*", "Django==4.2.*"),
        (
            "foo ; platform_machine == 'x86-64'",
            "foo ; platform_machine == 'x86-64'",
        ),
    ];
    for (input, want) in cases {
        let r = parse_requirement(input).unwrap_or_else(|e| panic!("{input}: {e}"));
        assert_eq!(r.as_str(), want, "{input}");
    }
    assert_eq!(
        parse_requirement("python-ldap==3.4.4").expect("ok").name(),
        "python-ldap"
    );
    assert_eq!(
        parse_requirement("requests[socks]>=2").expect("ok").name(),
        "requests"
    );
}

#[test]
fn rejects_options_urls_and_paths() {
    let cases = [
        "-r other.txt",
        "--requirement=other.txt",
        "-e git+https://github.com/x/y.git#egg=y",
        "--index-url https://evil.example/simple",
        "--extra-index-url=https://evil.example/simple",
        "-i https://evil.example/simple",
        "--find-links /tmp/wheels",
        "-c constraints.txt",
        "--trusted-host evil.example",
        "foo @ https://evil.example/foo.whl",
        "foo@git+https://evil.example/foo",
        "https://evil.example/foo-1.0.tar.gz",
        "git+ssh://git@example.com/foo.git",
        "./vendor/foo",
        "../foo",
        "/abs/path/foo.whl",
        "~/foo",
        "foo==1.0 --hash=sha256:abcd",
        "foo ; python_version > '3' --hash=sha256:abcd",
        "foo ; python_version > '3' -i https://evil.example",
        "foo==1.0 -i https://evil.example",
        "foo\nbar",
        "foo\0",
        "foo$(id)",
        "foo; rm -rf /",
        "_foo",
        "foo-",
        "foo bar",
        "foo ==",
        "foo [x",
        "foo[x y]",
        "foo ;",
        "",
        "   ",
    ];
    for input in cases {
        assert!(parse_requirement(input).is_err(), "accepted {input:?}");
    }
    assert!(parse_requirement(&"a".repeat(spec::MAX_SPEC_LEN + 1)).is_err());
}

#[test]
fn logical_lines_strip_comments_and_join_continuations() {
    let text = "# header\n\
                \n\
                Babel==2.9.1 ; python_version < '3.11'  # min version\n\
                pytz  # no pinning\n\
                foo==1.0 \\\n    ; python_version > '3'\n\
                \t\n\
                pkg#notacomment\n\
                last";
    let lines = logical_lines(text);
    assert_eq!(
        lines,
        vec![
            (3, "Babel==2.9.1 ; python_version < '3.11'".to_owned()),
            (4, "pytz".to_owned()),
            (5, "foo==1.0      ; python_version > '3'".to_owned()),
            (8, "pkg#notacomment".to_owned()),
            (9, "last".to_owned()),
        ]
    );
    assert_eq!(
        parse_requirement(&lines[2].1).expect("joined").as_str(),
        "foo==1.0 ; python_version > '3'"
    );
}

// ---- manifest literal parser ------------------------------------------------

const REAL_MANIFEST: &str = r#"# -*- coding: utf-8 -*-
# Part of Odoo. See LICENSE file for full copyright and licensing details.
{
    'name': 'Phone Numbers Validation',
    'version': '2.1',
    'summary': 'Centralize phone number validation',
    'description': """
Phone Numbers Validation
========================

This module adds the feature of validation and formatting phone numbers
according to a destination country.
        """,
    'category': 'Hidden',
    "sequence": -1,
    'depends': [
        'base',
        'mail',
    ],
    'data': [
        'security/ir.model.access.csv',  # trailing comment
    ],
    'external_dependencies': {
        'python': ['phonenumbers'],
        'apt': {
            'phonenumbers': 'python3-phonenumbers',
        },
    },
    'assets': {
        'web.assets_backend': [
            ('include', 'web._assets_helpers'),
            'phone_validation/static/src/**/*',
        ],
    },
    'installable': True,
    'auto_install': False,
    'author': 'Odoo S.A.', 'license': 'LGPL-3',
}
"#;

#[test]
fn parses_a_real_manifest() {
    let v = parse_literal(REAL_MANIFEST).expect("parse");
    assert_eq!(
        v.get("name"),
        Some(&Value::Str("Phone Numbers Validation".into()))
    );
    assert_eq!(v.get("sequence"), Some(&Value::Int(-1)));
    assert!(matches!(v.get("description"), Some(Value::Str(s)) if s.contains("=====\n")));
    let m = manifest_from_value(&v).expect("shape");
    assert!(m.installable);
    assert_eq!(m.python_dependencies, ["phonenumbers"]);
}

#[test]
fn parses_literal_corner_cases() {
    let v = parse_literal(
        r#"{
        'a': 'x' "y"
             '''z''',
        'b': (1,),
        'c': (2),
        'd': (),
        'e': {1, 2,},
        'f': [1.5, -2e3, .5, 1_000, 0x1F, 0o17, 0b101, 00],
        'g': None, 'h': False,
        'i': r'\d+\'',
        'j': 'tab\there \x41é\101 \q',
        'k': b'bytes',
        'l': U'unicode',
        'm': 'line \
continued',
        ('t', 1): 'tuple key',
        'a': 'last wins',
    }"#,
    )
    .expect("parse");
    assert_eq!(v.get("a"), Some(&Value::Str("last wins".into())));
    assert_eq!(v.get("b"), Some(&Value::Tuple(vec![Value::Int(1)])));
    assert_eq!(v.get("c"), Some(&Value::Int(2)));
    assert_eq!(v.get("d"), Some(&Value::Tuple(vec![])));
    assert_eq!(
        v.get("e"),
        Some(&Value::List(vec![Value::Int(1), Value::Int(2)]))
    );
    assert_eq!(
        v.get("f"),
        Some(&Value::List(vec![
            Value::Float(1.5),
            Value::Float(-2000.0),
            Value::Float(0.5),
            Value::Int(1000),
            Value::Int(31),
            Value::Int(15),
            Value::Int(5),
            Value::Int(0),
        ]))
    );
    assert_eq!(v.get("i"), Some(&Value::Str(r"\d+\'".into())));
    assert_eq!(
        v.get("j"),
        Some(&Value::Str("tab\there Aé\u{41} \\q".into()))
    );
    assert_eq!(v.get("k"), Some(&Value::Str("bytes".into())));
    assert_eq!(v.get("m"), Some(&Value::Str("line continued".into())));

    let first = parse_literal("{'a': 'x' 'y' '''z'''}").expect("parse");
    assert_eq!(first.get("a"), Some(&Value::Str("xyz".into())));
}

#[test]
fn rejects_code_in_manifests() {
    let cases = [
        "{'description': open('README.rst').read()}",
        "{'x': __import__('os').system('id')}",
        "__import__('os').system('id')",
        "{'x': 1 + 2}",
        "{'x': 'a' + 'b'}",
        "{'x': f'{1}'}",
        "{'x': rf'{1}'}",
        "{'x': lambda: 0}",
        "{'x': [i for i in range(3)]}",
        "{'x': os.environ}",
        "{'x': (yield)}",
        "{'x': -'a'}",
        "{'x': -True}",
        "{'x': 1j}",
        "{'x': 010}",
        "{'x': 1__0}",
        "{'x': b'é'}",
        "{'x': 'a' b'b'}",
        "{'x': 'unterminated}",
        "{'x': '\\N{BULLET}'}",
        "{[1]: 2}",
        "{([1],): 2}",
        "{[1], 2}",
        "{'a': 1} {'b': 2}",
        "{'a': 1}; import os",
        "{'a' 1}",
        "{'a': 1,, }",
        "[1 2]",
        "",
        "# only a comment",
    ];
    for input in cases {
        assert!(parse_literal(input).is_err(), "accepted {input:?}");
    }
    let err = parse_literal("{\n  'x': open('f'),\n}").expect_err("code");
    assert_eq!((err.line, err.col), (2, 8), "{err:?}");
    assert!(err.message.contains("`open`"), "{err:?}");
}

#[test]
fn deep_nesting_is_refused_not_overflowed() {
    let deep = format!("{}{}", "[".repeat(100_000), "]".repeat(100_000));
    let err = parse_literal(&deep).expect_err("deep");
    assert!(err.message.contains("deeply"), "{err:?}");
    let ok = format!("{}{}", "[".repeat(60), "]".repeat(60));
    parse_literal(&ok).expect("60 levels");
}

#[test]
fn manifest_shape_errors() {
    for (src, want) in [
        ("['not', 'a', 'dict']", "not a dict"),
        ("{'external_dependencies': ['x']}", "not a dict"),
        ("{'external_dependencies': {'python': 'x'}}", "not a list"),
        ("{'external_dependencies': {'python': [1]}}", "non-string"),
    ] {
        let v = parse_literal(src).expect("literal");
        let err = manifest_from_value(&v).expect_err(src);
        assert!(err.contains(want), "{src}: {err}");
    }
    let v = parse_literal("{'installable': False, 'external_dependencies': {'python': ['x']}}")
        .expect("literal");
    let m = manifest_from_value(&v).expect("shape");
    assert!(!m.installable);
    let v = parse_literal("{'external_dependencies': {'bin': ['wkhtmltopdf']}}").expect("literal");
    assert!(
        manifest_from_value(&v)
            .expect("shape")
            .python_dependencies
            .is_empty()
    );
}

#[test]
fn read_manifest_limits() {
    let dir = tempfile::tempdir().expect("tempdir");
    let big = dir.path().join("big.py");
    write(
        &big,
        &format!(
            "{{'a': '{}'}}",
            "x".repeat(manifest::MAX_MANIFEST_BYTES as usize)
        ),
    );
    assert!(matches!(
        read_manifest(&big),
        Err(ManifestError::TooLarge(_))
    ));
    let bad = dir.path().join("bad.py");
    fs::write(&bad, b"{'a': '\xff'}").expect("write");
    assert!(matches!(
        read_manifest(&bad),
        Err(ManifestError::Syntax { .. })
    ));
    let bom = dir.path().join("bom.py");
    write(&bom, "\u{feff}{'name': 'x'}");
    read_manifest(&bom).expect("BOM allowed");
}

/// Converts a parsed value to the JSON Python's `json.dumps` would produce
/// for the same literal (tuples and sets as lists, keys as strings).
fn to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::Str(s) => serde_json::Value::String(s.clone()),
        Value::Int(i) => serde_json::Value::from(*i),
        Value::Float(f) => serde_json::Value::from(*f),
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::None => serde_json::Value::Null,
        Value::List(items) | Value::Tuple(items) => {
            serde_json::Value::Array(items.iter().map(to_json).collect())
        }
        Value::Dict(items) => {
            let mut map = serde_json::Map::new();
            for (k, val) in items {
                let key = match k {
                    Value::Str(s) => s.clone(),
                    other => to_json(other).to_string(),
                };
                map.insert(key, to_json(val));
            }
            serde_json::Value::Object(map)
        }
    }
}

/// `ast.literal_eval` + `json.dumps` in a real Python, for differential tests.
fn python_json(path: &Path) -> Option<serde_json::Value> {
    const SCRIPT: &str = r"
import ast, json, sys
def norm(v):
    if isinstance(v, (tuple, list, set, frozenset)):
        return [norm(x) for x in v]
    if isinstance(v, dict):
        return {(k if isinstance(k, str) else json.dumps(norm(k), separators=(',', ':'))): norm(x) for k, x in v.items()}
    if isinstance(v, bytes):
        return v.decode('latin-1')
    return v
src = open(sys.argv[1], encoding='utf-8-sig').read()
print(json.dumps(norm(ast.literal_eval(src))))
";
    let out = std::process::Command::new(python3())
        .args(["-I", "-B", "-c", SCRIPT])
        .arg(path)
        .output()
        .expect("run python");
    out.status
        .success()
        .then(|| serde_json::from_slice(&out.stdout).expect("json"))
}

#[test]
fn agrees_with_python_literal_eval() {
    let dir = tempfile::tempdir().expect("tempdir");
    let samples = [
        REAL_MANIFEST,
        r#"{'a': 'x' "y" '''z''', 'b': (1,), 'c': {1, 2}, 'd': [1.5, -2e3, .5, 1_000, 0x1F]}"#,
        r#"{'s': 'tab\there \x41é\101 \q', 'r': r'\d+\'', 'n': None, 't': True}"#,
        "{'m': 'line \\\ncontinued', 'u': u'x', 'e': {}, 'l': [], 'p': ()}",
        "{\r\n  'crlf': '''a\r\nb''',\r\n}\r\n",
    ];
    for (i, src) in samples.iter().enumerate() {
        let path = dir.path().join(format!("m{i}.py"));
        write(&path, src);
        let ours = to_json(&parse_literal(src).unwrap_or_else(|e| panic!("sample {i}: {e:?}")));
        let theirs = python_json(&path).expect("python accepts sample");
        assert_eq!(ours, theirs, "sample {i}");
    }
}

/// Every manifest of a real Odoo checkout parses and matches Python. Runs when
/// `ODHIVE_TEST_ODOO_SOURCE` points at one (e.g. `~/OdHive/sources/odoo-19.0`).
#[test]
fn real_odoo_manifests_agree_with_python() {
    let Some(src) = std::env::var_os("ODHIVE_TEST_ODOO_SOURCE").map(PathBuf::from) else {
        eprintln!("ODHIVE_TEST_ODOO_SOURCE not set; skipping");
        return;
    };
    let mut checked = 0;
    for base in [src.join("addons"), src.join("odoo/addons")] {
        for entry in fs::read_dir(&base).expect("addons dir") {
            let path = entry.expect("entry").path().join(MANIFEST_FILE);
            if !path.is_file() {
                continue;
            }
            let text = fs::read_to_string(&path).expect("read");
            let ours = parse_literal(text.trim_start_matches('\u{feff}'))
                .unwrap_or_else(|e| panic!("{}: {e:?}", path.display()));
            let theirs = python_json(&path).expect("python parses it");
            assert_eq!(to_json(&ours), theirs, "{}", path.display());
            read_manifest(&path).unwrap_or_else(|e| panic!("{e}"));
            checked += 1;
        }
    }
    assert!(checked > 100, "only {checked} manifests found");
    eprintln!("{checked} manifests agree");
}

// ---- collect ----------------------------------------------------------------

/// Odoo source + Enterprise-like path + custom path.
fn tree() -> (tempfile::TempDir, PathBuf, Vec<PathBuf>) {
    let root = tempfile::tempdir().expect("tempdir");
    let odoo = root.path().join("odoo");
    write(
        &odoo.join(REQUIREMENTS_FILE),
        "# Odoo\nBabel==2.17.0 ; python_version >= '3.13'\npytz  # unpinned\nlxml==6.0.2\n",
    );
    // Bundled addons are covered by requirements.txt and must not be scanned.
    write(
        &odoo.join("addons/phone_validation").join(MANIFEST_FILE),
        "{'external_dependencies': {'python': ['phonenumbers']}}",
    );
    let ent = root.path().join("enterprise");
    write(
        &ent.join("account_x").join(MANIFEST_FILE),
        "{'name': 'X', 'external_dependencies': {'python': ['pdfminer.six', 'pytz']}}",
    );
    write(
        &ent.join("old_mod").join(MANIFEST_FILE),
        "{'installable': False, 'external_dependencies': {'python': ['never-installed']}}",
    );
    write(&ent.join("not_a_module/README"), "x");
    let custom = root.path().join("custom");
    write(
        &custom.join(REQUIREMENTS_FILE),
        "requests>=2\n--index-url https://evil.example/simple\n-e git+https://x/y#egg=y\n\
         foo @ https://evil.example/foo.whl\nlxml==6.0.2\n",
    );
    write(
        &custom.join("my_mod").join(MANIFEST_FILE),
        "{'external_dependencies': {'python': ['xlrd', '--extra-index-url=https://evil.example']}}",
    );
    write(
        &custom.join("evil_mod").join(MANIFEST_FILE),
        "{'name': __import__('os').system('touch /tmp/pwned')}",
    );
    let paths = vec![ent, custom, root.path().join("missing")];
    (root, odoo, paths)
}

#[test]
fn collects_from_odoo_addons_requirements_and_manifests() {
    let (_root, odoo, paths) = tree();
    let plan = collect(&odoo, &paths).expect("collect");

    let reqs: Vec<&str> = plan
        .requirements
        .iter()
        .map(|e| e.requirement.as_str())
        .collect();
    assert_eq!(
        reqs,
        [
            "Babel==2.17.0 ; python_version >= '3.13'",
            "pytz",
            "lxml==6.0.2",
            "pdfminer.six",
            "requests>=2",
            "xlrd",
        ]
    );
    assert_eq!(
        plan.requirements[1].origin,
        Origin::File {
            path: odoo.join(REQUIREMENTS_FILE),
            line: 3
        }
    );
    assert!(matches!(
        &plan.requirements[3].origin,
        Origin::Manifest { module, .. } if module == "account_x"
    ));

    let skipped: Vec<&str> = plan.skipped.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(
        skipped,
        [
            "--index-url https://evil.example/simple",
            "-e git+https://x/y#egg=y",
            "foo @ https://evil.example/foo.whl",
            "--extra-index-url=https://evil.example",
        ]
    );
    assert_eq!(plan.files.len(), 2);
    assert_eq!(
        plan.modules, 2,
        "account_x + my_mod (old_mod not installable, evil_mod broken)"
    );
    let warned: Vec<String> = plan
        .warnings
        .iter()
        .map(|w| {
            w.path
                .file_name()
                .expect("name")
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(warned, [MANIFEST_FILE, "missing"]);
    assert!(plan.warnings[0].message.contains("`__import__`"));
}

#[test]
fn missing_odoo_requirements_is_an_error() {
    let root = tempfile::tempdir().expect("tempdir");
    let err = collect(root.path(), &[]).expect_err("missing");
    assert!(
        matches!(err, RequirementsError::NoOdooRequirements(_)),
        "{err}"
    );
}

#[test]
fn oversized_requirements_file_is_refused() {
    let root = tempfile::tempdir().expect("tempdir");
    let big = format!("pytz\n{}", "#".repeat(MAX_REQUIREMENTS_BYTES as usize));
    write(&root.path().join(REQUIREMENTS_FILE), &big);
    let err = collect(root.path(), &[]).expect_err("big");
    assert!(matches!(err, RequirementsError::TooLarge(_)), "{err}");
}

/// Real Odoo checkouts' requirements.txt are accepted line for line.
#[test]
fn real_odoo_requirements_are_fully_accepted() {
    let Some(src) = std::env::var_os("ODHIVE_TEST_ODOO_SOURCE").map(PathBuf::from) else {
        eprintln!("ODHIVE_TEST_ODOO_SOURCE not set; skipping");
        return;
    };
    let plan = collect(&src, &[]).expect("collect");
    assert!(plan.skipped.is_empty(), "{:#?}", plan.skipped);
    assert!(plan.requirements.len() > 30);
}

// ---- install ----------------------------------------------------------------

fn plan_of(reqs: &[&str]) -> InstallPlan {
    let mut plan = InstallPlan::default();
    let mut seen = BTreeSet::new();
    for (i, r) in reqs.iter().enumerate() {
        plan.add(
            &mut seen,
            r,
            Origin::File {
                path: "requirements.txt".into(),
                line: i + 1,
            },
        );
    }
    assert!(plan.skipped.is_empty());
    plan
}

/// A real venv without pip (fast), shared by install tests.
fn bare_venv(dir: &Path) -> PathBuf {
    let venv = dir.join(".venv");
    let st = std::process::Command::new(python3())
        .args(["-I", "-m", "venv", "--without-pip"])
        .arg(&venv)
        .status()
        .expect("venv");
    assert!(st.success());
    venv.join("bin/python")
}

/// A fake uv that records argv, cwd and the `-r` file, then runs `body`.
fn fake_uv(dir: &Path, body: &str) -> PathBuf {
    let rec = dir.join("rec");
    script(
        &dir.join("uv"),
        &format!(
            r#"rec='{rec}'
printf '%s\n' "$@" > "$rec.argv"
pwd > "$rec.cwd"
env > "$rec.env"
prev=
for a in "$@"; do
  if [ "$prev" = "-r" ]; then cat "$a" > "$rec.req"; ls -ld "$(dirname "$a")" > "$rec.perm"; fi
  prev="$a"
done
{body}
"#,
            rec = rec.display()
        ),
    )
}

#[tokio::test]
async fn uv_install_gets_a_fixed_argument_vector_and_validated_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let py = bare_venv(dir.path());
    let uv = fake_uv(
        dir.path(),
        "echo 'Resolved 2 packages'; echo 'Installed 2 packages' >&2",
    );
    let plan = plan_of(&["pytz", "Babel==2.17.0 ; python_version >= '3.13'"]);
    let (log, sink) = events();
    install(
        &plan,
        &InstallOptions::new(py.clone(), VenvTool::Uv(uv)),
        sink,
    )
    .await
    .expect("install");

    let rec = |ext: &str| fs::read_to_string(dir.path().join(format!("rec.{ext}"))).expect(ext);
    let argv: Vec<String> = rec("argv").lines().map(str::to_owned).collect();
    assert_eq!(
        &argv[..5],
        [
            "pip",
            "install",
            "--no-config",
            "--python",
            &py.to_string_lossy()
        ]
    );
    assert_eq!(argv[5], "-r");
    assert_eq!(argv.len(), 7);
    assert!(argv[6].ends_with("/requirements.txt"), "{argv:?}");
    assert_eq!(
        rec("req"),
        "pytz\nBabel==2.17.0 ; python_version >= '3.13'\n"
    );
    assert!(rec("perm").starts_with("drwx------"), "{}", rec("perm"));
    // Ran inside the private dir, which is gone afterwards.
    // Compared by the random dir name: on macOS `pwd` reports /private/var.
    let cwd = PathBuf::from(rec("cwd").trim());
    let work = Path::new(&argv[6]).parent().expect("parent");
    assert_eq!(cwd.file_name(), work.file_name());
    assert!(!work.exists());
    assert!(!rec("env").lines().any(|l| l.starts_with("VIRTUAL_ENV=")));

    let log = log.lock().expect("lock");
    assert!(matches!(
        log.first(),
        Some(InstallEvent::Started { tool, count: 2, .. }) if tool == "uv pip install"
    ));
    assert!(
        log.iter()
            .any(|e| matches!(e, InstallEvent::Output { line } if line == "Installed 2 packages"))
    );
    assert_eq!(log.last(), Some(&InstallEvent::Finished { count: 2 }));
}

#[tokio::test]
async fn pip_install_runs_the_venv_interpreter() {
    let dir = tempfile::tempdir().expect("tempdir");
    let real = bare_venv(dir.path());
    // Wrap the venv interpreter: `-m pip` is recorded, anything else (the
    // probe) runs the real venv Python.
    let rec = dir.path().join("pip-argv");
    let wrapper = script(
        &real.with_file_name("python-wrapper"),
        &format!(
            r#"case " $* " in
  *" -m pip "*) printf '%s\n' "$@" > '{}'; echo "Successfully installed pytz"; exit 0 ;;
esac
exec '{}' "$@"
"#,
            rec.display(),
            real.display()
        ),
    );
    let plan = plan_of(&["pytz"]);
    install(
        &plan,
        &InstallOptions::new(wrapper, VenvTool::Stdlib),
        |_| {},
    )
    .await
    .expect("install");
    let argv: Vec<String> = fs::read_to_string(&rec)
        .expect("argv")
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(
        &argv[..11],
        [
            "-I",
            "-m",
            "pip",
            "install",
            "--require-virtualenv",
            "--no-input",
            "--disable-pip-version-check",
            "--progress-bar",
            "off",
            "-r",
            &argv[10],
        ]
    );
    assert_eq!(argv.len(), 11);
}

#[tokio::test]
async fn refuses_to_install_outside_a_venv() {
    let plan = plan_of(&["pytz"]);
    let err = install(
        &plan,
        &InstallOptions::new(python3(), VenvTool::Stdlib),
        |_| {},
    )
    .await
    .expect_err("system python");
    assert!(matches!(err, RequirementsError::NotAVenv(_)), "{err}");

    let err = install(
        &plan,
        &InstallOptions::new("/nonexistent/python".into(), VenvTool::Stdlib),
        |_| {},
    )
    .await
    .expect_err("missing");
    assert!(matches!(err, RequirementsError::Python { .. }), "{err}");
}

#[tokio::test]
async fn empty_plan_runs_nothing() {
    let (log, sink) = events();
    install(
        &InstallPlan::default(),
        &InstallOptions::new("/nonexistent/python".into(), VenvTool::Stdlib),
        sink,
    )
    .await
    .expect("nothing to do");
    assert_eq!(
        *log.lock().expect("lock"),
        [InstallEvent::Finished { count: 0 }]
    );
}

#[tokio::test]
async fn failure_reports_tail_and_build_hint() {
    let dir = tempfile::tempdir().expect("tempdir");
    let py = bare_venv(dir.path());
    let uv = fake_uv(
        dir.path(),
        "echo 'Building psycopg2==2.9.10'; echo 'Error: pg_config executable not found.' >&2; echo 'build failed' >&2; exit 1",
    );
    let err = install(
        &plan_of(&["psycopg2==2.9.10"]),
        &InstallOptions::new(py, VenvTool::Uv(uv)),
        |_| {},
    )
    .await
    .expect_err("fails");
    let RequirementsError::Failed {
        status,
        output,
        hint,
        ..
    } = &err
    else {
        panic!("{err}");
    };
    assert_eq!(status.code(), Some(1));
    assert!(output.ends_with("build failed"), "{output}");
    assert_eq!(
        hint.as_ref().map(|h| h.library),
        Some(crate::syslib::Library::Libpq),
        "{hint:?}"
    );
    let hint = hint.as_ref().expect("hint");
    let text = err.to_string();
    assert!(text.ends_with(&format!("\nhint: {hint}")), "{text}");
    assert!(text.contains("PostgreSQL client headers"), "{text}");
}

#[tokio::test]
async fn timeout_kills_the_installer_group() {
    let dir = tempfile::tempdir().expect("tempdir");
    let py = bare_venv(dir.path());
    let pidfile = dir.path().join("child.pid");
    let uv = fake_uv(
        dir.path(),
        &format!(
            r#"sh -c 'echo $$ > "{}"; exec sleep 30' & wait"#,
            pidfile.display()
        ),
    );
    let mut opts = InstallOptions::new(py, VenvTool::Uv(uv));
    opts.timeout = Duration::from_millis(1500);
    let err = install(&plan_of(&["pytz"]), &opts, |_| {})
        .await
        .expect_err("timeout");
    assert!(matches!(err, RequirementsError::Timeout(_)), "{err}");
    let pid: i32 = fs::read_to_string(&pidfile)
        .expect("pid")
        .trim()
        .parse()
        .expect("pid");
    let pid = rustix::process::Pid::from_raw(pid).expect("pid");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while rustix::process::test_kill_process(pid).is_ok() {
        assert!(
            std::time::Instant::now() < deadline,
            "build helper still running"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Installs a real Odoo checkout's requirements into a fresh venv with the
/// best interpreter. Slow and needs network: runs only when
/// `ODHIVE_TEST_REAL_INSTALL` names an Odoo source.
#[tokio::test]
async fn real_odoo_install() {
    let Some(src) = std::env::var_os("ODHIVE_TEST_REAL_INSTALL").map(PathBuf::from) else {
        eprintln!("ODHIVE_TEST_REAL_INSTALL not set; skipping");
        return;
    };
    let version = crate::version::detect_version(&src).expect("odoo version");
    let disc = python::discover(&python::DiscoverOptions::from_env()).await;
    let support = python::PySupport::for_source(&src, version);
    let interp = python::pick(&disc.interpreters, support).expect("a supported python");
    let path_var = std::env::var_os("PATH").expect("PATH");
    let tool = VenvTool::prefer_uv(find_executable(&path_var, "uv"));
    let proj = tempfile::tempdir().expect("tempdir");
    let venv = crate::venv::create(
        proj.path(),
        &crate::venv::CreateOptions::new(interp.path.clone(), tool.clone()),
        |_| {},
    )
    .await
    .expect("venv");
    let plan = collect(&src, &[]).expect("collect");
    install(
        &plan,
        &InstallOptions::new(venv.python.clone(), tool),
        |e| {
            if let InstallEvent::Output { line } = e {
                eprintln!("{line}");
            }
        },
    )
    .await
    .expect("install");
    let st = std::process::Command::new(&venv.python)
        .args(["-I", "-c", "import psycopg2, lxml, werkzeug, babel, gevent"])
        .status()
        .expect("python");
    assert!(st.success(), "core imports fail in the new venv");
}

#[test]
fn requirements_are_validated_when_read_back() {
    let r = parse_requirement("Babel >= 2.6.0").expect("req");
    let json = serde_json::to_value(&r).expect("json");
    assert_eq!(
        serde_json::from_value::<super::Requirement>(json).expect("back"),
        r
    );
    for bad in ["-e .", "git+https://x/y", "pkg @ file:///tmp/x", ""] {
        assert!(
            serde_json::from_value::<super::Requirement>(bad.into()).is_err(),
            "{bad}"
        );
    }
}
