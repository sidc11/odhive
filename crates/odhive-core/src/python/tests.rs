use std::os::unix::fs::PermissionsExt;

use super::*;

const T: Duration = Duration::from_secs(20);

/// Writes an executable test fixture that prints `stdout` and exits with `code`.
fn fake(dir: &Path, name: &str, stdout: &str, code: i32) -> PathBuf {
    let p = dir.join(name);
    let body = format!("#!/bin/sh\ncat <<'JSON'\n{stdout}\nJSON\nexit {code}\n");
    fs::write(&p, body).expect("write");
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).expect("chmod");
    p
}

fn probe_json(exe: &Path, version: [u32; 3], imp: &str, ft: bool, venv: bool) -> String {
    serde_json::json!({
        "format": 1,
        "version": version,
        "executable": exe,
        "implementation": imp,
        "free_threaded": ft,
        "venv": venv,
    })
    .to_string()
}

/// A fake interpreter that reports itself as `sys.executable`.
fn fake_python(dir: &Path, name: &str, version: [u32; 3]) -> PathBuf {
    let p = dir.join(name);
    fake(
        dir,
        name,
        &probe_json(&p, version, "cpython", false, false),
        0,
    )
}

fn interp(version: (u8, u8, u16), imp: &str, ft: bool) -> Interpreter {
    Interpreter {
        path: PathBuf::from(format!(
            "/py/{}.{}.{}{imp}{ft}",
            version.0, version.1, version.2
        )),
        version: PyVersion::new(version.0, version.1, version.2),
        implementation: imp.into(),
        free_threaded: ft,
        origins: vec![Origin::Path],
    }
}

fn opts(path_var: Option<&Path>) -> DiscoverOptions {
    DiscoverOptions {
        path_var: path_var.map(|p| p.as_os_str().to_owned()),
        uv: None,
        explicit: Vec::new(),
        timeout: T,
    }
}

fn v(s: &str) -> OdooVersion {
    s.parse().expect("version")
}

#[test]
fn reads_range_from_release_py() {
    let text = "version_info = (19, 0, 0, FINAL, 0, '')\n\nMIN_PY_VERSION = (3, 10)\nMAX_PY_VERSION = (3, 14)\nMIN_PG_VERSION = 13\n";
    assert_eq!(
        PySupport::from_release_py(text),
        Some(PySupport {
            min: (3, 10),
            max: (3, 14)
        })
    );
    // Missing, commented, indented, inverted, non-3 or odd shapes are refused.
    for bad in [
        "MIN_PY_VERSION = (3, 10)\n",
        "# MIN_PY_VERSION = (3, 10)\n# MAX_PY_VERSION = (3, 14)\n",
        "    MIN_PY_VERSION = (3, 10)\n    MAX_PY_VERSION = (3, 14)\n",
        "MIN_PY_VERSION = (3, 14)\nMAX_PY_VERSION = (3, 10)\n",
        "MIN_PY_VERSION = (2, 7)\nMAX_PY_VERSION = (3, 14)\n",
        "MIN_PY_VERSION = (3, 10, 1)\nMAX_PY_VERSION = (3, 14)\n",
        "MIN_PY_VERSION == (3, 10)\nMAX_PY_VERSION = (3, 14)\n",
        "MIN_PY_VERSION = get()\nMAX_PY_VERSION = (3, 14)\n",
    ] {
        assert_eq!(PySupport::from_release_py(bad), None, "{bad:?}");
    }
    assert!(
        PySupport::from_release_py("MIN_PY_VERSION=(3,12,)\nMAX_PY_VERSION = (3, 14)").is_some()
    );
}

#[test]
fn range_falls_back_to_known_odoo_values() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert_eq!(PySupport::for_source(dir.path(), v("19.0")).min, (3, 10));
    assert_eq!(PySupport::for_source(dir.path(), v("20.0")).min, (3, 12));
    assert_eq!(
        PySupport::for_source(dir.path(), v("saas~19.2")).max,
        (3, 14)
    );

    fs::create_dir(dir.path().join("odoo")).expect("mkdir");
    fs::write(
        dir.path().join("odoo/release.py"),
        "MIN_PY_VERSION = (3, 11)\nMAX_PY_VERSION = (3, 15)\n",
    )
    .expect("write");
    let s = PySupport::for_source(dir.path(), v("19.0"));
    assert_eq!((s.min, s.max), ((3, 11), (3, 15)));
    assert_eq!(s.to_string(), "Python 3.11–3.15");
}

