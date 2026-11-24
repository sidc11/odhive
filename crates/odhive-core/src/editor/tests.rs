use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};

use super::*;

fn executable(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    fs::write(&p, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    p
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|&x| x.to_owned()).collect()
}

// --- allowed_file -----------------------------------------------------------

struct Tree {
    _tmp: tempfile::TempDir,
    project: PathBuf,
    source: PathBuf,
    outside: PathBuf,
}

fn tree() -> Tree {
    let tmp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(tmp.path()).unwrap();
    let project = base.join("project");
    let source = base.join("source");
    let outside = base.join("outside");
    for d in [&project, &source, &outside] {
        fs::create_dir_all(d.join("pkg")).unwrap();
        fs::write(d.join("pkg/mod.py"), "x = 1\n").unwrap();
    }
    Tree {
        _tmp: tmp,
        project,
        source,
        outside,
    }
}

fn roots(t: &Tree) -> Vec<PathBuf> {
    vec![t.project.clone(), t.source.clone()]
}

fn path_str(p: &Path) -> String {
    p.to_str().unwrap().to_owned()
}

#[test]
fn accepts_files_inside_any_root() {
    let t = tree();
    for root in [&t.project, &t.source] {
        let file = root.join("pkg/mod.py");
        assert_eq!(allowed_file(&roots(&t), &path_str(&file), 1).unwrap(), file);
    }
}

#[test]
fn rejects_files_outside_the_roots() {
    let t = tree();
    let file = t.outside.join("pkg/mod.py");
    assert!(matches!(
        allowed_file(&roots(&t), &path_str(&file), 1),
        Err(EditorError::NotAllowed(_))
    ));
    assert!(matches!(
        allowed_file(&roots(&t), "/etc/passwd", 1),
        Err(EditorError::NotAllowed(_))
    ));
}

#[test]
fn rejects_dot_dot_escapes() {
    let t = tree();
    let sneaky = format!("{}/../outside/pkg/mod.py", t.project.display());
    assert!(matches!(
        allowed_file(&roots(&t), &sneaky, 1),
        Err(EditorError::NotAllowed(_))
    ));
}

#[test]
fn rejects_symlinks_leading_out() {
    let t = tree();
    let link = t.project.join("pkg/link.py");
    symlink(t.outside.join("pkg/mod.py"), &link).unwrap();
    assert!(matches!(
        allowed_file(&roots(&t), &path_str(&link), 1),
        Err(EditorError::NotAllowed(_))
    ));
    let dir_link = t.project.join("escape");
    symlink(&t.outside, &dir_link).unwrap();
    let via = dir_link.join("pkg/mod.py");
    assert!(matches!(
        allowed_file(&roots(&t), &path_str(&via), 1),
        Err(EditorError::NotAllowed(_))
    ));
}

#[test]
fn follows_symlinks_that_stay_inside() {
    let t = tree();
    let link = t.project.join("pkg/alias.py");
    symlink(t.source.join("pkg/mod.py"), &link).unwrap();
    assert_eq!(
        allowed_file(&roots(&t), &path_str(&link), 3).unwrap(),
        t.source.join("pkg/mod.py")
    );
}

#[test]
fn rejects_relative_paths_and_nul() {
    let t = tree();
    for bad in ["pkg/mod.py", "", "./pkg/mod.py", "/x\0/y"] {
        assert!(
            matches!(
                allowed_file(&roots(&t), bad, 1),
                Err(EditorError::NotAbsolute(_))
            ),
            "{bad:?}"
        );
    }
}

#[test]
fn rejects_missing_files_and_directories() {
    let t = tree();
    let missing = t.project.join("pkg/nope.py");
    assert!(matches!(
        allowed_file(&roots(&t), &path_str(&missing), 1),
        Err(EditorError::NoFile(_))
    ));
    assert!(matches!(
        allowed_file(&roots(&t), &path_str(&t.project.join("pkg")), 1),
        Err(EditorError::NoFile(_))
    ));
    let dangling = t.project.join("pkg/dangling.py");
    symlink(t.project.join("pkg/gone.py"), &dangling).unwrap();
    assert!(allowed_file(&roots(&t), &path_str(&dangling), 1).is_err());
}

#[test]
fn rejects_bad_line_numbers() {
    let t = tree();
    let file = path_str(&t.project.join("pkg/mod.py"));
    assert!(matches!(
        allowed_file(&roots(&t), &file, 0),
        Err(EditorError::BadLine(0))
    ));
    assert!(matches!(
        allowed_file(&roots(&t), &file, MAX_LINE + 1),
        Err(EditorError::BadLine(_))
    ));
    assert!(allowed_file(&roots(&t), &file, MAX_LINE).is_ok());
}

#[test]
fn skips_missing_roots() {
    let t = tree();
    let file = t.source.join("pkg/mod.py");
    let roots = vec![t.project.join("does-not-exist"), t.source.clone()];
    assert_eq!(allowed_file(&roots, &path_str(&file), 1).unwrap(), file);
    assert!(matches!(
        allowed_file(&[t.project.join("nope")], &path_str(&file), 1),
        Err(EditorError::NotAllowed(_))
    ));
}

// --- Editor::resolve ----------------------------------------------------------

#[test]
fn config_template_wins_and_fills_placeholders() {
    let bin = tempfile::tempdir().unwrap();
    let ed = executable(bin.path(), "myeditor");
    executable(bin.path(), "code");
    let template = s(&["myeditor", "--goto", "{file}:{line}", "--line={line}"]);
    let e = Editor::resolve(Some(&template), Some("code"), None, bin.path().as_os_str()).unwrap();
    assert_eq!(e.source, EditorSource::Config);
    assert_eq!(e.program(), ed);
    assert_eq!(
        e.args(Path::new("/p/a b.py"), 12),
        s(&["--goto", "/p/a b.py:12", "--line=12"])
    );
}

