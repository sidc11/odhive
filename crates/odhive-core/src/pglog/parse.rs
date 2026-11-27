//! Parser for PostgreSQL's `stderr` log format.
//!
//! With the default `log_line_prefix` (`%m [%p] `, Debian/Ubuntu add
//! `%q%u@%d `) a record looks like:
//!
//! ```text
//! 2026-11-27 10:15:01.123 UTC [4321] odoo@demo ERROR:  relation "x" does not exist at character 15
//! 2026-11-27 10:15:01.123 UTC [4321] odoo@demo STATEMENT:  SELECT * FROM x
//! ```
//!
//! The prefix is read up to the process id; anything between it and the
//! severity (user, database, application, …) is searched for `user@db`.
//! `DETAIL`, `HINT`, `QUERY`, `CONTEXT`, `LOCATION` and `STATEMENT` lines of
//! the same process, and lines starting with a tab (the rest of a
//! multi-line statement), continue the record before them. Lines in no known
//! form become entries without a header.
//!
//! Records become [`Entry`] values with logger [`LOGGER`]; severities map
//! to [`Level`]s: `DEBUG1`–`5` debug; `INFO`, `NOTICE` and `LOG` info;
//! `WARNING` warning; `ERROR` error; `FATAL` and `PANIC` critical.
//! Timestamps are converted to UTC with [`Zones`].

use std::sync::LazyLock;

use regex::Regex;

use super::tz::Zones;
use crate::logs::parse::{Entry, Header, Level, MAX_ENTRY_BYTES, MAX_ENTRY_LINES, Timestamp};

/// Logger name of PostgreSQL entries.
pub const LOGGER: &str = "postgresql";

static PREFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(\d{4})-(\d{2})-(\d{2}) (\d{2}):(\d{2}):(\d{2})(?:\.(\d{1,6}))? ([A-Za-z]{1,6}|[+-]\d{2}(?::?\d{2})?) \[(\d{1,10})\](?:-\d+)? (.*?)\b(DEBUG[1-5]|INFO|NOTICE|WARNING|ERROR|LOG|FATAL|PANIC|DETAIL|HINT|QUERY|CONTEXT|LOCATION|STATEMENT):  ?(.*)$",
    )
    .expect("valid regex")
});

static USER_DB: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|\s)[^\s@]*@(\S*)").expect("valid regex"));

/// A parsed prefix.
struct Line<'a> {
    timestamp: Timestamp,
    pid: u32,
    db: Option<String>,
    severity: &'a str,
    text: &'a str,
}

fn level_of(severity: &str) -> Option<Level> {
    Some(match severity {
        s if s.starts_with("DEBUG") => Level::Debug,
        "INFO" | "NOTICE" | "LOG" => Level::Info,
        "WARNING" => Level::Warning,
        "ERROR" => Level::Error,
        "FATAL" | "PANIC" => Level::Critical,
        _ => return None,
    })
}

fn parse_line<'a>(line: &'a str, zones: &Zones) -> Option<Line<'a>> {
    let c = PREFIX.captures(line)?;
    let num = |i: usize| c.get(i).and_then(|m| m.as_str().parse::<i64>().ok());
    let (year, month, day) = (num(1)?, num(2)?, num(3)?);
    let (hour, minute, second) = (num(4)?, num(5)?, num(6)?);
    let millis = c.get(7).map_or(0, |m| {
        let digits = format!("{:0<3}", m.as_str());
        digits[..3].parse::<i64>().unwrap_or(0)
    });
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    // An unknown abbreviation is read as UTC.
    let offset = zones.offset(c.get(8)?.as_str()).unwrap_or(0);
    let local_ms =
        (days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second.min(59))
            * 1000
            + millis;
    let utc_ms = local_ms - i64::from(offset) * 1000;
    let timestamp = Timestamp::from_unix_millis(u64::try_from(utc_ms).unwrap_or(0));
    let pid = c.get(9)?.as_str().parse().ok()?;
    let middle = c.get(10).map_or("", |m| m.as_str());
    let db = USER_DB
        .captures(middle)
        .and_then(|m| m.get(1))
        .map(|m| m.as_str())
        .filter(|d| !d.is_empty() && *d != "[unknown]")
        .map(str::to_owned);
    Some(Line {
        timestamp,
        pid,
        db,
        severity: c.get(11)?.as_str(),
        text: c.get(12).map_or("", |m| m.as_str()),
    })
}

