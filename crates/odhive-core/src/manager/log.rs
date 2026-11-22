//! The log of one managed instance: output lines go through its
//! [`InstanceLog`] (parser, buffer, file) and completed entries are
//! published on the bus.
//!
//! Lines are fed from the supervisor's callback, so none is lost to a slow
//! event subscriber. An entry is complete when the next one starts; the last
//! one is completed after [`IDLE_FLUSH`] without output, or when the process
//! ends.
//!
//! Entries naming a missing system tool (see [`crate::syslib::detect_log`])
//! are reported as [`Notice::SystemLibraryMissing`], once per library for
//! the life of the log.

use std::collections::BTreeSet;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use super::fix::Watcher;
use super::{Inner, Notice};
use crate::events::{LogEvent, Topic};
use crate::logs::LogSettings;
use crate::logs::store::{InstanceLog, Page, Record};
use crate::ports::PortOwner;
use crate::syslib::{self, Hint, Library};

/// Time without output after which the pending entry is completed.
pub const IDLE_FLUSH: Duration = Duration::from_millis(200);

/// Where an instance's log goes, as given by its plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LogTarget {
    pub dir: Option<PathBuf>,
    pub settings: LogSettings,
}

#[derive(Debug)]
struct State {
    log: InstanceLog,
    last_line: Instant,
    /// Processes started so far; entries belong to the latest.
    run: u64,
    /// Missing system libraries already reported.
    reported: BTreeSet<Library>,
}

/// Source of [`LogSlot::id`]s.
static NEXT_LOG_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub(super) struct LogSlot {
    /// Tells this log from an earlier or later one of the same instance
    /// (whose sequence numbers start over). Unique within the process.
    id: u64,
    owner: PortOwner,
    target: LogTarget,
    /// Receives the system-library notices.
    inner: Weak<Inner>,
    state: Mutex<State>,
    topic: Topic<LogEvent>,
    idle: Arc<Notify>,
    /// Sees every completed entry of the instance's output.
    watcher: Mutex<Option<Watcher>>,
}

impl Drop for LogSlot {
    fn drop(&mut self) {
        // Wakes the idle flusher so it notices the slot is gone.
        self.idle.notify_one();
    }
}

impl LogSlot {
    /// A log for `owner`. When its file cannot be opened the log is kept in
    /// memory only and the error is returned with it. Needs a Tokio runtime.
    pub fn new(
        owner: PortOwner,
        target: LogTarget,
        inner: Weak<Inner>,
        topic: Topic<LogEvent>,
    ) -> (Arc<Self>, Option<io::Error>) {
        let (log, error) = match &target.dir {
            Some(dir) => match InstanceLog::open(dir, &owner.instance, &target.settings) {
                Ok(log) => (log, None),
                Err(e) => (InstanceLog::in_memory(&target.settings), Some(e)),
            },
            None => (InstanceLog::in_memory(&target.settings), None),
        };
        let idle = Arc::new(Notify::new());
        let slot = Arc::new(Self {
            id: NEXT_LOG_ID.fetch_add(1, Ordering::Relaxed),
            owner,
            target,
            inner,
            state: Mutex::new(State {
                log,
                last_line: Instant::now(),
                run: 0,
                reported: BTreeSet::new(),
            }),
            topic,
            idle: idle.clone(),
            watcher: Mutex::new(None),
        });
        tokio::spawn(flush_when_idle(Arc::downgrade(&slot), idle));
        (slot, error)
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn target(&self) -> &LogTarget {
        &self.target
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // Every update leaves the log usable, so it is valid after a panic.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Who sees completed output entries from now on.
    pub fn set_watcher(&self, watcher: Option<Watcher>) {
        *self.watcher.lock().unwrap_or_else(PoisonError::into_inner) = watcher;
    }

    /// Hands an output entry to the watcher, then publishes it. Called with
    /// the state lock held, so events keep log order.
    fn output(&self, state: &mut State, record: Arc<Record>) {
        if let Some(w) = &*self.watcher.lock().unwrap_or_else(PoisonError::into_inner) {
            w.observe(&record.entry, state.run);
        }
        if let Some(library) = syslib::detect_log(&record.entry)
            && state.reported.insert(library)
            && let Some(inner) = self.inner.upgrade()
        {
            inner.notify(
                &self.owner,
                Notice::SystemLibraryMissing {
                    hint: Hint::for_host(library),
                },
            );
        }
        self.publish(record);
    }

    /// Publishes while the lock is held, so events keep log order.
    fn publish(&self, record: Arc<Record>) {
        self.topic.publish(LogEvent {
            owner: self.owner.clone(),
            record,
        });
    }

    /// One output line. Returns the file error that closed the log file.
    pub fn line(&self, line: &str) -> Option<io::Error> {
        let mut state = self.lock();
        let pushed = state.log.push_line(line);
        state.last_line = Instant::now();
        if let Some(record) = pushed.record {
            self.output(&mut state, record);
        }
        if state.log.has_pending() {
            self.idle.notify_one();
        }
        pushed.file_error
    }

    /// A new process was started: writes the start marker.
    pub fn started(&self, pid: u32) -> Option<io::Error> {
        self.write_marker(&format!("----- odhive: started pid {pid} -----"), true)
    }

    /// A line OdHive writes itself. Never handed to the watcher.
    pub fn marker(&self, text: &str) -> Option<io::Error> {
        self.write_marker(text, false)
    }

    fn write_marker(&self, text: &str, new_run: bool) -> Option<io::Error> {
        let mut state = self.lock();
        // The pending output entry is completed first; it belongs to the run.
        if let Some(record) = state.log.flush() {
            self.output(&mut state, record);
        }
        let (records, error) = state.log.push_marker(text);
        for record in records {
            self.publish(record);
        }
        if new_run {
            state.run += 1;
        }
        error
    }

    /// Completes the pending entry.
    pub fn flush(&self) {
        let mut state = self.lock();
        if let Some(record) = state.log.flush() {
            self.output(&mut state, record);
        }
    }

    /// Completes the pending entry if no line came for `idle`; otherwise
    /// returns how long to wait before checking again.
    fn flush_if_idle(&self, idle: Duration) -> Option<Duration> {
        let mut state = self.lock();
        if !state.log.has_pending() {
            return None;
        }
        let quiet = state.last_line.elapsed();
        if quiet < idle {
            return Some(idle - quiet);
        }
        if let Some(record) = state.log.flush() {
            self.output(&mut state, record);
        }
        None
    }

    pub fn read_from(&self, from: u64, limit: usize) -> Page {
        self.lock().log.buffer().read_from(from, limit)
    }

    pub fn tail(&self, n: usize) -> Vec<Arc<Record>> {
        self.lock().log.buffer().tail(n)
    }

    pub fn next_seq(&self) -> u64 {
        self.lock().log.buffer().next_seq()
    }
}

/// Completes a slot's pending entry once its output pauses. Ends when the
/// slot is dropped.
async fn flush_when_idle(slot: Weak<LogSlot>, idle: Arc<Notify>) {
    loop {
        idle.notified().await;
        let mut wait = IDLE_FLUSH;
        loop {
            tokio::time::sleep(wait).await;
            let Some(slot) = slot.upgrade() else {
                return;
            };
            match slot.flush_if_idle(IDLE_FLUSH) {
                Some(more) => wait = more,
                None => break,
            }
        }
        if slot.strong_count() == 0 {
            return;
        }
    }
}
