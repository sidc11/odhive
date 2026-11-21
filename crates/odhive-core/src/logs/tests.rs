use std::os::unix::fs::{PermissionsExt, symlink};

use super::*;

fn mode(path: &Path) -> u32 {
    fs::metadata(path).expect("metadata").permissions().mode() & 0o777
}

#[test]
fn log_path_validates_the_instance() {
    let dir = Path::new("/p/logs");
    assert_eq!(log_path(dir, "main").expect("path"), dir.join("main.log"));
    for bad in ["", "../x", "a/b", "Main", "a.b"] {
        assert!(log_path(dir, bad).is_err(), "{bad:?}");
    }
    assert_eq!(rotated_path(&dir.join("main.log")), dir.join("main.log.1"));
}

#[test]
fn writes_are_private_and_appended() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("logs/main.log");
    let mut log = LogFile::open(&path, DEFAULT_MAX_BYTES).expect("open");
    log.write_line("one").expect("write");
    drop(log);
    let mut log = LogFile::open(&path, DEFAULT_MAX_BYTES).expect("reopen");
    log.write_line("two").expect("write");
    assert_eq!(fs::read_to_string(&path).expect("read"), "one\ntwo\n");
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(&dir.path().join("logs")), 0o700);
}

#[test]
fn a_symlinked_log_file_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("elsewhere");
    fs::write(&target, "keep\n").expect("write");
    let path = dir.path().join("main.log");
    symlink(&target, &path).expect("symlink");
    assert!(LogFile::open(&path, DEFAULT_MAX_BYTES).is_err());
    assert!(read_tail(&path, 5).is_err());
    assert_eq!(fs::read_to_string(&target).expect("read"), "keep\n");
}

#[test]
fn rotation_keeps_one_generation_and_tail_spans_both() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("main.log");
    let mut log = LogFile::open(&path, 12).expect("open");
    for line in ["aaaa", "bbbb", "cccc", "dddd", "eeee"] {
        log.write_line(line).expect("write");
    }
    // 12 bytes hold two 5-byte lines.
    assert_eq!(fs::read_to_string(&path).expect("read"), "eeee\n");
    assert_eq!(
        fs::read_to_string(rotated_path(&path)).expect("read"),
        "cccc\ndddd\n"
    );
    assert_eq!(read_tail(&path, 2).expect("tail"), ["dddd", "eeee"]);
    assert_eq!(
        read_tail(&path, 10).expect("tail"),
        ["cccc", "dddd", "eeee"]
    );
    assert!(read_tail(&path, 0).expect("tail").is_empty());
    assert!(
        read_tail(&dir.path().join("none.log"), 3)
            .expect("tail")
            .is_empty()
    );
}

#[test]
fn tail_keeps_an_unterminated_last_line_and_strips_cr() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("main.log");
    fs::write(&path, "a\r\nb\n\nc").expect("write");
    assert_eq!(read_tail(&path, 3).expect("tail"), ["b", "", "c"]);
}

#[test]
fn follower_sees_new_lines_partial_lines_and_rotation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("main.log");
    let mut f = Follower::from_end(&path);
    assert!(f.poll().expect("poll").is_empty());

    let mut log = LogFile::open(&path, 16).expect("open");
    log.write_line("first").expect("write");
    assert_eq!(f.poll().expect("poll"), ["first"]);

    // Half a line is held back until its newline arrives.
    let mut raw = OpenOptions::new().append(true).open(&path).expect("open");
    raw.write_all(b"par").expect("write");
    assert!(f.poll().expect("poll").is_empty());
    raw.write_all(b"t\n").expect("write");
    assert_eq!(f.poll().expect("poll"), ["part"]);
    drop(raw);

    // Rotation: the next line goes to a fresh file.
    log.write_line("after-rotation").expect("write");
    assert!(rotated_path(&path).exists());
    assert_eq!(f.poll().expect("poll"), ["after-rotation"]);
}

#[test]
fn follower_from_end_skips_existing_lines_and_restarts_on_truncation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("main.log");
    fs::write(&path, "old\n").expect("write");
    let mut f = Follower::from_end(&path);
    assert!(f.poll().expect("poll").is_empty());
    fs::write(&path, "x\n").expect("truncate");
    assert_eq!(f.poll().expect("poll"), ["x"]);
}
