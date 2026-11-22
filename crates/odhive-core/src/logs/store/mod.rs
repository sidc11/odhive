//! In-memory log store: the recent parsed entries of one instance.
//!
//! [`LogBuffer`] is a ring buffer bounded by entry count *and* by an
//! estimate of the memory its entries use, so a burst of huge tracebacks
//! cannot grow it past its budget. Every entry gets a sequence number that
//! keeps increasing across evictions, so a reader can resume from where it
//! stopped and learn how many entries it missed ([`Page::skipped`]).
//!
//! [`InstanceLog`] feeds raw output lines through a [`Parser`] into a buffer
//! and appends them to the instance's rotating log file.

use std::collections::VecDeque;
use std::io;
use std::mem::size_of;
use std::path::Path;
use std::sync::Arc;

use serde::Serialize;

use super::parse::{Entry, Location, Parser};
use super::{LogFile, LogSettings, log_path};

/// Size limits of a [`LogBuffer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Most entries kept (at least 1).
    pub max_entries: usize,
    /// Memory budget for the kept entries, estimated by [`entry_size`]. The
    /// newest entry is always kept, even when it alone exceeds the budget.
    pub max_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        LogSettings::default().buffer_limits()
    }
}

/// An entry with its sequence number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Record {
    /// Position in the instance's log, from 1, never reused.
    pub seq: u64,
    /// The entry.
    #[serde(flatten)]
    pub entry: Entry,
}

/// Entries read from a [`LogBuffer`].
#[derive(Debug, Clone, Default)]
pub struct Page {
    /// Entries in log order.
    pub records: Vec<Arc<Record>>,
    /// Entries asked for that were already evicted.
    pub skipped: u64,
    /// Sequence number to pass to the next [`LogBuffer::read_from`].
    pub next: u64,
}

/// Estimated heap and inline memory an entry holds.
#[must_use]
pub fn entry_size(entry: &Entry) -> usize {
    let header = entry
        .header
        .as_ref()
        .map_or(0, |h| h.logger.len() + h.db.as_ref().map_or(0, String::len));
    let locations: usize = entry
        .locations
        .iter()
        .map(|l| size_of::<Location>() + l.path.len() + l.function.as_ref().map_or(0, String::len))
        .sum();
    let exceptions: usize = entry
        .exceptions
        .iter()
        .map(|e| size_of::<String>() + e.len())
        .sum();
    size_of::<Record>()
        + size_of::<Arc<Record>>()
        + header
        + entry.message.len()
        + locations
        + exceptions
}

/// A bounded ring buffer of [`Record`]s.
#[derive(Debug)]
pub struct LogBuffer {
    records: VecDeque<(Arc<Record>, usize)>,
    limits: Limits,
    bytes: usize,
    next_seq: u64,
}

impl Default for LogBuffer {
    fn default() -> Self {
        Self::new(Limits::default())
    }
}

impl LogBuffer {
    /// An empty buffer.
    #[must_use]
    pub fn new(limits: Limits) -> Self {
        Self {
            records: VecDeque::new(),
            limits: Limits {
                max_entries: limits.max_entries.max(1),
                max_bytes: limits.max_bytes,
            },
            bytes: 0,
            next_seq: 1,
        }
    }

    /// The limits.
    #[must_use]
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Adds `entry`, evicting the oldest entries while over a limit.
    pub fn push(&mut self, entry: Entry) -> Arc<Record> {
        let size = entry_size(&entry);
        let record = Arc::new(Record {
            seq: self.next_seq,
            entry,
        });
        self.next_seq += 1;
        self.records.push_back((Arc::clone(&record), size));
        self.bytes += size;
        while self.records.len() > self.limits.max_entries
            || (self.bytes > self.limits.max_bytes && self.records.len() > 1)
        {
            if let Some((_, s)) = self.records.pop_front() {
                self.bytes -= s;
            }
        }
        record
    }

    /// Number of entries held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether no entries are held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Estimated memory of the held entries.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Sequence number of the oldest held entry (equals [`Self::next_seq`]
    /// when empty).
    #[must_use]
    pub fn first_seq(&self) -> u64 {
        self.records.front().map_or(self.next_seq, |(r, _)| r.seq)
    }

