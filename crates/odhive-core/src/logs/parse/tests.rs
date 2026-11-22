use std::collections::BTreeMap;

use proptest::prelude::*;

use super::*;

const LINE_19: &str = "2026-11-22 06:27:39,726 6340 INFO ? odoo: Odoo version 19.0 ";
const LINE_20: &str =
    "2026-11-22 06:28:04,911 6418 INFO ohs20 odoo.modules.loading: loading 1 modules...";

fn one(text: &str) -> Entry {
    let mut entries = parse_all(text);
    assert_eq!(entries.len(), 1, "{entries:#?}");
    entries.remove(0)
}

fn header(entry: &Entry) -> &Header {
    entry.header.as_ref().expect("entry has a header")
}

#[test]
fn parses_odoo_19_header_and_drops_empty_perf_info() {
    let entry = one(LINE_19);
    let h = header(&entry);
    assert_eq!(h.timestamp.to_string(), "2026-11-22 06:27:39,726");
    assert_eq!(h.pid, 6340);
    assert_eq!(h.level, Level::Info);
    assert_eq!(h.db, None);
    assert_eq!(h.logger, "odoo");
    assert_eq!(entry.message, "Odoo version 19.0");
    assert!(!entry.has_traceback() && !entry.truncated);
}

#[test]
fn parses_odoo_20_header_with_database() {
    let entry = one(LINE_20);
    let h = header(&entry);
    assert_eq!(h.db.as_deref(), Some("ohs20"));
    assert_eq!(h.logger, "odoo.modules.loading");
    assert_eq!(entry.message, "loading 1 modules...");
}

#[test]
fn parses_every_level() {
    for (name, level) in [
        ("DEBUG", Level::Debug),
        ("INFO", Level::Info),
        ("RUNBOT", Level::Runbot),
        ("WARNING", Level::Warning),
        ("ERROR", Level::Error),
        ("CRITICAL", Level::Critical),
    ] {
        let entry = one(&format!("2026-01-02 03:04:05,006 1 {name} db x.y: m"));
        assert_eq!(entry.level(), Some(level));
        assert_eq!(level.as_str(), name);
        assert_eq!(Level::from_name(name), Some(level));
    }
    assert!(Level::Debug < Level::Info && Level::Info < Level::Runbot);
    assert!(Level::Runbot < Level::Warning && Level::Error < Level::Critical);
}

#[test]
fn keeps_empty_messages_and_colons_inside_messages() {
    let empty = one("2026-01-02 03:04:05,006 1 INFO db odoo.x:  ");
    assert_eq!(empty.message, "");
    assert_eq!(header(&empty).logger, "odoo.x");

    let bare = one("2026-01-02 03:04:05,006 1 INFO db odoo.x:");
    assert_eq!(bare.message, "");

    let colon = one("2026-01-02 03:04:05,006 1 INFO db odoo.x: a: b: c");
    assert_eq!(colon.message, "a: b: c");
}

#[test]
fn malformed_headers_are_continuation_lines() {
    let bad = [
        "2026-13-02 03:04:05,006 1 INFO db x: month 13",
        "2026-01-32 03:04:05,006 1 INFO db x: day 32",
        "2026-01-02 24:04:05,006 1 INFO db x: hour 24",
        "2026-01-02 03:04:05.006 1 INFO db x: dot not comma",
        "2026-01-02 03:04:05,006 1x INFO db x: pid",
        "2026-01-02 03:04:05,006 99999999999 INFO db x: pid overflow",
        "2026-01-02 03:04:05,006 1 NOTICE db x: unknown level",
        "2026-01-02 03:04:05,006 1 info db x: lower-case level",
        "2026-01-02 03:04:05,006 1 INFO db no colon",
        "2026-01-02 03:04:05,006 1 INFO  x: empty db",
        "2026-01-02 03:04:05,006 1 INFO db : empty logger",
        "2026-01-02 03:04:05,006  1 INFO db x: double space",
        " 2026-01-02 03:04:05,006 1 INFO db x: leading space",
        "2026-01-02 03:04:05,006",
    ];
    for line in bad {
        let entry = one(&format!("{LINE_20}\n{line}"));
        assert_eq!(
            entry.message.lines().nth(1),
            Some(line.trim_end()),
            "{line}"
        );
    }
}

