use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};

use super::*;
use crate::logs::parse::Level;

fn utc() -> Zones {
    Zones::default()
}

fn parse(lines: &[&str], zones: Zones) -> Vec<Entry> {
    let mut p = PgParser::new(zones);
    let mut out: Vec<Entry> = lines.iter().filter_map(|l| p.push(l)).collect();
    out.extend(p.flush());
    out
}

#[test]
fn parses_debian_prefix_with_user_and_db() {
    let e = parse(
        &[
            "2026-11-27 10:15:01.123 UTC [4321] odoo@demo ERROR:  relation \"x\" does not exist at character 15",
            "2026-11-27 10:15:01.123 UTC [4321] odoo@demo STATEMENT:  SELECT *",
            "\tFROM x",
            "2026-11-27 10:15:02.000 UTC [99] LOG:  checkpoint starting: time",
        ],
        utc(),
    );
    assert_eq!(e.len(), 2);
    let h = e[0].header.as_ref().expect("header");
    assert_eq!(h.timestamp.to_rfc3339(), "2026-11-27T10:15:01.123Z");
    assert_eq!(h.pid, 4321);
    assert_eq!(h.level, Level::Error);
    assert_eq!(h.db.as_deref(), Some("demo"));
    assert_eq!(h.logger, LOGGER);
    assert_eq!(
        e[0].message,
        "relation \"x\" does not exist at character 15\nSTATEMENT:  SELECT *\n\tFROM x"
    );
    let h = e[1].header.as_ref().expect("header");
    assert_eq!(h.level, Level::Info);
    assert_eq!(h.db, None);
    assert_eq!(e[1].message, "checkpoint starting: time");
}

#[test]
fn maps_every_severity() {
    let cases = [
        ("DEBUG3", Level::Debug),
        ("INFO", Level::Info),
        ("NOTICE", Level::Info),
        ("LOG", Level::Info),
        ("WARNING", Level::Warning),
        ("ERROR", Level::Error),
        ("FATAL", Level::Critical),
        ("PANIC", Level::Critical),
    ];
    for (sev, level) in cases {
        let line = format!("2026-01-01 00:00:00.000 UTC [1] {sev}:  x");
        let e = parse(&[&line], utc());
        assert_eq!(e[0].level(), Some(level), "{sev}");
    }
}

#[test]
fn detail_of_another_process_starts_its_own_entry() {
    let e = parse(
        &[
            "2026-01-01 00:00:00.000 UTC [1] ERROR:  a",
            "2026-01-01 00:00:00.000 UTC [2] DETAIL:  b",
        ],
        utc(),
    );
    assert_eq!(e.len(), 2);
    assert_eq!(e[1].message, "DETAIL:  b");
}

#[test]
fn converts_zones_to_utc() {
    let zones = Zones::from_pairs(&[("CET", 3600)]);
    let cases = [
        ("2026-01-01 00:30:00.500 CET", "2025-12-31T23:30:00.500Z"),
        ("2026-01-01 05:30:00 +0530", "2026-01-01T00:00:00.000Z"),
        ("2026-01-01 05:30:00.1 +05:30", "2026-01-01T00:00:00.100Z"),
        ("2026-01-01 00:00:00.000 -03", "2026-01-01T03:00:00.000Z"),
        ("2026-01-01 00:00:00.000 GMT", "2026-01-01T00:00:00.000Z"),
        // Unknown abbreviations are read as UTC.
        ("2026-01-01 00:00:00.000 XYZ", "2026-01-01T00:00:00.000Z"),
    ];
    for (ts, want) in cases {
        let line = format!("{ts} [7] LOG:  x");
        let e = parse(&[&line], zones.clone());
        let h = e[0].header.as_ref().unwrap_or_else(|| panic!("{ts}"));
        assert_eq!(h.timestamp.to_rfc3339(), want, "{ts}");
    }
}

#[test]
fn custom_prefix_text_is_skipped_and_unformatted_lines_kept() {
    let e = parse(
        &[
            "starting server",
            "2026-01-01 00:00:00.000 UTC [5] app=psql bob@shop WARNING:  careful",
            "",
            "2026-01-01 00:00:00.000 UTC [5] [unknown]@[unknown] LOG:  connection received",
        ],
        utc(),
    );
    assert_eq!(e.len(), 3);
    assert!(e[0].header.is_none());
    assert_eq!(e[0].message, "starting server");
    assert_eq!(
        e[1].header.as_ref().and_then(|h| h.db.as_deref()),
        Some("shop")
    );
    assert_eq!(e[2].header.as_ref().and_then(|h| h.db.clone()), None);
}

