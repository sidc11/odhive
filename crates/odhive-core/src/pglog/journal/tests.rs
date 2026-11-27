use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::json;

use super::*;
use crate::logs::parse::Level;

const UNIT: &str = "postgresql.service";

fn limits() -> Limits {
    Limits {
        max_entries: 100,
        max_bytes: 1 << 20,
    }
}

/// One journal entry as `journalctl --output=json` prints it.
fn entry(i: u32, ident: &str, pid: u32, priority: u8, message: &str) -> String {
    json!({
        "__CURSOR": format!("s=abc;i={i:x}"),
        "__REALTIME_TIMESTAMP": format!("{}", 1_795_288_154_894_026_u64 + u64::from(i) * 1_000_000),
        "_PID": pid.to_string(),
        "PRIORITY": priority.to_string(),
        "SYSLOG_IDENTIFIER": ident,
        "_SYSTEMD_UNIT": UNIT,
        "MESSAGE": message,
    })
    .to_string()
}

/// A copy of the fake journalctl in its own folder, with `lines` as its
/// journal.
fn fake(lines: &[String]) -> (tempfile::TempDir, Journal) {
    let dir = tempfile::tempdir().expect("tempdir");
    let src =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/fake-journalctl");
    let exe = dir.path().join("journalctl");
    fs::copy(src, &exe).expect("copy fixture");
    write_journal(dir.path(), lines);
    let journal = Journal {
        journalctl: exe,
        unit: UNIT.to_owned(),
    };
    (dir, journal)
}

fn write_journal(dir: &Path, lines: &[String]) {
    let mut text = lines.join("\n");
    text.push('\n');
    fs::write(dir.join("journal.jsonl"), text).expect("write journal");
}

fn messages(tail: &JournalTail) -> Vec<String> {
    tail.tail(100)
        .iter()
        .map(|r| r.entry.message.clone())
        .collect()
}

#[test]
fn unit_names_are_postgresql_or_versioned() {
    for ok in ["postgresql.service", "postgresql-16.service"] {
        assert!(is_unit_name(ok), "{ok}");
    }
    for bad in [
        "postgresql@.service",
        "postgresql@16-main.service",
        "postgresql.socket",
        "sshd.service",
        "postgresql-16.service --all",
        "--unit=x",
        "postgresql-.service",
    ] {
        assert!(!is_unit_name(bad), "{bad}");
    }
}

#[test]
fn detect_unit_prefers_plain_then_newest_major() {
    let a = tempfile::tempdir().expect("tempdir");
    let b = tempfile::tempdir().expect("tempdir");
    let dirs = vec![
        a.path().to_path_buf(),
        b.path().to_path_buf(),
        PathBuf::from("/nonexistent/systemd"),
    ];
    assert_eq!(detect_unit(&dirs), None);
    fs::write(a.path().join("postgresql@.service"), "").expect("write");
    fs::write(a.path().join("sshd.service"), "").expect("write");
    assert_eq!(detect_unit(&dirs), None);
    fs::write(a.path().join("postgresql-15.service"), "").expect("write");
    fs::write(b.path().join("postgresql-17.service"), "").expect("write");
    assert_eq!(detect_unit(&dirs).as_deref(), Some("postgresql-17.service"));
    fs::write(b.path().join("postgresql.service"), "").expect("write");
    assert_eq!(detect_unit(&dirs).as_deref(), Some("postgresql.service"));
}

#[test]
fn args_name_the_unit_and_cursor_as_single_arguments() {
    let a = args(UNIT, Some("s=1;i=2"), 10);
    let a: Vec<&str> = a.iter().map(|s| s.to_str().expect("utf-8")).collect();
    assert_eq!(
        a,
        [
            "--unit=postgresql.service",
            "--output=json",
            "--all",
            "--no-pager",
            "--lines=10",
            "--after-cursor=s=1;i=2",
        ]
    );
}

