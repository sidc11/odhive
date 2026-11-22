//! Fuzzes the Odoo log parser: any input must parse without panicking,
//! stay within the documented caps, and give the same entries whether fed
//! line by line or as one text.
#![no_main]

use libfuzzer_sys::fuzz_target;
use odhive_core::logs::parse::{
    MAX_ENTRY_BYTES, MAX_ENTRY_LINES, MAX_EXCEPTIONS, MAX_LOCATIONS, Parser, parse_all,
};

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let entries = parse_all(&text);

    let mut parser = Parser::new();
    let mut streamed = Vec::new();
    for line in text.split_inclusive('\n') {
        streamed.extend(parser.push(line));
    }
    streamed.extend(parser.flush());
    assert!(!parser.has_pending());
    assert_eq!(entries, streamed, "streaming and whole-text parsing differ");

    assert!(entries.len() <= text.lines().count());
    for (i, entry) in entries.iter().enumerate() {
        assert!(
            entry.header.is_some() || i == 0,
            "only text before the first record lacks a header"
        );
        assert!(entry.header.is_some() || !entry.message.is_empty());
        assert!(!entry.message.contains('\x1b'), "escape sequence kept");
        assert_eq!(entry.message.trim_end(), entry.message);
        assert!(entry.message.len() <= MAX_ENTRY_BYTES);
        assert!(entry.message.lines().count() <= MAX_ENTRY_LINES);
        assert!(entry.locations.len() <= MAX_LOCATIONS);
        assert!(entry.exceptions.len() <= MAX_EXCEPTIONS);
        assert!(
            entry
                .locations
                .iter()
                .all(|l| l.line > 0 && !l.path.is_empty())
        );
        assert!(serde_json::to_string(entry).is_ok());
    }
});