#[test]
fn rejects_impossible_dates() {
    let e = parse(&["2026-13-01 00:00:00.000 UTC [5] LOG:  x"], utc());
    assert!(e[0].header.is_none());
}

/// A TZif v2 file with an empty first block (as `zic -b slim` writes) and
/// the given types; transitions use them in order.
fn tzif(types: &[(i32, &str)]) -> Vec<u8> {
    fn header(out: &mut Vec<u8>, timecnt: u32, typecnt: u32, charcnt: u32) {
        out.extend_from_slice(b"TZif2");
        out.extend_from_slice(&[0; 15]);
        for n in [0, 0, 0, timecnt, typecnt, charcnt] {
            out.extend_from_slice(&n.to_be_bytes());
        }
    }
    let mut chars = Vec::new();
    let mut infos = Vec::new();
    for &(off, name) in types {
        infos.extend_from_slice(&off.to_be_bytes());
        infos.push(0);
        infos.push(u8::try_from(chars.len()).expect("small"));
        chars.extend_from_slice(name.as_bytes());
        chars.push(0);
    }
    let mut out = Vec::new();
    header(&mut out, 0, 1, 1);
    out.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0]);
    let n = u32::try_from(types.len()).expect("small");
    header(&mut out, n, n, u32::try_from(chars.len()).expect("small"));
    for i in 0..types.len() {
        out.extend_from_slice(&(i64::try_from(i).expect("small") * 1000).to_be_bytes());
    }
    out.extend((0..types.len()).map(|i| u8::try_from(i).expect("small")));
    out.extend_from_slice(&infos);
    out.extend_from_slice(&chars);
    out.extend_from_slice(b"\nCET-1CEST,M3.5.0,M10.5.0/3\n");
    out
}

#[test]
fn reads_abbreviations_from_a_zone_file() {
    let z = Zones::from_tzif(&tzif(&[(600, "LMT"), (3600, "CET"), (7200, "CEST")])).expect("tzif");
    assert_eq!(z.offset("CET"), Some(3600));
    assert_eq!(z.offset("CEST"), Some(7200));
    assert_eq!(z.offset("LMT"), Some(600));
    assert_eq!(z.offset("EST"), None);
    assert!(Zones::from_tzif(b"not a zone file").is_none());
    assert!(Zones::from_tzif(&tzif(&[(3600, "CET")])[..60]).is_none());
}

#[test]
fn reads_a_real_zone_file_when_present() {
    let path = Path::new("/usr/share/zoneinfo/Europe/Paris");
    if !path.exists() {
        return;
    }
    let z = Zones::load(path);
    assert_eq!(z.offset("CET"), Some(3600));
    assert_eq!(z.offset("CEST"), Some(7200));
}

#[test]
fn validate_rejects_relative_traversal_and_other_names() {
    assert_eq!(
        validate_log_path(Path::new("pg.log")),
        Err(LogPathError::Relative)
    );
    for p in [
        "/var/log/../../etc/shadow.log",
        "/var/log/./postgresql.log",
        "/var/log/postgresql/..",
    ] {
        assert_eq!(
            validate_log_path(Path::new(p)),
            Err(LogPathError::Traversal),
            "{p}"
        );
    }
    for p in [
        "/etc/passwd",
        "/var/log/postgresql/",
        "/x/.log",
        "/x/a.log.1",
    ] {
        assert_eq!(
            validate_log_path(Path::new(p)),
            Err(LogPathError::NotLogName),
            "{p}"
        );
    }
    assert_eq!(validate_log_path(Path::new("/var/log/pg.log")), Ok(()));
}

#[test]
fn check_rejects_folders_missing_files_and_links_to_other_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = fs::canonicalize(dir.path()).expect("canonical");
    let folder = base.join("folder.log");
    fs::create_dir(&folder).expect("mkdir");
    assert_eq!(check_log_path(&folder), Err(LogPathError::NotFile(folder)));

    let missing = base.join("missing.log");
    assert_eq!(
        check_log_path(&missing),
        Err(LogPathError::Missing(missing))
    );

    let secret = base.join("secret.txt");
    fs::write(&secret, "x").expect("write");
    let link = base.join("link.log");
    symlink(&secret, &link).expect("symlink");
    assert_eq!(check_log_path(&link), Err(LogPathError::NotLogName));

    let real = base.join("pg.log");
    fs::write(&real, "x\n").expect("write");
    let good_link = base.join("current.log");
    symlink(&real, &good_link).expect("symlink");
    assert_eq!(check_log_path(&good_link), Ok(real.clone()));
    assert_eq!(check_log_path(&real), Ok(real));
}

