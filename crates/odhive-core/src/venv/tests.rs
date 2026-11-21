use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};

use super::*;
use crate::python::find_executable;

fn python3() -> PathBuf {
    let path_var = std::env::var_os("PATH").expect("PATH");
    let p = find_executable(&path_var, "python3").expect("python3 on PATH");
    // Use the real interpreter, not a version-manager shim.
    fs::canonicalize(p).expect("canonicalize")
}

fn script(path: &Path, body: &str) -> PathBuf {
    fs::write(path, format!("#!/bin/sh\n{body}")).expect("write");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod");
    path.to_path_buf()
}

/// A fake `uv` that records its argv, then runs `body` with `$py` / `$dir` set
/// from `--python` and the argument after `--`.
fn fake_uv(dir: &Path, body: &str) -> PathBuf {
    let argv = dir.join("uv-argv");
    script(
        &dir.join("uv"),
        &format!(
            r#"printf '%s\n' "$@" > '{}'
py=; dir=
while [ $# -gt 0 ]; do
  case "$1" in
    --python) py="$2"; shift 2 ;;
    --) dir="$2"; shift 2 ;;
    *) shift ;;
  esac
done
{body}
"#,
            argv.display()
        ),
    )
}

fn events() -> (Arc<Mutex<Vec<VenvEvent>>>, impl FnMut(VenvEvent)) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&log);
    (log, move |e| sink.lock().expect("lock").push(e))
}

fn project() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

#[tokio::test]
async fn creates_with_stdlib_venv() {
    let proj = project();
    let mut opts = CreateOptions::new(python3(), VenvTool::Stdlib);
    opts.prompt = Some("shop".into());
    let (log, sink) = events();
    let venv = create(proj.path(), &opts, sink).await.expect("create");

    assert_eq!(venv.dir, proj.path().join(".venv"));
    assert_eq!(venv.python, proj.path().join(".venv/bin/python"));
    let probed = python::probe(&venv.python, python::DEFAULT_PROBE_TIMEOUT)
        .await
        .expect("probe");
    assert!(probed.venv);
    assert_eq!(probed.interpreter.version, venv.version);
    let cfg = fs::read_to_string(venv.dir.join("pyvenv.cfg")).expect("cfg");
    assert!(
        cfg.contains("prompt = 'shop'") || cfg.contains("prompt = shop"),
        "{cfg}"
    );

    let log = log.lock().expect("lock");
    assert!(matches!(
        log.first(),
        Some(VenvEvent::Started {
            tool: VenvTool::Stdlib,
            ..
        })
    ));
    assert!(
        matches!(log.last(), Some(VenvEvent::Finished { version, .. }) if *version == venv.version)
    );
}

#[tokio::test]
async fn uv_gets_a_fixed_argument_vector() {
    let proj = project();
    let tools = project();
    let uv = fake_uv(
        tools.path(),
        r#"echo "Creating virtual environment at: $dir" >&2; exec "$py" -I -m venv --without-pip "$dir""#,
    );
    let mut opts = CreateOptions::new(python3(), VenvTool::Uv(uv));
    opts.prompt = Some("my shop; rm -rf /".into());
    let (log, sink) = events();
    create(proj.path(), &opts, sink).await.expect("create");

    let argv = fs::read_to_string(tools.path().join("uv-argv")).expect("argv");
    let dir = proj.path().join(".venv");
    let expected = [
        "venv",
        "--no-config",
        "--no-project",
        "--no-python-downloads",
        "--python",
        &python3().to_string_lossy(),
        "--prompt",
        "my shop; rm -rf /",
        "--",
        &dir.to_string_lossy(),
    ]
    .join("\n");
    assert_eq!(argv.trim_end(), expected);
    assert!(log.lock().expect("lock").iter().any(|e| matches!(
        e,
        VenvEvent::Output { line } if line.starts_with("Creating virtual environment at: ")
    )));
}

