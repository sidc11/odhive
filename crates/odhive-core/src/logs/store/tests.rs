use std::fs;

use proptest::prelude::*;

use super::*;
use crate::logs::parse::parse_all;
use crate::logs::{generation_path, read_tail};

fn text(message: &str) -> Entry {
    Entry {
        header: None,
        message: message.to_owned(),
        locations: Vec::new(),
        exceptions: Vec::new(),
        truncated: false,
    }
}

fn messages(records: &[Arc<Record>]) -> Vec<&str> {
    records.iter().map(|r| r.entry.message.as_str()).collect()
}

fn seqs(records: &[Arc<Record>]) -> Vec<u64> {
    records.iter().map(|r| r.seq).collect()
}

fn roomy(max_entries: usize) -> Limits {
    Limits {
        max_entries,
        max_bytes: usize::MAX,
    }
}

#[test]
fn default_limits_match_the_roadmap() {
    let l = Limits::default();
    assert_eq!(l.max_entries, 50_000);
    assert_eq!(l.max_bytes, 64 * 1024 * 1024);
}

#[test]
fn evicts_the_oldest_by_count_and_keeps_counting() {
    let mut b = LogBuffer::new(roomy(3));
    assert!(b.is_empty());
    assert_eq!((b.first_seq(), b.next_seq()), (1, 1));
    for m in ["a", "b", "c", "d", "e"] {
        b.push(text(m));
    }
    assert_eq!(b.len(), 3);
    assert_eq!((b.first_seq(), b.next_seq()), (3, 6));
    assert_eq!(messages(&b.tail(10)), ["c", "d", "e"]);
    assert_eq!(messages(&b.tail(2)), ["d", "e"]);
    assert!(b.tail(0).is_empty());

    b.clear();
    assert!(b.is_empty());
    assert_eq!(b.bytes(), 0);
    assert_eq!(
        b.push(text("f")).seq,
        6,
        "sequence numbers are never reused"
    );
}

#[test]
fn zero_entry_limit_still_keeps_the_newest() {
    let mut b = LogBuffer::new(roomy(0));
    b.push(text("a"));
    b.push(text("b"));
    assert_eq!(messages(&b.tail(5)), ["b"]);
}

#[test]
fn evicts_by_memory_budget_but_keeps_the_newest() {
    let small = entry_size(&text("x"));
    let mut b = LogBuffer::new(Limits {
        max_entries: 1000,
        max_bytes: small * 3,
    });
    for _ in 0..10 {
        b.push(text("x"));
    }
    assert_eq!(b.len(), 3);
    assert!(b.bytes() <= small * 3);

    // One entry larger than the whole budget replaces everything else.
    let huge = "y".repeat(small * 10);
    b.push(text(&huge));
    assert_eq!(b.len(), 1);
    assert_eq!(b.tail(1)[0].entry.message, huge);
    assert_eq!(b.bytes(), entry_size(&text(&huge)));
}

#[test]
fn entry_size_counts_every_owned_string() {
    let entries = parse_all(include_str!("../parse/fixtures/odoo-19-import.log"));
    let tb = entries
        .iter()
        .find(|e| e.has_traceback())
        .expect("a traceback");
    let size = entry_size(tb);
    let strings: usize = tb.message.len()
        + tb.locations.iter().map(|l| l.path.len()).sum::<usize>()
        + tb.exceptions.iter().map(String::len).sum::<usize>();
    assert!(size > strings, "{size} <= {strings}");
}

#[test]
fn read_from_pages_and_reports_skipped_entries() {
    let mut b = LogBuffer::new(roomy(4));
    for i in 1..=6 {
        b.push(text(&i.to_string()));
    }
    // Held: 3..=6.
    let p = b.read_from(0, 2);
    assert_eq!((seqs(&p.records), p.skipped, p.next), (vec![3, 4], 2, 5));
    let p = b.read_from(p.next, 10);
    assert_eq!((seqs(&p.records), p.skipped, p.next), (vec![5, 6], 0, 7));
    // Caught up: nothing new, cursor stays.
    let p = b.read_from(p.next, 10);
    assert_eq!((p.records.len(), p.skipped, p.next), (0, 0, 7));
    // A cursor from the future is clamped to the end.
    let p = b.read_from(99, 10);
    assert_eq!((p.records.len(), p.skipped, p.next), (0, 0, 7));
    // A stale cursor learns how much it missed.
    b.push(text("7"));
    b.push(text("8"));
    let p = b.read_from(2, 1);
    assert_eq!((seqs(&p.records), p.skipped, p.next), (vec![5], 3, 6));
    assert!(b.read_from(1, 0).records.is_empty());
}

proptest! {
    /// Whatever the limits and sizes, the buffer stays within them, holds a
    /// contiguous run of the newest entries, and paging sees each once.
    #[test]
    fn buffer_invariants(
        sizes in proptest::collection::vec(0usize..400, 1..200),
        max_entries in 1usize..50,
        budget in 1usize..8_000,
        page in 1usize..20,
    ) {
        let mut b = LogBuffer::new(Limits { max_entries, max_bytes: budget });
        for (i, n) in sizes.iter().enumerate() {
            let r = b.push(text(&"z".repeat(*n)));
            prop_assert_eq!(r.seq, i as u64 + 1);
            prop_assert!(b.len() <= max_entries);
            prop_assert!(b.bytes() <= budget || b.len() == 1);
            prop_assert_eq!(b.tail(1)[0].seq, r.seq);
        }
        let all = b.tail(usize::MAX);
        prop_assert_eq!(all.len(), b.len());
        prop_assert_eq!(all.iter().map(|r| entry_size(&r.entry)).sum::<usize>(), b.bytes());
        for (k, r) in all.iter().enumerate() {
            prop_assert_eq!(r.seq, b.first_seq() + k as u64);
        }
        prop_assert_eq!(b.next_seq(), sizes.len() as u64 + 1);

        let mut cursor = 0;
        let mut seen = Vec::new();
        let mut skipped = 0;
        loop {
            let p = b.read_from(cursor, page);
            skipped += p.skipped;
            if p.records.is_empty() {
                break;
            }
            seen.extend(seqs(&p.records));
            cursor = p.next;
        }
        prop_assert_eq!(seen, seqs(&all));
        prop_assert_eq!(skipped, b.first_seq() - 1);
    }
}

