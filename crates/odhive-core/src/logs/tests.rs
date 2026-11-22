use std::os::unix::fs::{PermissionsExt, symlink};

use super::*;

fn mode(path: &Path) -> u32 {
    fs::metadata(path).expect("metadata").permissions().mode() & 0o777
}

fn rot(max_bytes: u64, keep: u32) -> Rotation {
    Rotation { max_bytes, keep }
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[test]
fn keeps_n_generations_oldest_last_and_tail_spans_them() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("main.log");
    // 5-byte lines, one per file.
    let mut log = LogFile::open(&path, rot(5, 3)).expect("open");
    for line in ["aaaa", "bbbb", "cccc", "dddd", "eeee", "ffff"] {
        log.write_line(line).expect("write");
    }
    assert_eq!(read(&path), "ffff\n");
    assert_eq!(read(&generation_path(&path, 1)), "eeee\n");
    assert_eq!(read(&generation_path(&path, 2)), "dddd\n");
    assert_eq!(read(&generation_path(&path, 3)), "cccc\n");
    assert!(!generation_path(&path, 4).exists());
    for g in 0..=3 {
        assert_eq!(mode(&generation_path(&path, g)), 0o600);
    }
    assert_eq!(read_tail(&path, 3).expect("tail"), ["dddd", "eeee", "ffff"]);
    assert_eq!(
        read_tail(&path, 99).expect("tail"),
        ["cccc", "dddd", "eeee", "ffff"]
    );
}

#[test]
fn lowering_keep_prunes_older_generations() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("main.log");
    let mut log = LogFile::open(&path, rot(5, 5)).expect("open");
    for line in ["1111", "2222", "3333", "4444", "5555", "6666"] {
        log.write_line(line).expect("write");
    }
    assert!(generation_path(&path, 5).exists());
    drop(log);
    let mut log = LogFile::open(&path, rot(5, 2)).expect("reopen");
    log.write_line("7777").expect("write");
    assert_eq!(read(&generation_path(&path, 1)), "6666\n");
    assert_eq!(read(&generation_path(&path, 2)), "5555\n");
    for g in 3..=6 {
        assert!(!generation_path(&path, g).exists(), "generation {g}");
    }
}

#[test]
fn tail_stops_at_a_gap_and_refuses_a_symlinked_generation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("main.log");
    fs::write(&path, "now\n").expect("write");
    fs::write(generation_path(&path, 2), "after-gap\n").expect("write");
    assert_eq!(read_tail(&path, 5).expect("tail"), ["now"]);

    let target = dir.path().join("secret");
    fs::write(&target, "secret\n").expect("write");
    symlink(&target, generation_path(&path, 1)).expect("symlink");
    assert!(read_tail(&path, 5).is_err());
    assert_eq!(read_tail(&path, 1).expect("tail"), ["now"]);
}

#[test]
fn rotation_moves_planted_symlinks_without_following_them() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("main.log");
    let target = dir.path().join("keep");
    fs::write(&target, "keep\n").expect("write");
    symlink(&target, generation_path(&path, 1)).expect("symlink");
    symlink(&target, generation_path(&path, 2)).expect("symlink");
    let mut log = LogFile::open(&path, rot(5, 2)).expect("open");
    log.write_line("aaaa").expect("write");
    log.write_line("bbbb").expect("write");
    assert_eq!(read(&target), "keep\n");
    assert_eq!(read(&generation_path(&path, 1)), "aaaa\n");
    assert!(
        fs::symlink_metadata(generation_path(&path, 2))
            .expect("meta")
            .file_type()
            .is_symlink()
    );
}

#[test]
fn settings_defaults_ranges_and_conversions() {
    let s = LogSettings::default();
    assert!(s.is_default());
    s.validate().expect("defaults are valid");
    assert_eq!(s.rotation(), Rotation::default());
    assert_eq!(s.rotation(), rot(16 * 1024 * 1024, 5));
    assert_eq!(s.buffer_limits().max_entries, 50_000);
    assert_eq!(s.buffer_limits().max_bytes, 64 * 1024 * 1024);

    let bad = [
        LogSettings {
            buffer_entries: 99,
            ..s
        },
        LogSettings {
            buffer_entries: 1_000_001,
            ..s
        },
        LogSettings { buffer_mb: 0, ..s },
        LogSettings {
            buffer_mb: 1025,
            ..s
        },
        LogSettings {
            file_max_mb: 0,
            ..s
        },
        LogSettings {
            file_max_mb: 1025,
            ..s
        },
        LogSettings { file_keep: 0, ..s },
        LogSettings { file_keep: 51, ..s },
    ];
    for b in bad {
        let msg = b.validate().expect_err("out of range");
        assert!(msg.contains("must be between"), "{msg}");
    }
    let edge = LogSettings {
        buffer_entries: 100,
        buffer_mb: 1024,
        file_max_mb: 1,
        file_keep: 50,
    };
    edge.validate().expect("edges are valid");
    assert!(!edge.is_default());
}

#[test]
fn settings_toml() {
    let s: LogSettings = toml::from_str("file_keep = 9").expect("parse");
    assert_eq!(
        s,
        LogSettings {
            file_keep: 9,
            ..LogSettings::default()
        }
    );
    assert!(toml::from_str::<LogSettings>("max = 1").is_err());
    assert!(toml::from_str::<LogSettings>("file_keep = -1").is_err());
}

#[test]
fn log_path_validates_the_instance() {
    let dir = Path::new("/p/logs");
    assert_eq!(log_path(dir, "main").expect("path"), dir.join("main.log"));
    for bad in ["", "../x", "a/b", "Main", "a.b"] {
        assert!(log_path(dir, bad).is_err(), "{bad:?}");
    }
    let main = dir.join("main.log");
    assert_eq!(generation_path(&main, 0), main);
    assert_eq!(generation_path(&main, 1), dir.join("main.log.1"));
    assert_eq!(generation_path(&main, 12), dir.join("main.log.12"));
}

#[test]
fn writes_are_private_and_appended() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("logs/main.log");
    let mut log = LogFile::open(&path, Rotation::default()).expect("open");
    log.write_line("one").expect("write");
    drop(log);
    let mut log = LogFile::open(&path, Rotation::default()).expect("reopen");
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
    assert!(LogFile::open(&path, Rotation::default()).is_err());
    assert!(read_tail(&path, 5).is_err());
    assert_eq!(fs::read_to_string(&target).expect("read"), "keep\n");
}

#[test]
fn rotation_keeps_one_generation_and_tail_spans_both() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("main.log");
    let mut log = LogFile::open(&path, rot(12, 1)).expect("open");
    for line in ["aaaa", "bbbb", "cccc", "dddd", "eeee"] {
        log.write_line(line).expect("write");
    }
    // 12 bytes hold two 5-byte lines.
    assert_eq!(fs::read_to_string(&path).expect("read"), "eeee\n");
    assert_eq!(
        fs::read_to_string(generation_path(&path, 1)).expect("read"),
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

    let mut log = LogFile::open(&path, rot(16, 1)).expect("open");
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
    assert!(generation_path(&path, 1).exists());
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