#[tokio::test]
async fn existing_venv_is_kept_unless_replace() {
    let proj = project();
    let dir = proj.path().join(".venv");
    fs::create_dir(&dir).expect("mkdir");
    fs::write(dir.join("pyvenv.cfg"), "home = /x\n").expect("write");
    fs::write(dir.join("marker"), "keep").expect("write");

    let mut opts = CreateOptions::new(python3(), VenvTool::Stdlib);
    let err = create(proj.path(), &opts, |_| {})
        .await
        .expect_err("exists");
    assert!(matches!(err, VenvError::AlreadyExists(_)), "{err}");
    assert!(dir.join("marker").exists());

    opts.replace = true;
    let (log, sink) = events();
    create(proj.path(), &opts, sink).await.expect("replace");
    assert!(!dir.join("marker").exists());
    assert!(matches!(
        log.lock().expect("lock")[0],
        VenvEvent::Removing { .. }
    ));
}

#[tokio::test]
async fn replace_refuses_things_that_are_not_venvs() {
    let mut opts = CreateOptions::new(python3(), VenvTool::Stdlib);
    opts.replace = true;

    // A symlink to a real venv elsewhere: neither the link nor its target go.
    let proj = project();
    let elsewhere = project();
    fs::write(elsewhere.path().join("pyvenv.cfg"), "").expect("write");
    std::os::unix::fs::symlink(elsewhere.path(), proj.path().join(".venv")).expect("symlink");
    let err = create(proj.path(), &opts, |_| {})
        .await
        .expect_err("symlink");
    assert!(
        matches!(
            err,
            VenvError::NotAVenv {
                reason: "is a symlink",
                ..
            }
        ),
        "{err}"
    );
    assert!(elsewhere.path().join("pyvenv.cfg").exists());

    // A directory without pyvenv.cfg (someone's own folder).
    let proj = project();
    fs::create_dir(proj.path().join(".venv")).expect("mkdir");
    fs::write(proj.path().join(".venv/notes.txt"), "mine").expect("write");
    let err = create(proj.path(), &opts, |_| {})
        .await
        .expect_err("no cfg");
    assert!(matches!(err, VenvError::NotAVenv { .. }), "{err}");
    assert!(proj.path().join(".venv/notes.txt").exists());

    // A plain file.
    let proj = project();
    fs::write(proj.path().join(".venv"), "VAR=1").expect("write");
    let err = create(proj.path(), &opts, |_| {}).await.expect_err("file");
    assert!(
        matches!(
            err,
            VenvError::NotAVenv {
                reason: "is not a directory",
                ..
            }
        ),
        "{err}"
    );
    assert!(proj.path().join(".venv").is_file());
}

#[tokio::test]
async fn failed_tool_removes_partial_venv_and_reports_output() {
    let proj = project();
    let tools = project();
    let uv = fake_uv(
        tools.path(),
        r#"mkdir -p "$dir/lib"; echo 'error: No interpreter found' >&2; exit 2"#,
    );
    let opts = CreateOptions::new(python3(), VenvTool::Uv(uv));
    let err = create(proj.path(), &opts, |_| {}).await.expect_err("fail");
    match err {
        VenvError::Failed { output, .. } => {
            assert!(output.contains("No interpreter found"), "{output}")
        }
        other => panic!("{other}"),
    }
    assert!(!proj.path().join(".venv").exists());
}

