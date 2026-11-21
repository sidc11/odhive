//! The subset of Python's `configparser.RawConfigParser` that Odoo uses to read
//! `odoo.conf`, implemented to match its behavior:
//!
//! - `[section]` headers; `key = value` or `key: value` (first `=` or `:` wins)
//! - keys are trimmed and lowercased, values trimmed; no escapes, quotes or
//!   interpolation (`%`, `\` and quotes are literal)
//! - full-line comments start with `#` or `;` and are skipped, even inside a
//!   multi-line value; inline comments are not stripped
//! - lines indented deeper than their key continue its value (joined with `\n`);
//!   blank lines inside a value are kept, trailing ones dropped
//! - entries before any section, keys without a delimiter and duplicate sections or
//!   keys are errors (`strict=True`)
//! - `[DEFAULT]` entries apply to every section that does not set the key

use std::collections::BTreeMap;

use thiserror::Error;

/// Parsed file: section name → (key → value), in file order.
pub type Sections = Vec<(String, Vec<(String, String)>)>;

/// Entries while parsing: values are collected line by line.
type RawEntries = Vec<(String, Vec<String>)>;

const DEFAULT_SECTION: &str = "DEFAULT";

/// A syntax error, with its 1-based line number.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum IniError {
    /// An entry appears before any `[section]`.
    #[error("line {0}: entry before any [section] header")]
    MissingSection(usize),
    /// A line that is neither a header, an entry, a comment nor a continuation.
    #[error("line {0}: expected `key = value`")]
    NoDelimiter(usize),
    /// A key with nothing before the delimiter.
    #[error("line {0}: empty key")]
    EmptyKey(usize),
    /// The same section appears twice.
    #[error("line {line}: duplicate section [{name}]")]
    DuplicateSection {
        /// Line number.
        line: usize,
        /// Section name.
        name: String,
    },
    /// The same key appears twice in a section.
    #[error("line {line}: duplicate key {key:?} in [{section}]")]
    DuplicateKey {
        /// Line number.
        line: usize,
        /// Section name.
        section: String,
        /// Key.
        key: String,
    },
}

/// Parses `text`. `[DEFAULT]` is folded into the other sections and not returned.
///
/// # Errors
/// Returns the first syntax error.
pub fn parse(text: &str) -> Result<Sections, IniError> {
    // Values are collected as lines and joined at the end, like configparser.
    let mut sections: Vec<(String, RawEntries)> = Vec::new();
    let mut defaults: RawEntries = Vec::new();
    // Current section: index into `sections`, or None for [DEFAULT].
    let mut current: Option<Option<usize>> = None;
    // Entry a continuation line would extend, and the indent of its key line.
    let mut open: Option<(Option<usize>, usize, usize)> = None;

    for (i, raw) in text.lines().enumerate() {
        let line_no = i + 1;
        let trimmed = raw.trim();
        let indent = raw.len() - raw.trim_start().len();

        if trimmed.starts_with(['#', ';']) {
            continue;
        }
        if trimmed.is_empty() {
            // Blank lines inside a value are kept (trailing ones dropped at the end).
            if let Some((sec, idx, _)) = open {
                entry_lines(&mut sections, &mut defaults, sec, idx).push(String::new());
            }
            continue;
        }
        if let Some((sec, idx, key_indent)) = open
            && indent > key_indent
        {
            entry_lines(&mut sections, &mut defaults, sec, idx).push(trimmed.to_owned());
            continue;
        }
        if let Some(name) = trimmed.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
            if name == DEFAULT_SECTION {
                current = Some(None);
            } else {
                if sections.iter().any(|(n, _)| n == name) {
                    return Err(IniError::DuplicateSection {
                        line: line_no,
                        name: name.to_owned(),
                    });
                }
                sections.push((name.to_owned(), Vec::new()));
                current = Some(Some(sections.len() - 1));
            }
            open = None;
            continue;
        }
        let Some(sec) = current else {
            return Err(IniError::MissingSection(line_no));
        };
        let Some(pos) = trimmed.find(['=', ':']) else {
            return Err(IniError::NoDelimiter(line_no));
        };
        let key = trimmed[..pos].trim().to_lowercase();
        let value = trimmed[pos + 1..].trim().to_owned();
        if key.is_empty() {
            return Err(IniError::EmptyKey(line_no));
        }
        let (section_name, entries) = match sec {
            Some(s) => (sections[s].0.clone(), &mut sections[s].1),
            None => (DEFAULT_SECTION.to_owned(), &mut defaults),
        };
        if entries.iter().any(|(k, _)| *k == key) {
            return Err(IniError::DuplicateKey {
                line: line_no,
                section: section_name,
                key,
            });
        }
        entries.push((key, vec![value]));
        open = Some((sec, entries.len() - 1, indent));
    }

    let join = |lines: &[String]| lines.join("\n").trim_end().to_owned();
    let defaults: Vec<(String, String)> =
        defaults.iter().map(|(k, l)| (k.clone(), join(l))).collect();
    Ok(sections
        .into_iter()
        .map(|(name, entries)| {
            // Like `items()`: [DEFAULT] keys first (overridden in place), then the
            // section's own keys.
            let own: Vec<(String, String)> =
                entries.iter().map(|(k, l)| (k.clone(), join(l))).collect();
            let mut out: Vec<(String, String)> = defaults
                .iter()
                .map(|(k, v)| {
                    let v = own.iter().find(|(ok, _)| ok == k).map_or(v, |(_, ov)| ov);
                    (k.clone(), v.clone())
                })
                .collect();
            for (k, v) in own {
                if !out.iter().any(|(ek, _)| *ek == k) {
                    out.push((k, v));
                }
            }
            (name, out)
        })
        .collect())
}

