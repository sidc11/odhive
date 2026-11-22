//! Parser for Odoo's text log format.
//!
//! Odoo 19 and 20 write one record per log call:
//!
//! ```text
//! 2026-11-22 06:27:41,016 6340 CRITICAL demo odoo.service.server: Failed to initialize database `demo`.
//! Traceback (most recent call last):
//!   File "/opt/odoo/odoo/service/server.py", line 1591, in preload_registries
//! ...
//! ```
//!
//! that is `asctime pid levelname dbname logger: message`, where 19 adds a
//! `perf_info` field (often empty, so lines end in a space). The timestamp is
//! UTC. A message with newlines, such as a traceback, continues on the
//! following lines: every line that does not start a new record belongs to
//! the record before it. Lines before the first record (for example a Python
//! error printed before Odoo sets up logging) form entries without a header.
//!
//! ANSI color codes (Odoo colors its output on a terminal, or with
//! `ODOO_PY_COLORS`) are removed. Entries are capped at
//! [`MAX_ENTRY_LINES`] lines and [`MAX_ENTRY_BYTES`] bytes; tracebacks are
//! still analysed past the cap, so the exception line is never lost.
//!
//! Paths in [`Location`] come straight from the log and are not checked
//! against anything; whoever opens them must validate them first.

use std::borrow::Cow;
use std::fmt;

use serde::{Serialize, Serializer};

use super::MAX_LINE_BYTES;

#[cfg(test)]
mod tests;

/// Most lines kept in one entry's message; further lines are dropped.
pub const MAX_ENTRY_LINES: usize = 2_000;

/// Most bytes kept in one entry's message; further lines are dropped.
pub const MAX_ENTRY_BYTES: usize = 256 * 1024;

/// Most source locations recorded per entry.
pub const MAX_LOCATIONS: usize = 256;

/// Most exception lines recorded per entry (one per chained traceback).
pub const MAX_EXCEPTIONS: usize = 16;

/// Longest exception line kept, in bytes.
const MAX_EXCEPTION_BYTES: usize = 1024;

const TRACEBACK_START: &str = "Traceback (most recent call last):";

/// Severity of a record, in increasing order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    /// `DEBUG`.
    Debug,
    /// `INFO`.
    Info,
    /// `RUNBOT` (25), Odoo's own level between info and warning.
    Runbot,
    /// `WARNING`.
    Warning,
    /// `ERROR`.
    Error,
    /// `CRITICAL`.
    Critical,
}

impl Level {
    /// The level named `name` as Odoo prints it (`INFO`, `WARNING`, ...).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "DEBUG" => Self::Debug,
            "INFO" => Self::Info,
            "RUNBOT" => Self::Runbot,
            "WARNING" => Self::Warning,
            "ERROR" => Self::Error,
            "CRITICAL" => Self::Critical,
            _ => return None,
        })
    }

    /// The name Odoo prints for this level.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Runbot => "RUNBOT",
            Self::Warning => "WARNING",
            Self::Error => "ERROR",
            Self::Critical => "CRITICAL",
        }
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A record's time (UTC), to the millisecond.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp {
    /// Year.
    pub year: u16,
    /// Month, 1–12.
    pub month: u8,
    /// Day of the month, 1–31.
    pub day: u8,
    /// Hour, 0–23.
    pub hour: u8,
    /// Minute, 0–59.
    pub minute: u8,
    /// Second, 0–59.
    pub second: u8,
    /// Millisecond, 0–999.
    pub millis: u16,
}

impl Timestamp {
    /// Parses Odoo's `YYYY-MM-DD HH:MM:SS,mmm`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let b = text.as_bytes();
        if b.len() != 23 {
            return None;
        }
        let seps = [
            (4, b'-'),
            (7, b'-'),
            (10, b' '),
            (13, b':'),
            (16, b':'),
            (19, b','),
        ];
        if seps.iter().any(|&(i, c)| b[i] != c) {
            return None;
        }
        let num = |from: usize, to: usize| -> Option<u16> {
            b[from..to].iter().try_fold(0u16, |n, &d| {
                d.is_ascii_digit().then(|| n * 10 + u16::from(d - b'0'))
            })
        };
        let small = |from: usize| num(from, from + 2).and_then(|n| u8::try_from(n).ok());
        let ts = Self {
            year: num(0, 4)?,
            month: small(5)?,
            day: small(8)?,
            hour: small(11)?,
            minute: small(14)?,
            second: small(17)?,
            millis: num(20, 23)?,
        };
        let valid = (1..=12).contains(&ts.month)
            && (1..=31).contains(&ts.day)
            && ts.hour < 24
            && ts.minute < 60
            && ts.second < 60;
        valid.then_some(ts)
    }

    /// RFC 3339 form, e.g. `2026-11-22T06:27:41.016Z`.
    #[must_use]
    pub fn to_rfc3339(&self) -> String {
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
            self.year, self.month, self.day, self.hour, self.minute, self.second, self.millis
        )
    }
}