#[test]
fn json_lines_become_entries_with_postgres_or_journal_headers() {
    let out = [
        entry(1, "systemd", 1, 6, "Starting PostgreSQL database server..."),
        entry(
            2,
            "postgres",
            42,
            6,
            "2026-11-27 10:15:01.123 UTC [42] odoo@shop ERROR:  relation \"x\" does not exist",
        ),
        entry(
            3,
            "postgres",
            42,
            6,
            "2026-11-27 10:15:01.123 UTC [42] odoo@shop STATEMENT:  SELECT *",
        ),
        entry(4, "postgres", 42, 6, "\tFROM x"),
        entry(5, "postgres", 43, 4, "no prefix here"),
        "not json".to_owned(),
        json!({"MESSAGE": "no cursor"}).to_string(),
        json!({"__CURSOR": "c", "MESSAGE": [104, 105, 255], "PRIORITY": "3"}).to_string(),
    ]
    .join("\n");
    let e = parse_output(&out, Zones::default());
    assert_eq!(e.len(), 4, "{e:#?}");

    let h = e[0].header.as_ref().expect("header");
    assert_eq!(h.logger, "systemd");
    assert_eq!(h.pid, 1);
    assert_eq!(h.level, Level::Info);
    assert_eq!(h.timestamp.to_rfc3339(), "2026-11-21T19:09:15.894Z");
    assert_eq!(e[0].message, "Starting PostgreSQL database server...");

    let h = e[1].header.as_ref().expect("header");
    assert_eq!(h.logger, LOGGER);
    assert_eq!(h.level, Level::Error);
    assert_eq!(h.db.as_deref(), Some("shop"));
    assert_eq!(h.timestamp.to_rfc3339(), "2026-11-27T10:15:01.123Z");
    assert_eq!(
        e[1].message,
        "relation \"x\" does not exist\nSTATEMENT:  SELECT *\n\tFROM x"
    );

    // Without PostgreSQL's prefix: the journal's fields.
    let h = e[2].header.as_ref().expect("header");
    assert_eq!(
        (h.logger.as_str(), h.pid, h.level),
        (LOGGER, 43, Level::Warning)
    );

    // Bytes that are not UTF-8.
    assert_eq!(e[3].message, "hi\u{fffd}");
    assert_eq!(e[3].header.as_ref().expect("header").level, Level::Error);
}

#[test]
fn priorities_map_to_levels() {
    let cases = [
        ("0", Level::Critical),
        ("2", Level::Critical),
        ("3", Level::Error),
        ("4", Level::Warning),
        ("5", Level::Info),
        ("6", Level::Info),
        ("7", Level::Debug),
        ("x", Level::Info),
    ];
    for (p, want) in cases {
        assert_eq!(level_of(Some(p)), want, "{p}");
    }
    assert_eq!(level_of(None), Level::Info);
}

#[test]
fn tail_reads_the_last_lines_then_follows_after_the_cursor() {
    let mut lines: Vec<String> = (1..=3)
        .map(|i| {
            entry(
                i,
                "postgres",
                7,
                6,
                &format!("2026-11-27 10:00:0{i}.000 UTC [7] LOG:  line {i}"),
            )
        })
        .collect();
    let (dir, journal) = fake(&lines);
    let mut tail = JournalTail::with_limits(journal, Zones::default(), 2, limits()).expect("open");
    tail.set_min_interval(Duration::ZERO);
    assert_eq!(messages(&tail), ["line 2", "line 3"]);
    assert_eq!(tail.unit(), UNIT);

    lines.push(entry(
        4,
        "postgres",
        7,
        6,
        "2026-11-27 10:00:04.000 UTC [7] LOG:  line 4",
    ));
    lines.push(entry(
        5,
        "systemd",
        1,
        6,
        "Stopped PostgreSQL database server.",
    ));
    write_journal(dir.path(), &lines);
    tail.poll().expect("poll");
    // The systemd line completed line 4; it waits for more lines itself.
    assert_eq!(messages(&tail), ["line 2", "line 3", "line 4"]);
    tail.poll().expect("poll");
    assert_eq!(
        messages(&tail),
        [
            "line 2",
            "line 3",
            "line 4",
            "Stopped PostgreSQL database server."
        ]
    );

    let calls = fs::read_to_string(dir.path().join("calls.txt")).expect("calls");
    let calls: Vec<&str> = calls.lines().collect();
    assert_eq!(
        calls[0],
        "--unit=postgresql.service --output=json --all --no-pager --lines=2"
    );
    assert_eq!(
        calls[1],
        format!(
            "--unit=postgresql.service --output=json --all --no-pager --lines={POLL_LINES} --after-cursor=s=abc;i=3"
        )
    );
    assert!(
        calls[2].ends_with("--after-cursor=s=abc;i=5"),
        "{}",
        calls[2]
    );
}