#[tokio::test]
async fn timeout_kills_tool_and_removes_partial_venv() {
    let proj = project();
    let tools = project();
    let uv = fake_uv(tools.path(), r#"mkdir -p "$dir"; exec sleep 30"#);
    let mut opts = CreateOptions::new(python3(), VenvTool::Uv(uv));
    opts.timeout = Duration::from_millis(500);
    let start = std::time::Instant::now();
    let err = create(proj.path(), &opts, |_| {})
        .await
        .expect_err("timeout");
    assert!(matches!(err, VenvError::Timeout(_)), "{err}");
    assert!(start.elapsed() < Duration::from_secs(10));
    assert!(!proj.path().join(".venv").exists());
}

#[tokio::test]
async fn dropping_the_future_cancels_and_cleans_up() {
    let proj = project();
    let tools = project();
    let started = tools.path().join("started");
    let uv = fake_uv(
        tools.path(),
        &format!(
            r#"mkdir -p "$dir"; touch '{}'; exec sleep 30"#,
            started.display()
        ),
    );
    let opts = CreateOptions::new(python3(), VenvTool::Uv(uv));
    let fut = create(proj.path(), &opts, |_| {});
    let res = tokio::time::timeout(Duration::from_secs(3), async {
        tokio::pin!(fut);
        tokio::select! {
            r = &mut fut => Some(r),
            () = async {
                while !started.exists() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            } => None,
        }
    })
    .await
    .expect("tool started");
    assert!(res.is_none(), "create finished before cancel");
    // `fut` was dropped at the end of the block above.
    assert!(!proj.path().join(".venv").exists());
}

#[tokio::test]
async fn venv_that_does_not_run_as_venv_is_rejected() {
    let proj = project();
    let tools = project();
    // The "venv" python is a plain copy of a non-venv answer.
    let uv = fake_uv(
        tools.path(),
        r#"mkdir -p "$dir/bin"; printf '#!/bin/sh\nexec %s "$@"\n' "$py" > "$dir/bin/python"; chmod +x "$dir/bin/python""#,
    );
    let opts = CreateOptions::new(python3(), VenvTool::Uv(uv));
    let err = create(proj.path(), &opts, |_| {})
        .await
        .expect_err("verify");
    assert!(
        matches!(err, VenvError::Verify(ref m) if m.contains("does not run as a virtual environment")),
        "{err}"
    );
    assert!(!proj.path().join(".venv").exists());
}

#[tokio::test]
async fn missing_ensurepip_gets_a_helpful_error() {
    let proj = project();
    let tools = project();
    let real = python3();
    // Answers the probe like the real python; fails `-m venv` like Debian without python3-venv.
    let py = script(
        &tools.path().join("python3"),
        &format!(
            r#"if [ "$2" = "-B" ]; then exec '{}' "$@"; fi
echo 'The virtual environment was not created successfully because ensurepip is not' >&2
echo 'available.  On Debian/Ubuntu systems, you need to install the python3-venv' >&2
exit 1
"#,
            real.display()
        ),
    );
    let opts = CreateOptions::new(py, VenvTool::Stdlib);
    let err = create(proj.path(), &opts, |_| {})
        .await
        .expect_err("no venv");
    assert!(matches!(err, VenvError::StdlibVenvMissing { .. }), "{err}");
    assert!(err.to_string().contains("python3-venv"));
    assert!(!proj.path().join(".venv").exists());
}

#[tokio::test]
async fn base_interpreter_must_work_and_not_be_a_venv() {
    let proj = project();
    let opts = CreateOptions::new(proj.path().join("missing-python"), VenvTool::Stdlib);
    let err = create(proj.path(), &opts, |_| {})
        .await
        .expect_err("missing");
    assert!(matches!(err, VenvError::Verify(_)), "{err}");

    let base = project();
    let opts = CreateOptions::new(python3(), VenvTool::Stdlib);
    let first = create(base.path(), &opts, |_| {}).await.expect("create");
    let nested = CreateOptions::new(first.python, VenvTool::Stdlib);
    let err = create(proj.path(), &nested, |_| {})
        .await
        .expect_err("nested");
    assert!(
        matches!(err, VenvError::Verify(ref m) if m.contains("itself a virtual environment")),
        "{err}"
    );
    assert!(!proj.path().join(".venv").exists());
}

#[tokio::test]
async fn missing_project_dir_is_an_error() {
    let proj = project();
    let opts = CreateOptions::new(python3(), VenvTool::Stdlib);
    let err = create(&proj.path().join("nope"), &opts, |_| {})
        .await
        .expect_err("missing");
    assert!(matches!(err, VenvError::NoProjectDir(_)), "{err}");
}

/// The real uv, when installed (dev machines; CI runners usually lack it).
#[tokio::test]
async fn creates_with_real_uv_when_available() {
    let path_var = std::env::var_os("PATH").expect("PATH");
    let Some(uv) = find_executable(&path_var, "uv") else {
        eprintln!("uv not installed; skipping");
        return;
    };
    let proj = project();
    let mut opts = CreateOptions::new(python3(), VenvTool::Uv(uv));
    opts.prompt = Some("shop".into());
    let venv = create(proj.path(), &opts, |_| {}).await.expect("create");
    let probed = python::probe(&venv.python, python::DEFAULT_PROBE_TIMEOUT)
        .await
        .expect("probe");
    assert!(probed.venv);
}