#[test]
fn instance_log_parses_into_the_buffer_and_writes_the_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut log = InstanceLog::open(dir.path(), "main", &LogSettings::default()).expect("open");
    assert!(log.has_file());
    let raw = include_str!("../parse/fixtures/odoo-20-import.log");
    let mut completed = Vec::new();
    for line in raw.lines() {
        let pushed = log.push_line(line);
        assert!(pushed.file_error.is_none());
        completed.extend(pushed.record);
    }
    assert!(log.has_pending());
    completed.extend(log.flush());
    assert!(!log.has_pending());
    assert!(log.flush().is_none());

    let expected = parse_all(raw);
    assert_eq!(completed.len(), expected.len());
    let stored: Vec<Entry> = log
        .buffer()
        .tail(usize::MAX)
        .iter()
        .map(|r| r.entry.clone())
        .collect();
    assert_eq!(stored, expected);
    assert!(stored.iter().any(Entry::has_traceback));

    let file = fs::read_to_string(dir.path().join("main.log")).expect("read");
    assert_eq!(
        file.lines().collect::<Vec<_>>(),
        raw.lines().collect::<Vec<_>>()
    );

    log.clear();
    assert!(log.buffer().is_empty());
    assert!(
        !fs::read_to_string(dir.path().join("main.log"))
            .expect("read")
            .is_empty()
    );
}

#[test]
fn markers_end_the_pending_entry_and_stand_alone() {
    let mut log = InstanceLog::in_memory(&LogSettings::default());
    assert!(!log.has_file());
    log.push_line("2026-11-22 10:00:00,000 1 ERROR db odoo.x: boom");
    log.push_line("Traceback (most recent call last):");
    let (done, err) = log.push_marker("----- odhive: started pid 7 -----");
    assert!(err.is_none());
    assert_eq!(done.len(), 2);
    assert!(done[0].entry.message.starts_with("boom\nTraceback"));
    assert!(done[1].entry.header.is_none());
    assert_eq!(done[1].entry.message, "----- odhive: started pid 7 -----");
    assert!(!log.has_pending());
    // An unformatted line after the marker is its own entry, not merged.
    log.push_line("plain output");
    assert_eq!(log.flush().expect("entry").entry.message, "plain output");
    assert_eq!(log.buffer().len(), 3);
}

#[test]
fn instance_log_respects_settings() {
    let dir = tempfile::tempdir().expect("tempdir");
    let settings = LogSettings {
        buffer_entries: 100,
        buffer_mb: 1,
        file_max_mb: 1,
        file_keep: 2,
    };
    let mut log = InstanceLog::open(dir.path(), "main", &settings).expect("open");
    assert_eq!(log.buffer().limits().max_entries, 100);
    // ~3.5 MiB of 1 KiB records: rotates three times, keeps two generations.
    let body = "m".repeat(1000);
    for i in 0..3600 {
        log.push_line(&format!(
            "2026-11-22 10:00:00,000 1 INFO db odoo.x: {i} {body}"
        ));
    }
    log.flush();
    assert_eq!(log.buffer().len(), 100);
    let path = dir.path().join("main.log");
    assert!(generation_path(&path, 1).exists());
    assert!(generation_path(&path, 2).exists());
    assert!(!generation_path(&path, 3).exists());
    let tail = read_tail(&path, 1).expect("tail");
    assert!(tail[0].contains(" 3599 "));
}

#[test]
fn invalid_instance_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let err = InstanceLog::open(dir.path(), "../x", &LogSettings::default()).expect_err("bad id");
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    assert!(fs::read_dir(dir.path()).expect("dir").next().is_none());
}

#[cfg(unix)]
#[test]
fn a_failing_file_is_dropped_but_the_buffer_keeps_going() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("tempdir");
    let settings = LogSettings {
        file_max_mb: 1,
        ..LogSettings::default()
    };
    let mut log = InstanceLog::open(dir.path(), "main", &settings).expect("open");
    let line = |m: &str| format!("2026-11-22 10:00:00,000 1 INFO db odoo.x: {m}");
    log.push_line(&line("first"));
    // A read-only folder makes the next rotation fail.
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o500)).expect("chmod");
    let pushed = log.push_line(&line(&"b".repeat(1024 * 1024)));
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).expect("chmod");
    if rustix::process::geteuid().is_root() {
        return; // root ignores the folder mode
    }
    assert!(pushed.file_error.is_some());
    assert!(!log.has_file());
    assert!(log.push_line(&line("later")).file_error.is_none());
    log.flush();
    assert_eq!(log.buffer().len(), 3);
    assert_eq!(log.buffer().tail(1)[0].entry.message, "later");
}