#[test]
fn polls_closer_than_the_interval_do_not_run_journalctl() {
    let (dir, journal) = fake(&[]);
    let mut tail = JournalTail::with_limits(journal, Zones::default(), 10, limits()).expect("open");
    tail.set_min_interval(Duration::from_secs(3600));
    tail.poll().expect("poll");
    tail.poll().expect("poll");
    let calls = fs::read_to_string(dir.path().join("calls.txt")).expect("calls");
    assert_eq!(calls.lines().count(), 1);
}

#[test]
fn an_empty_journal_keeps_asking_for_the_last_lines() {
    let (dir, journal) = fake(&[]);
    let mut tail = JournalTail::with_limits(journal, Zones::default(), 10, limits()).expect("open");
    tail.set_min_interval(Duration::ZERO);
    assert!(messages(&tail).is_empty());
    write_journal(dir.path(), &[entry(1, "systemd", 1, 6, "Started.")]);
    tail.poll().expect("poll");
    tail.poll().expect("poll");
    assert_eq!(messages(&tail), ["Started."]);
    let calls = fs::read_to_string(dir.path().join("calls.txt")).expect("calls");
    assert!(
        calls
            .lines()
            .nth(1)
            .expect("second")
            .ends_with("--lines=10")
    );
}

#[test]
fn a_user_who_may_not_read_the_journal_is_told_so() {
    let (dir, journal) = fake(&[]);
    fs::write(dir.path().join("denied"), "").expect("write");
    let err = JournalTail::open(journal, Zones::default()).expect_err("denied");
    assert!(matches!(err, JournalError::PermissionDenied), "{err}");
    assert!(permission_hint().contains("systemd-journal"));
}

#[test]
fn a_failing_journalctl_is_an_error_with_its_message() {
    let (dir, journal) = fake(&[]);
    fs::write(dir.path().join("fail"), "Failed to open journal: bad\n").expect("write");
    let err = JournalTail::open(journal, Zones::default()).expect_err("fails");
    match err {
        JournalError::Failed(m) => assert_eq!(m, "Failed to open journal: bad"),
        other => panic!("{other}"),
    }
}

#[test]
fn a_missing_journalctl_or_bad_unit_is_refused() {
    let journal = Journal {
        journalctl: PathBuf::from("/nonexistent/journalctl"),
        unit: UNIT.to_owned(),
    };
    assert!(matches!(
        JournalTail::open(journal, Zones::default()),
        Err(JournalError::Io(_))
    ));
    let (_dir, mut journal) = fake(&[]);
    journal.unit = "--all".to_owned();
    assert!(matches!(
        JournalTail::open(journal, Zones::default()),
        Err(JournalError::BadUnit(_))
    ));
}

/// Against this machine's journal, when it has journalctl and a PostgreSQL
/// unit: either readable, or refused for permission.
#[test]
fn reads_the_real_journal_when_there() {
    let (Some(journalctl), Some(unit)) = (default_journalctl(), detect_unit(&default_unit_dirs()))
    else {
        return;
    };
    match JournalTail::open(Journal { journalctl, unit }, Zones::system()) {
        Ok(tail) => {
            for r in tail.tail(50) {
                assert!(r.entry.header.is_some(), "{:?}", r.entry);
            }
        }
        Err(JournalError::PermissionDenied) => {}
        Err(e) => panic!("{e}"),
    }
}
