//! The PostgreSQL server log kept by journald (Arch, Fedora and other
//! systemd systems where the server logs to `stderr`).
//!
//! - [`detect_unit`] looks for the server's unit file
//!   (`postgresql.service`, or PGDG's `postgresql-<major>.service`) in the
//!   systemd unit folders; nothing is run.
//! - [`JournalTail`] runs `journalctl --unit <unit> --output=json`, without
//!   a shell and with an empty environment, first for the last lines and
//!   then for the lines after the last cursor seen. Each `MESSAGE` is
//!   parsed with [`PgParser`]; lines without PostgreSQL's prefix (systemd's
//!   own "Started …" lines) get their header from the journal fields.
//! - A user who may not read the system journal gets no entries and
//!   journalctl's "not seeing messages" hint; that is reported as
//!   [`JournalError::PermissionDenied`].

use std::ffi::OsString;
use std::fs;
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::Value;
use thiserror::Error;

use super::parse::{LOGGER, PgParser};
use super::tz::Zones;
use super::{BUFFER_BYTES, BUFFER_ENTRIES, INITIAL_LINES};
use crate::logs::parse::{Entry, Header, Level, Timestamp};
use crate::logs::store::{Limits, LogBuffer, Page, Record};

#[cfg(test)]
mod tests;

/// Most journal lines read by one poll; older ones of a burst are skipped.
pub const POLL_LINES: usize = 5_000;

/// Most bytes read from one journalctl run.
const MAX_OUTPUT: u64 = 32 * 1024 * 1024;

/// Most bytes kept of journalctl's `stderr`.
const MAX_STDERR: u64 = 64 * 1024;

/// Polls closer together than this reuse the last result.
pub const MIN_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Where journalctl usually is.
const JOURNALCTL: &[&str] = &["/usr/bin/journalctl", "/bin/journalctl"];

static UNIT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^postgresql(?:-\d{1,3})?\.service$").expect("valid regex"));

/// journalctl, when this system has it (Linux only).
#[must_use]
pub fn default_journalctl() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        return None;
    }
    JOURNALCTL
        .iter()
        .map(PathBuf::from)
        .find(|p| fs::metadata(p).is_ok_and(|m| m.is_file()))
}

/// The systemd folders holding unit files (Linux only).
#[must_use]
pub fn default_unit_dirs() -> Vec<PathBuf> {
    if cfg!(target_os = "macos") {
        return Vec::new();
    }
    [
        "/etc/systemd/system",
        "/usr/lib/systemd/system",
        "/lib/systemd/system",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect()
}

/// Whether `name` is a PostgreSQL server unit OdHive reads:
/// `postgresql.service` or `postgresql-<major>.service`.
#[must_use]
pub fn is_unit_name(name: &str) -> bool {
    UNIT.is_match(name)
}

/// The PostgreSQL server unit found in `dirs`: `postgresql.service` when
/// there, else the `postgresql-<major>.service` of the highest major.
/// Debian's `postgresql@.service` template is not taken: its servers log to
/// files, which [`super::detect`] finds.
#[must_use]
pub fn detect_unit(dirs: &[PathBuf]) -> Option<String> {
    let mut best: Option<(u32, String)> = None;
    for dir in dirs {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten().take(4096) {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !is_unit_name(name) {
                continue;
            }
            // Links are fine (a unit enabled under another name, or masked
            // to /dev/null: then it does not run, and has no new lines).
            let rank = name
                .strip_prefix("postgresql-")
                .and_then(|r| r.strip_suffix(".service"))
                .and_then(|n| n.parse::<u32>().ok())
                .unwrap_or(u32::MAX);
            if best.as_ref().is_none_or(|(r, _)| rank > *r) {
                best = Some((rank, name.to_owned()));
            }
        }
    }
    best.map(|(_, name)| name)
}

/// A journal to read.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Journal {
    /// The journalctl executable.
    pub journalctl: PathBuf,
    /// The unit whose lines are read; see [`is_unit_name`].
    pub unit: String,
}