#[test]
fn groups_continuation_lines_and_keeps_indentation() {
    let text = format!("{LINE_20}\nfirst\n\tsecond, tab-indented\n  third\n\n\n{LINE_19}\nafter");
    let entries = parse_all(&text);
    assert_eq!(entries.len(), 2);
    assert_eq!(
        entries[0].message,
        "loading 1 modules...\nfirst\n\tsecond, tab-indented\n  third"
    );
    assert_eq!(entries[1].message, "Odoo version 19.0\nafter");
}

#[test]
fn text_before_the_first_record_is_its_own_entry() {
    let text = format!(
        "\n\nTraceback (most recent call last):\n  File \"/x.py\", line 3, in <module>\nImportError: boom\n{LINE_20}"
    );
    let entries = parse_all(&text);
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].header, None);
    assert_eq!(entries[0].level(), None);
    assert_eq!(entries[0].summary(), "Traceback (most recent call last):");
    assert_eq!(entries[0].exceptions, ["ImportError: boom"]);
    assert_eq!(entries[1].level(), Some(Level::Info));
}

#[test]
fn blank_input_has_no_entries() {
    assert!(parse_all("").is_empty());
    assert!(parse_all("\n \n\t\n").is_empty());
    let mut parser = Parser::new();
    assert_eq!(parser.push("   "), None);
    assert!(!parser.has_pending());
    assert_eq!(parser.flush(), None);
}

#[test]
fn push_returns_the_previous_entry_and_flush_the_last() {
    let mut parser = Parser::new();
    assert_eq!(parser.push(&format!("{LINE_20}\n")), None);
    assert_eq!(parser.push("continued\r\n"), None);
    assert!(parser.has_pending());
    let first = parser.push(LINE_19).expect("first entry completes");
    assert_eq!(first.message, "loading 1 modules...\ncontinued");
    let last = parser.flush().expect("last entry");
    assert_eq!(last.message, "Odoo version 19.0");
    assert!(!parser.has_pending());
    assert_eq!(parser.flush(), None);
}

#[test]
fn strips_ansi_colors() {
    // Odoo 19 ColoredFormatter: level wrapped in two SGR sequences.
    let line = "2026-11-22 06:27:39,726 6340 \x1b[1;32m\x1b[1;49mINFO\x1b[0m ohs19 werkzeug: \"GET / HTTP/1.1\" 200 - \x1b[1;31m\x1b[1;49m12\x1b[0m 0.004 0.010";
    let entry = one(line);
    assert_eq!(entry.level(), Some(Level::Info));
    assert_eq!(entry.message, "\"GET / HTTP/1.1\" 200 - 12 0.004 0.010");

    // Odoo 20 colors the pid with a true-color sequence.
    let line = "2026-11-22 06:27:39,726 \x1b[38;2;10;20;30m140001\x1b[0m WARNING ? odoo: x";
    assert_eq!(header(&one(line)).pid, 140_001);

    let osc = one(&format!(
        "{LINE_20}\nlink \x1b]8;;http://x\x07text\x1b]8;;\x1b\\ end \x1bc"
    ));
    assert_eq!(osc.message.lines().nth(1), Some("link text end"));
}

#[test]
fn extracts_frame_locations() {
    let entry = one(&format!(
        "{LINE_20}\nTraceback (most recent call last):\n  File \"/srv/odoo/a.py\", line 12, in run\n    x()\n  File \"/srv/my addons/b.py\", line 7, in <module>\n    ^^^\n  File \"<string>\", line 1\nValueError: bad"
    ));
    let got: Vec<_> = entry
        .locations
        .iter()
        .map(|l| (l.path.as_str(), l.line, l.function.as_deref()))
        .collect();
    assert_eq!(
        got,
        [
            ("/srv/odoo/a.py", 12, Some("run")),
            ("/srv/my addons/b.py", 7, Some("<module>")),
            ("<string>", 1, None),
        ]
    );
    assert_eq!(entry.exceptions, ["ValueError: bad"]);
}

#[test]
fn rejects_malformed_frames() {
    for line in [
        "  File \"/a.py\", line , in f",
        "  File \"/a.py\", line 0, in f",
        "  File \"/a.py\", line 9x, in f",
        "  File \"/a.py\", line 99999999999, in f",
        "  File \"\", line 3, in f",
        "  File /a.py, line 3, in f",
    ] {
        let entry = one(&format!("{LINE_20}\n{line}"));
        assert!(entry.locations.is_empty(), "{line}");
    }
}

