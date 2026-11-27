//! The PostgreSQL log files being followed, shared by every request.
//!
//! A file is opened on its first request and then polled by each request
//! for it: no task runs in the background. Tails nobody asked for within
//! [`IDLE`] are closed, and at most [`MAX_TAILS`] stay open.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use odhive_core::pglog::{PgLogTail, Zones};

/// A tail nobody read for this long is closed.
pub const IDLE: Duration = Duration::from_secs(300);

/// Most files followed at once.
pub const MAX_TAILS: usize = 8;

/// One followed file. `id` changes when the file is opened again, as its
/// sequence numbers then start over.
#[derive(Debug)]
pub(crate) struct Tail {
    pub id: u64,
    pub tail: Mutex<PgLogTail>,
}

#[derive(Debug)]
struct Slot {
    tail: Arc<Tail>,
    used: Instant,
}

/// The open tails, by file.
#[derive(Debug, Default)]
pub(crate) struct PgLogs {
    slots: Mutex<HashMap<PathBuf, Slot>>,
    ids: AtomicU64,
}

impl PgLogs {
    /// The tail of `path`, opened if needed. Blocks on file I/O.
    pub fn get(&self, path: &Path) -> io::Result<Arc<Tail>> {
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        slots.retain(|_, s| now.duration_since(s.used) < IDLE);
        if let Some(slot) = slots.get_mut(path) {
            slot.used = now;
            return Ok(slot.tail.clone());
        }
        let tail = Arc::new(Tail {
            id: self.ids.fetch_add(1, Ordering::Relaxed) + 1,
            tail: Mutex::new(PgLogTail::open(path, Zones::system())?),
        });
        while slots.len() >= MAX_TAILS {
            let oldest = slots
                .iter()
                .min_by_key(|(_, s)| s.used)
                .map(|(p, _)| p.clone());
            match oldest {
                Some(p) => slots.remove(&p),
                None => break,
            };
        }
        slots.insert(
            path.to_path_buf(),
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