/// Days since 1970-01-01 of a proleptic Gregorian date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[derive(Debug)]
struct Builder {
    header: Option<Header>,
    message: String,
    lines: usize,
    truncated: bool,
}

impl Builder {
    fn new(header: Option<Header>, first: &str) -> Self {
        let mut b = Self {
            header,
            message: String::new(),
            lines: 0,
            truncated: false,
        };
        b.append(first);
        b
    }

    fn append(&mut self, line: &str) {
        if self.lines >= MAX_ENTRY_LINES || self.message.len() + line.len() + 1 > MAX_ENTRY_BYTES {
            self.truncated = true;
            return;
        }
        if self.lines > 0 {
            self.message.push('\n');
        }
        self.message.push_str(line);
        self.lines += 1;
    }

    fn finish(self) -> Entry {
        Entry {
            header: self.header,
            message: self.message.trim_end().to_owned(),
            locations: Vec::new(),
            exceptions: Vec::new(),
            truncated: self.truncated,
        }
    }
}

/// Turns PostgreSQL log lines into [`Entry`] values, one line at a time.
///
/// Like [`crate::logs::parse::Parser`], [`PgParser::push`] returns the
/// entry the line completed; call [`PgParser::flush`] when no line has come
/// for a while.
#[derive(Debug)]
pub struct PgParser {
    zones: Zones,
    current: Option<Builder>,
}

impl PgParser {
    /// A parser converting timestamps with `zones`.
    #[must_use]
    pub fn new(zones: Zones) -> Self {
        Self {
            zones,
            current: None,
        }
    }

    /// Feeds one line (without its `\n`).
    pub fn push(&mut self, line: &str) -> Option<Entry> {
        self.push_with(line, None)
    }

    /// Like [`PgParser::push`], with the header a line that starts an entry
    /// gets when it has none of its own, e.g. from the journal fields of a
    /// line PostgreSQL wrote without `log_line_prefix`, or of a line by
    /// systemd.
    pub fn push_with(&mut self, line: &str, fallback: Option<Header>) -> Option<Entry> {
        let line = line.trim_end_matches(['\r', '\n']);
        if let Some(parsed) = parse_line(line, &self.zones) {
            let continues = level_of(parsed.severity).is_none()
                && self
                    .current
                    .as_ref()
                    .and_then(|b| b.header.as_ref())
                    .is_some_and(|h| h.pid == parsed.pid);
            if continues {
                let text = format!("{}:  {}", parsed.severity, parsed.text);
                if let Some(b) = &mut self.current {
                    b.append(&text);
                }
                return None;
            }
            let (level, message) = match level_of(parsed.severity) {
                Some(level) => (level, parsed.text.to_owned()),
                // A detail line without its record.
                None => (
                    Level::Info,
                    format!("{}:  {}", parsed.severity, parsed.text),
                ),
            };
            let header = Header {
                timestamp: parsed.timestamp,
                pid: parsed.pid,
                level,
                db: parsed.db,
                logger: LOGGER.to_owned(),
            };
            let done = self.current.take().map(Builder::finish);
            self.current = Some(Builder::new(Some(header), &message));
            return done;
        }
        if line.starts_with('\t')
            && let Some(b) = &mut self.current
        {
            b.append(line);
            return None;
        }
        if line.trim().is_empty() {
            return None;
        }
        let done = self.current.take().map(Builder::finish);
        self.current = Some(Builder::new(fallback, line));
        done
    }

    /// Completes and returns the pending entry, if any.
    pub fn flush(&mut self) -> Option<Entry> {
        self.current.take().map(Builder::finish)
    }

    /// Whether an entry is waiting for more lines.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        self.current.is_some()
    }
}