#[test]
fn checks_compat_by_minor_version() {
    let s = PySupport::for_odoo(v("19.0"));
    assert_eq!(s.check(PyVersion::new(3, 9, 18)), Compat::TooOld);
    assert_eq!(s.check(PyVersion::new(3, 10, 0)), Compat::Supported);
    assert_eq!(s.check(PyVersion::new(3, 14, 99)), Compat::Supported);
    assert_eq!(s.check(PyVersion::new(3, 15, 0)), Compat::TooNew);
}

#[test]
fn recognizes_python_file_names() {
    for ok in [
        "python3",
        "python3.12",
        "python3.9",
        "python3.14t",
        "python3.100",
    ] {
        assert!(is_python_name(ok), "{ok}");
    }
    for bad in [
        "python",
        "python2",
        "python2.7",
        "python3-config",
        "python3.12-config",
        "python3.",
        "python3.t",
        "python3.1234",
        "python3.12m",
        "python3x",
        "ipython3",
    ] {
        assert!(!is_python_name(bad), "{bad}");
    }
}

#[test]
fn picks_newest_supported_standard_cpython() {
    let s = PySupport::for_odoo(v("19.0"));
    let list = vec![
        interp((3, 9, 1), "cpython", false),
        interp((3, 12, 3), "cpython", false),
        interp((3, 13, 1), "cpython", true),
        interp((3, 13, 0), "pypy", false),
        interp((3, 12, 9), "cpython", false),
        interp((3, 15, 0), "cpython", false),
    ];
    assert_eq!(
        pick(&list, s).expect("pick").version,
        PyVersion::new(3, 12, 9)
    );

    // Ties keep discovery order (PATH before uv).
    let mut a = interp((3, 12, 9), "cpython", false);
    a.path = "/first".into();
    let mut b = a.clone();
    b.path = "/second".into();
    assert_eq!(pick(&[a, b], s).expect("pick").path, Path::new("/first"));
}

#[test]
fn pick_explains_what_was_found() {
    let s = PySupport::for_odoo(v("20.0"));
    assert_eq!(pick(&[], s), Err(PickError::NoneFound(s)));
    let list = vec![
        interp((3, 11, 2), "cpython", false),
        interp((3, 13, 1), "cpython", true),
        interp((3, 12, 0), "pypy", false),
    ];
    let err = pick(&list, s).expect_err("none");
    assert_eq!(
        err.to_string(),
        "no suitable interpreter for Python 3.12–3.14; found: 3.11.2, 3.13.1 (free-threaded), 3.12.0 (pypy)"
    );
}

#[test]
fn parses_probe_output_strictly() {
    let dir = tempfile::tempdir().expect("tempdir");
    let exe = dir.path().join("python3");
    fs::write(&exe, "").expect("write");
    let ok = probe_json(&exe, [3, 12, 4], "cpython", false, false);
    let p = parse_probe(&exe, ok.as_bytes()).expect("ok");
    assert_eq!(p.interpreter.version, PyVersion::new(3, 12, 4));
    assert_eq!(p.interpreter.path, fs::canonicalize(&exe).expect("canon"));
    assert!(!p.venv);

    let bad_cases = [
        "".to_owned(),
        "Python 3.12.4".to_owned(),
        probe_json(&exe, [2, 7, 18], "cpython", false, false),
        probe_json(&exe, [3, 300, 0], "cpython", false, false),
        probe_json(&exe, [3, 12, 4], "c python\n", false, false),
        probe_json(&exe, [3, 12, 4], "", false, false),
        ok.replace("\"format\":1", "\"format\":2"),
        ok.replace("[3,12,4]", "[3,12]"),
        probe_json(
            &dir.path().join("missing"),
            [3, 12, 4],
            "cpython",
            false,
            false,
        ),
    ];
    for bad in bad_cases {
        assert!(
            matches!(
                parse_probe(&exe, bad.as_bytes()),
                Err(PythonError::BadOutput { .. })
            ),
            "{bad}"
        );
    }
    // A relative sys.executable falls back to the path that was run.
    let rel = ok.replace(&*exe.to_string_lossy(), "python3");
    assert_eq!(
        parse_probe(&exe, rel.as_bytes())
            .expect("rel")
            .interpreter
            .path,
        fs::canonicalize(&exe).expect("canon")
    );
}

