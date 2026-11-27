//! The PostgreSQL logs being followed (files and journal units), shared by
//! every request.
//!
//! A log is opened on its first request and then polled by each request
//! for it: no task runs in the background. Tails nobody asked for within
//! [`IDLE`] are closed, and at most [`MAX_TAILS`] stay open.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use odhive_core::logs::store::{Page, Record};
use odhive_core::pglog::{Journal, JournalError, JournalTail, PgLogTail, Zones};

/// A tail nobody read for this long is closed.
pub const IDLE: Duration = Duration::from_secs(300);

/// Most logs followed at once.
pub const MAX_TAILS: usize = 8;

/// A log being followed.
#[derive(Debug)]
pub(crate) enum Source {
    /// A log file.
    File(PgLogTail),
    /// A unit's lines in the journal.
    Journal(JournalTail),
}

/// Why a log could not be read.
#[derive(Debug)]
pub(crate) enum ReadError {
    File(io::Error),
    Journal(JournalError),
}

impl Source {
    /// Reads what was written since the last call.
    pub fn poll(&mut self) -> Result<(), ReadError> {
        match self {
            Self::File(t) => t.poll().map_err(ReadError::File),
            Self::Journal(t) => t.poll().map_err(ReadError::Journal),
        }
    }

    /// Up to `limit` entries from sequence number `from`.
    pub fn read_from(&self, from: u64, limit: usize) -> Page {
        match self {
            Self::File(t) => t.read_from(from, limit),
            Self::Journal(t) => t.read_from(from, limit),
        }
    }

    /// The last `n` entries.
    pub fn tail(&self, n: usize) -> Vec<Arc<Record>> {
        match self {
            Self::File(t) => t.tail(n),
            Self::Journal(t) => t.tail(n),
        }
    }

    /// The sequence number of the next entry.
    pub fn next_seq(&self) -> u64 {
        match self {
            Self::File(t) => t.next_seq(),
            Self::Journal(t) => t.next_seq(),
        }
    }
}

/// One followed log. `id` changes when the log is opened again, as its
/// sequence numbers then start over.
#[derive(Debug)]
pub(crate) struct Tail {
    pub id: u64,
    pub tail: Mutex<Source>,
}

/// What a tail follows.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Key {
    File(PathBuf),
    Journal(Journal),
}

#[derive(Debug)]
struct Slot {
    tail: Arc<Tail>,
    used: Instant,
}

/// The open tails, by log.
#[derive(Debug, Default)]
pub(crate) struct PgLogs {
    slots: Mutex<HashMap<Key, Slot>>,
    ids: AtomicU64,
}

impl PgLogs {
    /// The tail of file `path`, opened if needed. Blocks on file I/O.
    pub fn get(&self, path: &Path) -> io::Result<Arc<Tail>> {
        self.get_or_open(Key::File(path.to_path_buf()), || {
            PgLogTail::open(path, Zones::system()).map(Source::File)
        })
    }

    /// The tail of `journal`, opened if needed. Blocks on journalctl.
    pub fn get_journal(&self, journal: &Journal) -> Result<Arc<Tail>, JournalError> {
        self.get_or_open(Key::Journal(journal.clone()), || {
            JournalTail::open(journal.clone(), Zones::system()).map(Source::Journal)
        })
    }

    fn get_or_open<E>(
        &self,
        key: Key,
        open: impl FnOnce() -> Result<Source, E>,
    ) -> Result<Arc<Tail>, E> {
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        slots.retain(|_, s| now.duration_since(s.used) < IDLE);
        if let Some(slot) = slots.get_mut(&key) {
            slot.used = now;
            return Ok(slot.tail.clone());
        }
        let tail = Arc::new(Tail {
            id: self.ids.fetch_add(1, Ordering::Relaxed) + 1,
            tail: Mutex::new(open()?),
        });
        while slots.len() >= MAX_TAILS {
            let oldest = slots
                .iter()
                .min_by_key(|(_, s)| s.used)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(p) => slots.remove(&p),
                None => break,
            };
        }
        slots.insert(
            key,
            Slot {
                tail: tail.clone(),
                used: now,
            },
        );
        Ok(tail)
    }

    /// Files open now.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reuses_a_tail_and_keeps_at_most_max() {
        let dir = tempfile::tempdir().expect("tempdir");
        let logs = PgLogs::default();
        let first = dir.path().join("pg0.log");
        std::fs::write(&first, "").expect("write");
        let a = logs.get(&first).expect("open");
        let b = logs.get(&first).expect("open");
        assert_eq!(a.id, b.id);
        for i in 1..=MAX_TAILS {
            let p = dir.path().join(format!("pg{i}.log"));
            std::fs::write(&p, "").expect("write");
            logs.get(&p).expect("open");
        }
        assert_eq!(logs.len(), MAX_TAILS);
        // The least recently used one went; opening it again starts anew.
        assert_ne!(logs.get(&first).expect("open").id, a.id);
    }

    #[test]
    fn a_missing_file_is_an_error() {
        let logs = PgLogs::default();
        let err = logs
            .get(Path::new("/nonexistent/pg.log"))
            .expect_err("missing");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert_eq!(logs.len(), 0);
    }
}