#[test]
fn extracts_xml_parse_location() {
    let entry = one(&format!(
        "{LINE_20}\nTraceback (most recent call last):\n  File \"/o/convert.py\", line 641, in _tag_root\nodoo.tools.convert.ParseError: while parsing /home/dev/addons/m1/views/v.xml:27\nError while validating view"
    ));
    let last = entry.locations.last().expect("xml location");
    assert_eq!(last.path, "/home/dev/addons/m1/views/v.xml");
    assert_eq!(last.line, 27);
    assert_eq!(last.function, None);
    assert_eq!(
        entry.exceptions,
        ["odoo.tools.convert.ParseError: while parsing /home/dev/addons/m1/views/v.xml:27"]
    );
}

#[test]
fn records_every_exception_of_a_chained_traceback() {
    let text = format!(
        "{LINE_20}
Traceback (most recent call last):
  File \"/a.py\", line 1, in f
ModuleNotFoundError: No module named 'ldap'

During handling of the above exception, another exception occurred:

Traceback (most recent call last):
  File \"/b.py\", line 2, in g
odoo.exceptions.UserError: cannot load"
    );
    let entry = one(&text);
    assert_eq!(
        entry.exceptions,
        [
            "ModuleNotFoundError: No module named 'ldap'",
            "odoo.exceptions.UserError: cannot load"
        ]
    );
    assert_eq!(entry.locations.len(), 2);
}

#[test]
fn caps_entry_lines_but_keeps_the_exception() {
    let mut text = format!("{LINE_20}\nTraceback (most recent call last):\n");
    for i in 0..MAX_ENTRY_LINES + 50 {
        text.push_str(&format!("  File \"/r.py\", line {}, in f\n", i + 1));
    }
    text.push_str("RecursionError: maximum recursion depth exceeded\n");
    let entry = one(&text);
    assert!(entry.truncated);
    assert_eq!(entry.message.lines().count(), MAX_ENTRY_LINES);
    assert_eq!(entry.locations.len(), MAX_LOCATIONS);
    assert_eq!(
        entry.exceptions,
        ["RecursionError: maximum recursion depth exceeded"]
    );
}

#[test]
fn caps_entry_bytes() {
    let long = "x".repeat(MAX_LINE_BYTES);
    let mut text = LINE_20.to_owned();
    for _ in 0..(MAX_ENTRY_BYTES / MAX_LINE_BYTES + 2) {
        text.push('\n');
        text.push_str(&long);
    }
    let entry = one(&text);
    assert!(entry.truncated);
    assert!(entry.message.len() <= MAX_ENTRY_BYTES);
}

#[test]
fn cuts_overlong_lines_on_a_char_boundary() {
    let line = format!("{LINE_20} {}", "é".repeat(MAX_LINE_BYTES));
    let entry = one(&line);
    assert!(entry.message.len() < MAX_LINE_BYTES);
    assert!(entry.message.ends_with('é'));
}

#[test]
fn caps_exception_count_and_length() {
    let mut text = LINE_20.to_owned();
    for _ in 0..MAX_EXCEPTIONS + 5 {
        text.push_str("\nTraceback (most recent call last):\n  File \"/a.py\", line 1, in f\n");
        text.push_str(&format!("E: {}", "y".repeat(5000)));
    }
    let entry = one(&text);
    assert_eq!(entry.exceptions.len(), MAX_EXCEPTIONS);
    assert!(
        entry
            .exceptions
            .iter()
            .all(|e| e.len() == MAX_EXCEPTION_BYTES)
    );
}

#[test]
fn timestamp_formats() {
    let ts = Timestamp::parse("2026-02-03 04:05:06,007").expect("valid");
    assert_eq!(ts.to_string(), "2026-02-03 04:05:06,007");
    assert_eq!(ts.to_rfc3339(), "2026-02-03T04:05:06.007Z");
    assert!(ts < Timestamp::parse("2026-02-03 04:05:06,008").expect("valid"));
    assert_eq!(Timestamp::parse("2026-02-03 04:05:06,0070"), None);
    assert_eq!(Timestamp::parse("2026-02-03 04:05:60,000"), None);
    assert_eq!(Timestamp::parse("2026-00-03 04:05:06,000"), None);
}

#[test]
fn serializes_flat_json() {
    let entry = one(&format!(
        "{LINE_20}\nTraceback (most recent call last):\n  File \"/a.py\", line 4, in f\nKeyError: 'x'"
    ));
    let json = serde_json::to_value(&entry).expect("serializable");
    assert_eq!(json["timestamp"], "2026-11-22T06:28:04.911Z");
    assert_eq!(json["pid"], 6418);
    assert_eq!(json["level"], "info");
    assert_eq!(json["db"], "ohs20");
    assert_eq!(json["logger"], "odoo.modules.loading");
    assert_eq!(json["locations"][0]["line"], 4);
    assert_eq!(json["exceptions"][0], "KeyError: 'x'");
    assert_eq!(json["truncated"], false);

    let raw = serde_json::to_value(one("plain text")).expect("serializable");
    assert!(raw.get("level").is_none());
    assert_eq!(raw["message"], "plain text");
}