/// Why the journal could not be read.
#[derive(Debug, Error)]
pub enum JournalError {
    /// The unit name is not one OdHive reads.
    #[error("{0} is not a PostgreSQL server unit")]
    BadUnit(String),
    /// This user may not read the system journal.
    #[error("your user may not read the system journal")]
    PermissionDenied,
    /// journalctl failed.
    #[error("journalctl failed: {0}")]
    Failed(String),
    /// journalctl could not be run.
    #[error("cannot run journalctl: {0}")]
    Io(#[from] io::Error),
}

/// What to tell a user who may not read the system journal.
#[must_use]
pub fn permission_hint() -> String {
    "Members of the systemd-journal group (also adm or wheel on many systems) may read the system journal. Add your user to it (sudo usermod -aG systemd-journal $USER, then log in again), or choose a log file you can read.".to_owned()
}

/// The `journalctl` arguments of one read: the last `lines` lines of
/// `unit`, only those after `cursor` when given.
#[must_use]
pub fn args(unit: &str, cursor: Option<&str>, lines: usize) -> Vec<OsString> {
    let mut out: Vec<OsString> = vec![
        format!("--unit={unit}").into(),
        "--output=json".into(),
        "--all".into(),
        "--no-pager".into(),
        format!("--lines={lines}").into(),
    ];
    if let Some(c) = cursor {
        out.push(format!("--after-cursor={c}").into());
    }
    out
}

/// One journal entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalLine {
    /// Its cursor.
    pub cursor: String,
    /// Its message (may hold several lines).
    pub message: String,
    /// The header of a line without PostgreSQL's prefix.
    pub header: Header,
}

/// A journal field that is text, or bytes (journald's form for text that
/// is not UTF-8).
fn field_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Array(items) => {
            let bytes: Option<Vec<u8>> = items
                .iter()
                .map(|b| b.as_u64().and_then(|n| u8::try_from(n).ok()))
                .collect();
            bytes.map(|b| String::from_utf8_lossy(&b).into_owned())
        }
        _ => None,
    }
}

fn level_of(priority: Option<&str>) -> Level {
    match priority.and_then(|p| p.parse::<u8>().ok()) {
        Some(0..=2) => Level::Critical,
        Some(3) => Level::Error,
        Some(4) => Level::Warning,
        Some(7) => Level::Debug,
        _ => Level::Info,
    }
}

/// Reads one line of `journalctl --output=json`. Lines that are not a
/// JSON object with a cursor give `None`.
#[must_use]
pub fn parse_json_line(line: &str) -> Option<JournalLine> {
    let v: Value = serde_json::from_str(line).ok()?;
    let obj = v.as_object()?;
    let text = |k: &str| obj.get(k).and_then(field_text);
    let cursor = text("__CURSOR")?;
    let message = text("MESSAGE").unwrap_or_default();
    let micros: u64 = text("__REALTIME_TIMESTAMP")
        .and_then(|t| t.parse().ok())
        .unwrap_or(0);
    let ident = text("SYSLOG_IDENTIFIER");
    let logger = match ident.as_deref() {
        None | Some("" | "postgres" | "postmaster") => LOGGER.to_owned(),
        Some(other) => other.to_owned(),
    };
    let header = Header {
        timestamp: Timestamp::from_unix_millis(micros / 1000),
        pid: text("_PID").and_then(|p| p.parse().ok()).unwrap_or(0),
        level: level_of(text("PRIORITY").as_deref()),
        db: None,
        logger,
    };
    Some(JournalLine {
        cursor,
        message,
        header,
    })
}

/// Whether journalctl's `stderr` says the system journal was not readable.
fn denied(stderr: &str) -> bool {
    stderr.contains("not seeing messages from other users and the system")
        || stderr.contains("insufficient permissions")
}