/// Odoo's own form, `YYYY-MM-DD HH:MM:SS,mmm`.
impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02},{:03}",
            self.year, self.month, self.day, self.hour, self.minute, self.second, self.millis
        )
    }
}

/// Serialized as RFC 3339.
impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_rfc3339())
    }
}

/// The fields in front of a record's message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Header {
    /// When the record was logged (UTC).
    pub timestamp: Timestamp,
    /// Process id (or, with colored Odoo 20 output, native thread id).
    pub pid: u32,
    /// Severity.
    pub level: Level,
    /// Database the record belongs to; `None` for Odoo's `?`.
    pub db: Option<String>,
    /// Python logger name, e.g. `odoo.modules.loading`.
    pub logger: String,
}

/// A source location named in an entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct Location {
    /// File path as logged (not validated).
    pub path: String,
    /// Line number, from 1.
    pub line: u32,
    /// Function the frame is in, when the log names it.
    pub function: Option<String>,
}

/// How [`Entry::header`] appears in an entry's JSON (flattened): every
/// field is present for an Odoo record and absent for unformatted text. For
/// the OpenAPI document only.
#[derive(utoipa::ToSchema)]
#[allow(dead_code)]
struct HeaderFields {
    /// When the record was logged (UTC, RFC 3339).
    #[schema(format = DateTime)]
    timestamp: Option<String>,
    /// Process id.
    pid: Option<u32>,
    /// Severity.
    level: Option<Level>,
    /// Database; `null` for Odoo's `?`.
    db: Option<String>,
    /// Python logger name.
    logger: Option<String>,
}

/// One log record, possibly spanning several lines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct Entry {
    /// The record header; `None` for text that is not in Odoo's format.
    #[serde(flatten)]
    #[schema(value_type = HeaderFields)]
    pub header: Option<Header>,
    /// The message: its first line and every continuation line, joined by
    /// `\n`, without trailing whitespace.
    pub message: String,
    /// `File "...", line N` frames and XML parse locations, in log order.
    pub locations: Vec<Location>,
    /// Exception lines ending each traceback, e.g.
    /// `ModuleNotFoundError: No module named 'ldap'`, in log order.
    pub exceptions: Vec<String>,
    /// Whether lines were dropped because the entry hit its size cap.
    pub truncated: bool,
}

impl Entry {
    /// The record's level; `None` for unformatted text.
    #[must_use]
    pub fn level(&self) -> Option<Level> {
        self.header.as_ref().map(|h| h.level)
    }

    /// The first line of the message.
    #[must_use]
    pub fn summary(&self) -> &str {
        self.message.lines().next().unwrap_or("")
    }

    /// Whether the entry contains a traceback.
    #[must_use]
    pub fn has_traceback(&self) -> bool {
        !self.exceptions.is_empty()
    }
}

/// Odoo's text form, `YYYY-MM-DD HH:MM:SS,mmm PID LEVEL DB LOGGER: message`
/// (just the message for unformatted text). Continuation lines follow on
/// their own lines; ANSI colors are not restored.
impl fmt::Display for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(h) = &self.header {
            write!(
                f,
                "{} {} {} {} {}: ",
                h.timestamp,
                h.pid,
                h.level,
                h.db.as_deref().unwrap_or("?"),
                h.logger
            )?;
        }
        f.write_str(&self.message)
    }
}

/// Turns log lines into [`Entry`] values, one line at a time.
///
/// An entry is complete once the next record starts, so [`Parser::push`]
/// returns the *previous* entry. Call [`Parser::flush`] at the end of input,
/// or when no line has arrived for a while, to get the last one. Use one
/// parser per output stream: interleaving two streams splits tracebacks.
#[derive(Debug, Default)]
pub struct Parser {
    current: Option<Builder>,
}

impl Parser {
    /// A parser with nothing pending.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one line (with or without its `\n`). Returns the entry this line
    /// completed, if any.
    pub fn push(&mut self, line: &str) -> Option<Entry> {
        let line = clean(line);
        if let Some((header, message)) = parse_header(&line) {
            let done = self.current.take().and_then(Builder::finish);
            self.current = Some(Builder::new(Some(header), message));
            return done;
        }
        match &mut self.current {
            Some(builder) => builder.append(&line),
            // Blank lines between unformatted output carry nothing.
            None if line.trim().is_empty() => {}
            None => self.current = Some(Builder::new(None, &line)),
        }
        None
    }

    /// Completes and returns the pending entry, if any.
    pub fn flush(&mut self) -> Option<Entry> {
        self.current.take().and_then(Builder::finish)
    }

    /// Whether an entry is waiting for more lines.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        self.current.is_some()
    }
}

/// Parses a whole log text.
#[must_use]
pub fn parse_all(text: &str) -> Vec<Entry> {
    let mut parser = Parser::new();
    let mut entries: Vec<Entry> = text.lines().filter_map(|l| parser.push(l)).collect();
    entries.extend(parser.flush());
    entries
}