#[test]
fn template_without_file_placeholder_appends_the_file() {
    let e = Editor::resolve(
        Some(&s(&["/usr/bin/true", "-n"])),
        None,
        None,
        OsStr::new(""),
    )
    .unwrap();
    assert_eq!(e.args(Path::new("/p/x.py"), 3), s(&["-n", "/p/x.py"]));
}

#[test]
fn empty_template_is_an_error() {
    assert!(matches!(
        Editor::resolve(Some(&[]), None, None, OsStr::new("")),
        Err(EditorError::EmptyTemplate)
    ));
    assert!(matches!(
        Editor::resolve(Some(&s(&[" "])), None, None, OsStr::new("")),
        Err(EditorError::EmptyTemplate)
    ));
}

#[test]
fn template_program_must_exist_and_relative_paths_are_refused() {
    let bin = tempfile::tempdir().unwrap();
    assert!(matches!(
        Editor::resolve(Some(&s(&["missing"])), None, None, bin.path().as_os_str()),
        Err(EditorError::NotFound)
    ));
    assert!(matches!(
        Editor::resolve(Some(&s(&["./ed"])), None, None, bin.path().as_os_str()),
        Err(EditorError::NotFound)
    ));
    let plain = bin.path().join("plain");
    fs::write(&plain, "not executable").unwrap();
    assert!(matches!(
        Editor::resolve(
            Some(&s(&[plain.to_str().unwrap()])),
            None,
            None,
            OsStr::new("")
        ),
        Err(EditorError::NotFound)
    ));
}

#[test]
fn known_gui_editors_from_the_environment() {
    let bin = tempfile::tempdir().unwrap();
    for name in ["code", "subl", "pycharm", "gedit", "codium"] {
        executable(bin.path(), name);
    }
    let path = bin.path().as_os_str();
    let file = Path::new("/p/m.py");

    let e = Editor::resolve(None, Some("code --wait"), Some("subl"), path).unwrap();
    assert_eq!(e.source, EditorSource::Visual);
    assert_eq!(e.args(file, 7), s(&["-g", "/p/m.py:7"]));

    let e = Editor::resolve(None, None, Some("subl"), path).unwrap();
    assert_eq!(e.source, EditorSource::Editor);
    assert_eq!(e.args(file, 7), s(&["/p/m.py:7"]));

    let e = Editor::resolve(None, Some("pycharm"), None, path).unwrap();
    assert_eq!(e.args(file, 7), s(&["--line", "7", "/p/m.py"]));

    let e = Editor::resolve(None, Some("gedit"), None, path).unwrap();
    assert_eq!(e.args(file, 7), s(&["+7", "/p/m.py"]));

    let e = Editor::resolve(None, Some("codium --new-window -w"), None, path).unwrap();
    assert_eq!(e.args(file, 7), s(&["--new-window", "-g", "/p/m.py:7"]));
}

#[test]
fn terminal_and_unknown_editors_fall_back_to_code() {
    let bin = tempfile::tempdir().unwrap();
    let code = executable(bin.path(), "code");
    executable(bin.path(), "nvim");
    executable(bin.path(), "vim");
    let e = Editor::resolve(None, Some("nvim"), Some("vim"), bin.path().as_os_str()).unwrap();
    assert_eq!(e.source, EditorSource::Default);
    assert_eq!(e.program(), code);
}

#[test]
fn known_editor_not_installed_is_skipped() {
    let bin = tempfile::tempdir().unwrap();
    executable(bin.path(), "subl");
    let e = Editor::resolve(None, Some("zed"), Some("subl"), bin.path().as_os_str()).unwrap();
    assert_eq!(e.source, EditorSource::Editor);
}

#[test]
fn nothing_found() {
    let bin = tempfile::tempdir().unwrap();
    executable(bin.path(), "vim");
    assert!(matches!(
        Editor::resolve(None, Some("vim"), Some(""), bin.path().as_os_str()),
        Err(EditorError::NotFound)
    ));
}

#[test]
fn absolute_program_paths_from_the_environment() {
    let bin = tempfile::tempdir().unwrap();
    let code = executable(bin.path(), "code");
    let visual = code.to_str().unwrap().to_owned();
    let e = Editor::resolve(None, Some(&visual), None, OsStr::new("")).unwrap();
    assert_eq!(e.source, EditorSource::Visual);
    assert_eq!(e.program(), code);
}

#[tokio::test]
async fn open_runs_the_program_with_the_args() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out.txt");
    let script = dir.path().join("ed");
    fs::write(
        &script,
        format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n", out.display()),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let template = s(&[script.to_str().unwrap(), "+{line}", "{file}"]);
    let e = Editor::resolve(Some(&template), None, None, OsStr::new("")).unwrap();
    e.open(Path::new("/p/it's here.py"), 42).unwrap();
    let mut text = String::new();
    for _ in 0..100 {
        text = fs::read_to_string(&out).unwrap_or_default();
        if text.ends_with(".py\n") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(text, "+42\n/p/it's here.py\n");
}

#[tokio::test]
async fn open_reports_spawn_failures() {
    let dir = tempfile::tempdir().unwrap();
    // Executable but not a program the kernel can run.
    let bad = dir.path().join("ed");
    fs::write(&bad, [0u8, 1, 2, 3]).unwrap();
    fs::set_permissions(&bad, fs::Permissions::from_mode(0o755)).unwrap();
    let template = s(&[bad.to_str().unwrap()]);
    let e = Editor::resolve(Some(&template), None, None, OsStr::new("")).unwrap();
    assert!(matches!(
        e.open(Path::new("/p/x.py"), 1),
        Err(EditorError::Spawn { .. })
    ));
}