#[test]
fn parses_uv_list() {
    let json = r#"[
        {"key":"cpython-3.12.16-linux-x86_64-gnu","version":"3.12.16","path":"/uv/bin/python3.12"},
        {"key":"cpython-3.13.18-linux-x86_64-gnu","version":"3.13.18","path":null,"url":"https://x"},
        {"key":"rel","path":"relative/python3"}
    ]"#;
    assert_eq!(
        parse_uv_list(json.as_bytes()).expect("parse"),
        vec![PathBuf::from("/uv/bin/python3.12")]
    );
    assert!(parse_uv_list(b"warning: oops").is_err());
}

#[tokio::test]
async fn discovers_fakes_on_path_and_skips_bad_ones() {
    let a = tempfile::tempdir().expect("a");
    let b = tempfile::tempdir().expect("b");
    let py312 = fake_python(a.path(), "python3.12", [3, 12, 5]);
    // `python3` in the same dir is a symlink to python3.12: probed, then merged.
    std::os::unix::fs::symlink(&py312, a.path().join("python3")).expect("symlink");
    fake(a.path(), "python3.11", "garbage", 0);
    fake(a.path(), "python3.10", "", 3);
    let venv = a.path().join("python3.13");
    fake(
        a.path(),
        "python3.13",
        &probe_json(&venv, [3, 13, 0], "cpython", false, true),
        0,
    );
    // Not executable, wrong names: never probed.
    fs::write(a.path().join("python3.9"), "#!/bin/sh\nexit 0\n").expect("write");
    fake_python(a.path(), "python3-config", [3, 12, 5]);
    fake_python(a.path(), "python2.7", [3, 12, 5]);
    let py314 = fake_python(b.path(), "python3.14", [3, 14, 1]);

    let path_var = std::env::join_paths([
        a.path().to_path_buf(),
        PathBuf::from("relative/dir"),
        PathBuf::from("/nonexistent/odhive-test"),
        b.path().to_path_buf(),
    ])
    .expect("join");
    let mut o = opts(None);
    o.path_var = Some(path_var);
    let d = discover(&o).await;

    let found: Vec<_> = d
        .interpreters
        .iter()
        .map(|i| i.version.to_string())
        .collect();
    assert_eq!(found, ["3.12.5", "3.14.1"]);
    assert_eq!(
        d.interpreters[0].path,
        fs::canonicalize(&py312).expect("canon")
    );
    assert_eq!(
        d.interpreters[1].path,
        fs::canonicalize(&py314).expect("canon")
    );
    let mut skipped: Vec<_> = d
        .skipped
        .iter()
        .map(|s| {
            s.path
                .file_name()
                .expect("name")
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    skipped.sort();
    assert_eq!(skipped, ["python3.10", "python3.11", "python3.13"]);
    assert!(
        d.skipped
            .iter()
            .any(|s| s.reason == "is a virtual environment")
    );
    assert!(d.uv_error.is_none());

    let s = PySupport::for_odoo(v("19.0"));
    assert_eq!(
        pick(&d.interpreters, s).expect("pick").version,
        PyVersion::new(3, 14, 1)
    );
}

#[tokio::test]
async fn uv_results_are_probed_and_merged() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bin = dir.path().join("bin");
    let uvdir = dir.path().join("uv-pythons");
    fs::create_dir_all(&bin).expect("mkdir");
    fs::create_dir_all(&uvdir).expect("mkdir");
    let on_path = fake_python(&bin, "python3.12", [3, 12, 5]);
    let uv_only = fake_python(&uvdir, "python3.13", [3, 13, 2]);
    let list = serde_json::json!([
        {"path": on_path},
        {"path": uv_only},
        {"path": null},
    ]);
    let uv = fake(dir.path(), "uv", &list.to_string(), 0);

    let mut o = opts(Some(&bin));
    o.uv = Some(uv);
    let d = discover(&o).await;
    assert!(d.uv_error.is_none(), "{:?}", d.uv_error);
    assert_eq!(d.interpreters.len(), 2);
    assert_eq!(d.interpreters[0].origins, [Origin::Path, Origin::Uv]);
    assert_eq!(d.interpreters[1].origins, [Origin::Uv]);
    assert!(d.skipped.is_empty());

    // A failing uv is reported, PATH results still come back.
    o.uv = Some(fake(dir.path(), "uv-broken", "", 2));
    let d = discover(&o).await;
    assert_eq!(d.interpreters.len(), 1);
    assert!(d.uv_error.is_some());
}

#[tokio::test]
async fn explicit_paths_come_first_and_errors_are_kept() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mine = fake_python(dir.path(), "mypython", [3, 12, 1]);
    let mut o = opts(None);
    o.explicit = vec![mine, dir.path().join("missing-python")];
    let d = discover(&o).await;
    assert_eq!(d.interpreters.len(), 1);
    assert_eq!(d.interpreters[0].origins, [Origin::Explicit]);
    assert_eq!(d.skipped.len(), 1);
    assert!(
        d.skipped[0].reason.contains("cannot run"),
        "{}",
        d.skipped[0].reason
    );
}