#[derive(Debug)]
struct Builder {
    header: Option<Header>,
    message: String,
    lines: usize,
    truncated: bool,
    in_traceback: bool,
    locations: Vec<Location>,
    exceptions: Vec<String>,
}

impl Builder {
    fn new(header: Option<Header>, first: &str) -> Self {
        let mut builder = Self {
            header,
            message: String::new(),
            lines: 0,
            truncated: false,
            in_traceback: false,
            locations: Vec::new(),
            exceptions: Vec::new(),
        };
        builder.append(first);
        builder
    }

    fn append(&mut self, line: &str) {
        let line = line.trim_end();
        self.analyse(line);
        let extra = usize::from(self.lines > 0) + line.len();
        if self.lines >= MAX_ENTRY_LINES || self.message.len() + extra > MAX_ENTRY_BYTES {
            self.truncated = true;
            return;
        }
        if self.lines > 0 {
            self.message.push('\n');
        }
        self.message.push_str(line);
        self.lines += 1;
    }

    /// Tracks tracebacks: frames are indented, the exception line that ends
    /// one is the first line after them that is not.
    fn analyse(&mut self, line: &str) {
        if line == TRACEBACK_START {
            self.in_traceback = true;
            return;
        }
        if self.locations.len() < MAX_LOCATIONS
            && let Some(location) = frame_location(line).or_else(|| xml_location(line))
        {
            self.locations.push(location);
        }
        if self.in_traceback && !line.is_empty() && !line.starts_with([' ', '\t']) {
            self.in_traceback = false;
            if self.exceptions.len() < MAX_EXCEPTIONS {
                self.exceptions
                    .push(truncate(line, MAX_EXCEPTION_BYTES).to_owned());
            }
        }
    }

    fn finish(mut self) -> Option<Entry> {
        let kept = self.message.trim_end().len();
        self.message.truncate(kept);
        if self.header.is_none() && self.message.is_empty() {
            return None;
        }
        Some(Entry {
            header: self.header,
            message: self.message,
            locations: self.locations,
            exceptions: self.exceptions,
            truncated: self.truncated,
        })
    }
}

/// Splits `asctime pid level db logger: message` into header and message.
fn parse_header(line: &str) -> Option<(Header, &str)> {
    let timestamp = Timestamp::parse(line.get(..23)?)?;
    let rest = line.get(23..)?.strip_prefix(' ')?;
    let (pid, rest) = rest.split_once(' ')?;
    if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let pid = pid.parse().ok()?;
    let (level, rest) = rest.split_once(' ')?;
    let level = Level::from_name(level)?;
    let (db, rest) = rest.split_once(' ')?;
    if db.is_empty() {
        return None;
    }
    let (logger, message) = match rest.split_once(": ") {
        Some(parts) => parts,
        None => (rest.strip_suffix(':')?, ""),
    };
    if logger.is_empty() || logger.contains(char::is_whitespace) {
        return None;
    }
    let header = Header {
        timestamp,
        pid,
        level,
        db: (db != "?").then(|| db.to_owned()),
        logger: logger.to_owned(),
    };
    Some((header, message))
}

/// `File "<path>", line <n>[, in <function>]`, as Python prints a frame.
fn frame_location(line: &str) -> Option<Location> {
    let rest = line.trim_start().strip_prefix("File \"")?;
    let (path, rest) = rest.split_once("\", line ")?;
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    let (number, rest) = rest.split_at(digits);
    let function = match rest.strip_prefix(", in ") {
        Some(function) if !function.is_empty() => Some(function.to_owned()),
        Some(_) => None,
        None if rest.is_empty() => None,
        None => return None,
    };
    location(path, number, function)
}

/// Odoo's `ParseError: while parsing <path>:<line>` for data files.
fn xml_location(line: &str) -> Option<Location> {
    let (_, rest) = line.split_once("ParseError: while parsing ")?;
    let (path, number) = rest.trim_end().rsplit_once(':')?;
    location(path, number, None)
}

fn location(path: &str, number: &str, function: Option<String>) -> Option<Location> {
    if path.is_empty() || number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let line = number.parse().ok().filter(|&n| n > 0)?;
    Some(Location {
        path: path.to_owned(),
        line,
        function,
    })
}

/// Cuts `line` to [`MAX_LINE_BYTES`], drops a trailing newline and removes
/// ANSI escape sequences.
fn clean(line: &str) -> Cow<'_, str> {
    let line = truncate(line, MAX_LINE_BYTES);
    let line = line.strip_suffix('\n').unwrap_or(line);
    let line = line.strip_suffix('\r').unwrap_or(line);
    if !line.contains('\x1b') {
        return Cow::Borrowed(line);
    }
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // CSI: parameters and intermediates up to a final byte @–~.
            Some('[') => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            // OSC: up to BEL or ST (ESC \).
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\x07' {
                        break;
                    }
                    if c == '\x1b' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            // Any other escape is two characters long.
            _ => {}
        }
    }
    Cow::Owned(out)
}

/// The longest prefix of `s` that fits in `max` bytes.
fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}