    /// Sequence number the next entry will get.
    #[must_use]
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Up to `limit` entries starting at sequence number `from`. Use `0` or
    /// `1` to start at the oldest held entry, and [`Page::next`] to continue.
    #[must_use]
    pub fn read_from(&self, from: u64, limit: usize) -> Page {
        let first = self.first_seq();
        let from = from.max(1);
        let skipped = first.saturating_sub(from);
        let start = from.max(first);
        // Sequence numbers are contiguous, so the index follows directly.
        let index = usize::try_from(start - first).unwrap_or(usize::MAX);
        let records: Vec<Arc<Record>> = self
            .records
            .iter()
            .skip(index)
            .take(limit)
            .map(|(r, _)| Arc::clone(r))
            .collect();
        let next = records
            .last()
            .map_or(start.min(self.next_seq), |r| r.seq + 1);
        Page {
            records,
            skipped,
            next,
        }
    }

    /// The newest `n` entries, in log order.
    #[must_use]
    pub fn tail(&self, n: usize) -> Vec<Arc<Record>> {
        let skip = self.records.len().saturating_sub(n);
        self.records
            .iter()
            .skip(skip)
            .map(|(r, _)| Arc::clone(r))
            .collect()
    }

    /// Drops every entry. Sequence numbers keep counting.
    pub fn clear(&mut self) {
        self.records.clear();
        self.bytes = 0;
    }
}

/// What [`InstanceLog::push_line`] did.
#[derive(Debug, Default)]
pub struct Pushed {
    /// The entry the line completed, now in the buffer.
    pub record: Option<Arc<Record>>,
    /// Writing the log file failed; the file is closed and later lines only
    /// go to the buffer.
    pub file_error: Option<io::Error>,
}

/// The log of one running instance: parser, buffer and rotating file.
#[derive(Debug)]
pub struct InstanceLog {
    parser: Parser,
    buffer: LogBuffer,
    file: Option<LogFile>,
}

impl InstanceLog {
    /// A log that writes `<logs_dir>/<instance>.log`.
    ///
    /// # Errors
    /// An invalid instance id or an I/O error opening the file.
    pub fn open(logs_dir: &Path, instance: &str, settings: &LogSettings) -> io::Result<Self> {
        let file = LogFile::open(&log_path(logs_dir, instance)?, settings.rotation())?;
        Ok(Self {
            parser: Parser::new(),
            buffer: LogBuffer::new(settings.buffer_limits()),
            file: Some(file),
        })
    }

    /// A log kept only in memory.
    #[must_use]
    pub fn in_memory(settings: &LogSettings) -> Self {
        Self {
            parser: Parser::new(),
            buffer: LogBuffer::new(settings.buffer_limits()),
            file: None,
        }
    }

    /// Whether lines still go to a file.
    #[must_use]
    pub fn has_file(&self) -> bool {
        self.file.is_some()
    }

    /// Records one output line: appends it to the file and returns the entry
    /// it completed, if any (see [`Parser::push`]).
    pub fn push_line(&mut self, line: &str) -> Pushed {
        let file_error = self.write(line);
        let record = self.parser.push(line).map(|e| self.buffer.push(e));
        Pushed { record, file_error }
    }

    /// Records a line OdHive writes itself, e.g. a start marker. It ends the
    /// pending entry and is never merged into one.
    pub fn push_marker(&mut self, text: &str) -> (Vec<Arc<Record>>, Option<io::Error>) {
        let file_error = self.write(text);
        let mut done: Vec<Arc<Record>> = self.flush().into_iter().collect();
        let mut parser = Parser::new();
        let entries = parser.push(text).into_iter().chain(parser.flush());
        done.extend(entries.map(|e| self.buffer.push(e)));
        (done, file_error)
    }

    /// Completes the pending entry (end of output, or no line for a while).
    pub fn flush(&mut self) -> Option<Arc<Record>> {
        self.parser.flush().map(|e| self.buffer.push(e))
    }

    /// Whether an entry is waiting for more lines.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        self.parser.has_pending()
    }

    /// The buffer.
    #[must_use]
    pub fn buffer(&self) -> &LogBuffer {
        &self.buffer
    }

    /// Empties the buffer (the file is kept).
    pub fn clear(&mut self) {
        self.buffer.clear();
    }

    fn write(&mut self, line: &str) -> Option<io::Error> {
        let file = self.file.as_mut()?;
        let err = file.write_line(line).err()?;
        self.file = None;
        Some(err)
    }
}

#[cfg(test)]
mod tests;