#[tokio::test]
async fn probe_times_out() {
    let dir = tempfile::tempdir().expect("tempdir");
    let p = dir.path().join("python3");
    fs::write(&p, "#!/bin/sh\nexec sleep 30\n").expect("write");
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).expect("chmod");
    let start = std::time::Instant::now();
    let err = probe(&p, Duration::from_millis(300))
        .await
        .expect_err("timeout");
    assert!(matches!(err, PythonError::Timeout { .. }), "{err}");
    assert!(start.elapsed() < Duration::from_secs(10));
}

#[tokio::test]
async fn probe_caps_output() {
    let dir = tempfile::tempdir().expect("tempdir");
    let p = dir.path().join("python3");
    fs::write(&p, "#!/bin/sh\nhead -c 200000 /dev/zero | tr '\\0' 'x'\n").expect("write");
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).expect("chmod");
    let err = probe(&p, T).await.expect_err("too large");
    assert!(
        matches!(err, PythonError::BadOutput { ref reason, .. } if reason == "output too large"),
        "{err}"
    );
}

/// The real probe script against the `python3` on this machine (CI has one).
#[tokio::test]
async fn probes_a_real_python() {
    let path_var = std::env::var_os("PATH").expect("PATH");
    let python = find_executable(&path_var, "python3").expect("python3 on PATH");
    let p = probe(&python, T).await.expect("probe");
    assert_eq!(p.interpreter.version.major, 3);
    assert!(p.interpreter.version.minor >= 8);
    assert!(p.interpreter.path.is_absolute());
    assert!(!p.interpreter.implementation.is_empty());
}

/// `-I` keeps `PYTHONPATH` / `PYTHONSTARTUP` code out of the probe.
#[tokio::test]
async fn real_probe_ignores_python_env() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("json.py"), "raise SystemExit('hijacked')\n").expect("write");
    fs::write(
        dir.path().join("sysconfig.py"),
        "raise SystemExit('hijacked')\n",
    )
    .expect("write");
    let path_var = std::env::var_os("PATH").expect("PATH");
    let python = find_executable(&path_var, "python3").expect("python3 on PATH");
    let out = Command::new(&python)
        .args(["-I", "-B", "-c", PROBE_PY])
        .current_dir(dir.path())
        .env("PYTHONPATH", dir.path())
        .env("PYTHONSTARTUP", dir.path().join("json.py"))
        .output()
        .await
        .expect("run");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    parse_probe(&python, &out.stdout).expect("parse");
}

/// A path seen again (PATH, then uv) merges its origin even when the path is not
/// canonical — e.g. macOS temp dirs under `/var`, which is `/private/var`.
#[tokio::test]
async fn repeated_non_canonical_path_keeps_both_origins() {
    let dir = tempfile::tempdir().expect("tempdir");
    let real = dir.path().join("real");
    fs::create_dir(&real).expect("mkdir");
    std::os::unix::fs::symlink(&real, dir.path().join("link")).expect("symlink");
    let exe = fake_python(&real, "python3.12", [3, 12, 5]);
    let via_link = dir.path().join("link/python3.12");
    let uv = fake(
        dir.path(),
        "uv",
        &serde_json::json!([{ "path": via_link }]).to_string(),
        0,
    );
    let mut o = opts(Some(&dir.path().join("link")));
    o.uv = Some(uv);
    let d = discover(&o).await;
    assert_eq!(d.interpreters.len(), 1);
    assert_eq!(
        d.interpreters[0].path,
        fs::canonicalize(&exe).expect("canon")
    );
    assert_eq!(d.interpreters[0].origins, [Origin::Path, Origin::Uv]);
}

#[test]
fn versions_read_back_what_they_serialize_to() {
    let v = PyVersion::new(3, 12, 7);
    let json = serde_json::to_value(v).expect("json");
    assert_eq!(json, "3.12.7");
    assert_eq!(serde_json::from_value::<PyVersion>(json).expect("back"), v);
    for bad in ["3.12", "3.12.7.1", "3.x.1", "", "3.12.70000"] {
        assert!(
            serde_json::from_value::<PyVersion>(bad.into()).is_err(),
            "{bad}"
        );
    }
}