/// Runs journalctl; its `stdout` (capped) when it succeeds.
fn run(journal: &Journal, cursor: Option<&str>, lines: usize) -> Result<String, JournalError> {
    let mut child = Command::new(&journal.journalctl)
        .args(args(&journal.unit, cursor, lines))
        .env_clear()
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stderr = child.stderr.take();
    let errors = std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(e) = stderr {
            let mut capped = e.take(MAX_STDERR);
            let _ = capped.read_to_string(&mut text);
            // Drain the rest so journalctl never blocks on a full pipe.
            let _ = io::copy(&mut capped.into_inner(), &mut io::sink());
        }
        text
    });
    let mut out = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        stdout.take(MAX_OUTPUT).read_to_end(&mut out)?;
    }
    if out.len() as u64 >= MAX_OUTPUT {
        let _ = child.kill();
    }
    let status = child.wait()?;
    let stderr = errors.join().unwrap_or_default();
    if denied(&stderr) {
        return Err(JournalError::PermissionDenied);
    }
    if !status.success() && out.is_empty() {
        let first = stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        return Err(JournalError::Failed(if first.is_empty() {
            status.to_string()
        } else {
            first.to_owned()
        }));
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// Reads and follows a unit's lines in the journal.
#[derive(Debug)]
pub struct JournalTail {
    journal: Journal,
    cursor: Option<String>,
    parser: PgParser,
    buffer: LogBuffer,
    initial: usize,
    polled: Option<Instant>,
    min_interval: Duration,
}

impl JournalTail {
    /// Starts with the unit's last [`INITIAL_LINES`] lines.
    ///
    /// # Errors
    /// [`JournalError`].
    pub fn open(journal: Journal, zones: Zones) -> Result<Self, JournalError> {
        Self::with_limits(
            journal,
            zones,
            INITIAL_LINES,
            Limits {
                max_entries: BUFFER_ENTRIES,
                max_bytes: BUFFER_BYTES,
            },
        )
    }

    /// [`JournalTail::open`] with other limits.
    ///
    /// # Errors
    /// [`JournalError`].
    pub fn with_limits(
        journal: Journal,
        zones: Zones,
        initial: usize,
        limits: Limits,
    ) -> Result<Self, JournalError> {
        if !is_unit_name(&journal.unit) {
            return Err(JournalError::BadUnit(journal.unit));
        }
        let mut tail = Self {
            journal,
            cursor: None,
            parser: PgParser::new(zones),
            buffer: LogBuffer::new(limits),
            initial,
            polled: None,
            min_interval: MIN_POLL_INTERVAL,
        };
        tail.read()?;
        if let Some(entry) = tail.parser.flush() {
            tail.buffer.push(entry);
        }
        Ok(tail)
    }

    /// Runs journalctl for what came since the last cursor; whether there
    /// was anything.
    fn read(&mut self) -> Result<bool, JournalError> {
        self.polled = Some(Instant::now());
        let lines = if self.cursor.is_some() {
            POLL_LINES
        } else {
            self.initial
        };
        let out = run(&self.journal, self.cursor.as_deref(), lines)?;
        let mut any = false;
        for line in out.lines() {
            let Some(j) = parse_json_line(line) else {
                continue;
            };
            any = true;
            for entry in feed(&mut self.parser, &j) {
                self.buffer.push(entry);
            }
            self.cursor = Some(j.cursor);
        }
        Ok(any)
    }

    /// Reads what the journal got since the last call, at most once per
    /// [`MIN_POLL_INTERVAL`]. An entry still waiting for more lines is
    /// completed when nothing new came.
    ///
    /// # Errors
    /// [`JournalError`].
    pub fn poll(&mut self) -> Result<(), JournalError> {
        if self.polled.is_some_and(|t| t.elapsed() < self.min_interval) {
            return Ok(());
        }
        if !self.read()?
            && let Some(entry) = self.parser.flush()
        {
            self.buffer.push(entry);
        }
        Ok(())
    }

    /// Up to `limit` entries from sequence number `from`.
    #[must_use]
    pub fn read_from(&self, from: u64, limit: usize) -> Page {
        self.buffer.read_from(from, limit)
    }

    /// The last `n` entries.
    #[must_use]
    pub fn tail(&self, n: usize) -> Vec<Arc<Record>> {
        self.buffer.tail(n)
    }

    /// The sequence number of the next entry.
    #[must_use]
    pub fn next_seq(&self) -> u64 {
        self.buffer.next_seq()
    }

    /// Sets how close together polls may run journalctl.
    pub fn set_min_interval(&mut self, interval: Duration) {
        self.min_interval = interval;
    }

    /// The unit read.
    #[must_use]
    pub fn unit(&self) -> &str {
        &self.journal.unit
    }
}

/// Feeds each line of a journal entry's message to `parser`; the entries
/// completed.
fn feed(parser: &mut PgParser, j: &JournalLine) -> Vec<Entry> {
    j.message
        .lines()
        .filter_map(|text| parser.push_with(text, Some(j.header.clone())))
        .collect()
}

/// The entries `out` (`journalctl --output=json`) gives.
#[cfg(test)]
pub(crate) fn parse_output(out: &str, zones: Zones) -> Vec<Entry> {
    let mut parser = PgParser::new(zones);
    let mut entries = Vec::new();
    for j in out.lines().filter_map(parse_json_line) {
        entries.extend(feed(&mut parser, &j));
    }
    entries.extend(parser.flush());
    entries
}