fn entry_lines<'a>(
    sections: &'a mut [(String, RawEntries)],
    defaults: &'a mut RawEntries,
    sec: Option<usize>,
    idx: usize,
) -> &'a mut Vec<String> {
    match sec {
        Some(s) => &mut sections[s].1[idx].1,
        None => &mut defaults[idx].1,
    }
}

/// Writes sections the way `RawConfigParser.write` does: `[name]`, `key = value`
/// lines, a blank line after each section.
///
/// Callers must ensure keys and values contain no line breaks.
#[must_use]
pub fn write<'a, I, E>(sections: I) -> String
where
    I: IntoIterator<Item = (&'a str, E)>,
    E: IntoIterator<Item = (&'a str, &'a str)>,
{
    let mut out = String::new();
    for (name, entries) in sections {
        out.push('[');
        out.push_str(name);
        out.push_str("]\n");
        for (k, v) in entries {
            out.push_str(k);
            out.push_str(" = ");
            out.push_str(v);
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

/// Convenience: one section as a sorted map.
#[must_use]
pub fn section_map(sections: &Sections, name: &str) -> Option<BTreeMap<String, String>> {
    sections
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, e)| e.iter().cloned().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(text: &str) -> Vec<(String, String)> {
        let s = parse(text).expect("parse");
        s.into_iter()
            .find(|(n, _)| n == "options")
            .map(|(_, e)| e)
            .expect("options")
    }

    fn kv(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn parses_like_raw_config_parser() {
        let text = "\
# comment
; also a comment
[options]
admin_passwd = s3cr%t\\n'q'
DB_Host: localhost
dbfilter = ^%d$ # not a comment
empty =
url = http://x:8069/a=b
addons_path = /a,
    /b ,
\t/c
";
        assert_eq!(
            options(text),
            kv(&[
                ("admin_passwd", "s3cr%t\\n'q'"),
                ("db_host", "localhost"),
                ("dbfilter", "^%d$ # not a comment"),
                ("empty", ""),
                ("url", "http://x:8069/a=b"),
                ("addons_path", "/a,\n/b ,\n/c"),
            ])
        );
    }

    #[test]
    fn keeps_other_sections_and_applies_defaults() {
        let s =
            parse("[DEFAULT]\nshared = 1\n[options]\nshared = 2\n[queue_job]\nchannels = root:2\n")
                .expect("parse");
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].1, kv(&[("shared", "2")]));
        assert_eq!(s[1].0, "queue_job");
        assert_eq!(s[1].1, kv(&[("shared", "1"), ("channels", "root:2")]));
    }

    #[test]
    fn continuation_rules_follow_configparser() {
        // A blank line inside a value is kept; trailing blank lines are dropped.
        assert_eq!(
            parse("[options]\na = 1\n\n  more\n\nb = 2\n").expect("parse")[0].1,
            kv(&[("a", "1\n\nmore"), ("b", "2")])
        );
        // A comment inside a value is skipped; the value continues.
        assert_eq!(
            parse("[options]\na = 1\n# c\n  b = 2\n").expect("parse")[0].1,
            kv(&[("a", "1\nb = 2")])
        );
        // Only lines indented deeper than their key continue it.
        assert_eq!(
            parse("[options]\n  a = 1\n  b = 2\n    c\n").expect("parse")[0].1,
            kv(&[("a", "1"), ("b", "2\nc")])
        );
    }

    #[test]
    fn rejects_what_python_rejects() {
        assert_eq!(parse("a = 1\n"), Err(IniError::MissingSection(1)));
        assert_eq!(
            parse("[options]\njust a line\n"),
            Err(IniError::NoDelimiter(2))
        );
        assert_eq!(parse("[options]\n= x\n"), Err(IniError::EmptyKey(2)));
        assert!(matches!(
            parse("[options]\na = 1\nA = 2\n"),
            Err(IniError::DuplicateKey { line: 3, .. })
        ));
        assert!(matches!(
            parse("[options]\n[options]\n"),
            Err(IniError::DuplicateSection { line: 2, .. })
        ));
    }

    #[test]
    fn write_matches_python_output_and_round_trips() {
        let text = write([
            ("options", vec![("a", "1"), ("empty", "")]),
            ("extra", vec![("k", "v")]),
        ]);
        assert_eq!(text, "[options]\na = 1\nempty = \n\n[extra]\nk = v\n\n");
        let back = parse(&text).expect("parse");
        assert_eq!(back[0].1, kv(&[("a", "1"), ("empty", "")]));
        assert_eq!(section_map(&back, "extra").expect("extra")["k"], "v");
    }
}