// ---- captured Odoo 19 / 20 output -------------------------------------------

struct Sample {
    name: &'static str,
    text: &'static str,
    /// Lines starting a record, counted with `grep -cE` on the fixture.
    records: usize,
    levels: &'static [(Level, usize)],
    /// The record holding the traceback: logger, level, frames, exception.
    traceback: Option<(&'static str, Level, usize, &'static str)>,
}

macro_rules! fixture {
    ($name:literal) => {
        ($name, include_str!(concat!("fixtures/", $name, ".log")))
    };
}

fn samples() -> Vec<Sample> {
    use Level::{Critical, Error, Info, Warning};
    let s = |(name, text): (&'static str, &'static str), records, levels, traceback| Sample {
        name,
        text,
        records,
        levels,
        traceback,
    };
    let import = "ModuleNotFoundError: No module named 'ldap_missing_pkg'";
    let view = "odoo.tools.convert.ParseError: while parsing /home/dev/work/addons/brk_view/views/bad.xml:2";
    let serve = "AttributeError: The method 'res.partner.nofn' does not exist";
    let nodb = "psycopg2.OperationalError: connection to server at \"127.0.0.1\", port 25499 failed: Connection refused";
    vec![
        s(
            fixture!("odoo-19-init"),
            191,
            &[(Info, 185), (Warning, 6)],
            None,
        ),
        s(
            fixture!("odoo-20-init"),
            216,
            &[(Info, 209), (Warning, 7)],
            None,
        ),
        s(
            fixture!("odoo-19-import"),
            21,
            &[(Info, 13), (Warning, 5), (Error, 1), (Critical, 2)],
            Some(("odoo.service.server", Critical, 8, import)),
        ),
        s(
            fixture!("odoo-20-import"),
            23,
            &[(Info, 14), (Warning, 6), (Error, 1), (Critical, 2)],
            Some(("odoo.service.server", Critical, 8, import)),
        ),
        s(
            fixture!("odoo-19-view"),
            21,
            &[(Info, 14), (Warning, 5), (Error, 1), (Critical, 1)],
            Some(("odoo.service.server", Critical, 10, view)),
        ),
        s(
            fixture!("odoo-20-view"),
            23,
            &[(Info, 15), (Warning, 6), (Error, 1), (Critical, 1)],
            Some(("odoo.service.server", Critical, 10, view)),
        ),
        s(
            fixture!("odoo-19-serve"),
            20,
            &[(Info, 18), (Warning, 1), (Error, 1)],
            Some(("odoo.http", Error, 15, serve)),
        ),
        s(
            fixture!("odoo-20-serve"),
            23,
            &[(Info, 20), (Warning, 2), (Error, 1)],
            Some(("odoo.http", Error, 15, serve)),
        ),
        s(
            fixture!("odoo-19-nodb"),
            4,
            &[(Info, 4)],
            Some(("odoo.sql_db", Info, 10, nodb)),
        ),
        s(
            fixture!("odoo-20-nodb"),
            4,
            &[(Info, 4)],
            Some(("odoo.sql_db", Info, 9, nodb)),
        ),
    ]
}