#[test]
fn detect_lists_matching_files_newest_first() {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    let old = d.join("postgresql-15-main.log");
    let new = d.join("postgresql-16-main.log");
    fs::write(&old, "").expect("write");
    fs::write(&new, "").expect("write");
    fs::write(d.join("postgresql-16-main.log.1"), "").expect("write");
    fs::write(d.join("other.log"), "").expect("write");
    fs::create_dir(d.join("postgresql-dir.log")).expect("mkdir");
    symlink(&new, d.join("postgresql-link.log")).expect("symlink");
    let past = SystemTime::now() - std::time::Duration::from_secs(3600);
    fs::File::options()
        .write(true)
        .open(&old)
        .and_then(|f| f.set_modified(past))
        .expect("touch");
    let dirs = [
        SearchDir::new(d, "postgresql"),
        SearchDir::new(d.join("missing"), "postgresql"),
    ];
    let found: Vec<PathBuf> = detect(&dirs).into_iter().map(|c| c.path).collect();
    assert_eq!(found, [new, old]);
}

#[test]
fn group_name_reads_group_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("group");
    fs::write(&file, "root:x:0:\nadm:x:4:syslog,sid\nbad line\n").expect("write");
    assert_eq!(group_name(&file, 4).as_deref(), Some("adm"));
    assert_eq!(group_name(&file, 9), None);
}

#[test]
fn permission_hint_names_the_file() {
    let hint = permission_hint(Path::new("/nonexistent/pg.log"));
    assert!(hint.contains("/nonexistent/pg.log"), "{hint}");
}

fn limits() -> Limits {
    Limits {
        max_entries: 100,
        max_bytes: 1 << 20,
    }
}

#[test]
fn tail_reads_history_then_follows_and_flushes_when_idle() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("pg.log");
    let mut f = fs::File::create(&path).expect("create");
    writeln!(f, "2026-01-01 00:00:00.000 UTC [1] LOG:  one").expect("write");
    writeln!(f, "2026-01-01 00:00:01.000 UTC [1] LOG:  two").expect("write");
    let mut tail = PgLogTail::with_limits(&path, utc(), 1, limits()).expect("open");
    let first: Vec<String> = tail
        .tail(10)
        .iter()
        .map(|r| r.entry.message.clone())
        .collect();
    assert_eq!(first, ["two"]);

    writeln!(f, "2026-01-01 00:00:02.000 UTC [1] ERROR:  three").expect("write");
    tail.poll().expect("poll");
    // Waiting for DETAIL lines.
    assert_eq!(tail.read_from(2, 10).records.len(), 0);
    writeln!(f, "2026-01-01 00:00:02.000 UTC [1] DETAIL:  more").expect("write");
    tail.poll().expect("poll");
    tail.poll().expect("poll");
    let page = tail.read_from(2, 10);
    assert_eq!(page.records.len(), 1);
    assert_eq!(page.records[0].entry.message, "three\nDETAIL:  more");
    assert_eq!(page.next, 3);
    assert_eq!(tail.next_seq(), 3);
    assert_eq!(tail.path(), path);
}

#[test]
fn tail_follows_rotation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("pg.log");
    fs::write(&path, "").expect("write");
    let mut tail = PgLogTail::with_limits(&path, utc(), 10, limits()).expect("open");
    fs::rename(&path, dir.path().join("pg.log.1")).expect("rotate");
    fs::write(&path, "2026-01-01 00:00:00.000 UTC [1] LOG:  after\n").expect("write");
    tail.poll().expect("poll");
    tail.poll().expect("poll");
    let msgs: Vec<String> = tail
        .tail(10)
        .iter()
        .map(|r| r.entry.message.clone())
        .collect();
    assert_eq!(msgs, ["after"]);
}

#[test]
fn tail_refuses_unreadable_file() {
    if crate::platform::effective_uid() == 0 {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("pg.log");
    fs::write(&path, "x\n").expect("write");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).expect("chmod");
    let err = PgLogTail::open(&path, utc()).expect_err("unreadable");
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
}