#[test]
fn parses_captured_odoo_logs() {
    for sample in samples() {
        let name = sample.name;
        let entries = parse_all(sample.text);
        assert_eq!(entries.len(), sample.records, "{name}: record count");
        assert!(entries.iter().all(|e| e.header.is_some()), "{name}");
        assert!(entries.iter().all(|e| !e.truncated), "{name}");
        assert!(
            entries.iter().all(|e| e.message == e.message.trim_end()),
            "{name}: trailing whitespace"
        );

        let mut levels = BTreeMap::new();
        for entry in &entries {
            *levels.entry(entry.level().expect("header")).or_insert(0) += 1;
        }
        let expected: BTreeMap<_, _> = sample.levels.iter().copied().collect();
        assert_eq!(levels, expected, "{name}: levels");

        let version = if name.starts_with("odoo-19") {
            "19.0"
        } else {
            "20.0"
        };
        assert_eq!(entries[0].header.as_ref().map(|h| h.db.clone()), Some(None));
        assert!(
            entries
                .iter()
                .any(|e| e.message == format!("Odoo version {version}")),
            "{name}: version line"
        );

        let with_tb: Vec<_> = entries.iter().filter(|e| e.has_traceback()).collect();
        let Some((logger, level, frames, exception)) = sample.traceback else {
            assert!(with_tb.is_empty(), "{name}");
            assert!(entries.iter().all(|e| e.locations.is_empty()), "{name}");
            continue;
        };
        assert_eq!(with_tb.len(), 1, "{name}: one traceback");
        let tb = with_tb[0];
        let h = header(tb);
        assert_eq!((h.logger.as_str(), h.level), (logger, level), "{name}");
        assert_eq!(tb.exceptions, [exception], "{name}");
        let py_frames: Vec<_> = tb
            .locations
            .iter()
            .filter(|l| l.function.is_some())
            .collect();
        assert_eq!(py_frames.len(), frames, "{name}: frames");
        assert!(
            tb.locations
                .iter()
                .all(|l| l.path.starts_with("/home/dev/")),
            "{name}: paths"
        );
        assert!(tb.message.contains("Traceback (most recent call last):"));
    }
}

#[test]
fn captured_tracebacks_point_at_the_custom_module() {
    let by_name: BTreeMap<_, _> = samples().into_iter().map(|s| (s.name, s.text)).collect();
    for version in ["19", "20"] {
        let import = parse_all(by_name[format!("odoo-{version}-import").as_str()]);
        let frame = import
            .iter()
            .flat_map(|e| &e.locations)
            .last()
            .expect("frames");
        assert_eq!(frame.path, "/home/dev/work/addons/brk_import/models.py");
        assert_eq!(
            (frame.line, frame.function.as_deref()),
            (1, Some("<module>"))
        );

        let view = parse_all(by_name[format!("odoo-{version}-view").as_str()]);
        let xml = view
            .iter()
            .flat_map(|e| &e.locations)
            .find(|l| l.path.ends_with(".xml"))
            .expect("xml location");
        assert_eq!(xml.path, "/home/dev/work/addons/brk_view/views/bad.xml");
        assert_eq!((xml.line, xml.function.as_deref()), (2, None));
    }
}

#[test]
fn line_by_line_matches_whole_text() {
    for sample in samples() {
        let mut parser = Parser::new();
        let mut entries = Vec::new();
        for line in sample.text.split_inclusive('\n') {
            entries.extend(parser.push(line));
        }
        entries.extend(parser.flush());
        assert_eq!(entries, parse_all(sample.text), "{}", sample.name);
    }
}

// ---- robustness ---------------------------------------------------------------

fn log_line() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(LINE_19.to_owned()),
        Just(LINE_20.to_owned()),
        Just(TRACEBACK_START.to_owned()),
        Just("  File \"/a.py\", line 3, in f".to_owned()),
        Just("ValueError: x".to_owned()),
        Just("ParseError: while parsing /a.xml:9".to_owned()),
        Just(String::new()),
        "2026-0[0-9]-[0-3][0-9] [0-2][0-9]:[0-6][0-9]:[0-6][0-9],[0-9]{3} [0-9]{1,6} (INFO|ERROR|X) [?a-z]{0,3} [a-z.]{0,5}:? ?.{0,20}",
        ".{0,40}",
        "[\\x1b\\[\\]0-9;m\\x07\\\\a-z ]{0,30}",
    ]
}

proptest! {
    #[test]
    fn never_panics_and_keeps_invariants(lines in prop::collection::vec(log_line(), 0..60)) {
        let text = lines.join("\n");
        let entries = parse_all(&text);
        prop_assert!(entries.len() <= lines.len());
        for entry in &entries {
            prop_assert!(!entry.message.contains('\x1b'));
            prop_assert_eq!(entry.message.trim_end(), entry.message.as_str());
            prop_assert!(entry.message.len() <= MAX_ENTRY_BYTES);
            prop_assert!(entry.locations.iter().all(|l| l.line > 0 && !l.path.is_empty()));
            prop_assert!(entry.header.is_some() || !entry.message.is_empty());
        }
        let headers = entries.iter().filter(|e| e.header.is_some()).count();
        let raw = entries.len() - headers;
        prop_assert!(raw <= 1, "only text before the first record lacks a header");
    }

    #[test]
    fn arbitrary_bytes_never_panic(text in "\\PC{0,400}") {
        let _ = parse_all(&text);
    }
}
