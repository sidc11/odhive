//! The PostgreSQL server log, read-only.
//!
//! OdHive never writes to it or changes the server's logging; it only reads
//! a log file the user can already read:
//!
//! - [`detect`] lists the usual log files: on Linux
//!   `/var/log/postgresql/postgresql*.log` (Debian, Ubuntu) and
//!   `/var/lib/pgsql/data/log/*.log` (Fedora, RHEL); on macOS Homebrew's
//!   `{/opt/homebrew,/usr/local}/var/log/postgres*.log`.
//! - A project may name its own file instead (`postgres_log` in
//!   `odhive.toml`), checked by [`check_log_path`]: an absolute path
//!   without `.`/`..` parts, resolved through links, to a regular file
//!   named `*.log`.
//! - [`PgLogTail`] reads its last lines and follows it, across rotation,
//!   parsing each record with [`parse::PgParser`].
//!
//! A file that cannot be read for lack of permission is reported with a
//! hint ([`permission_hint`]) instead of an error.

use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use thiserror::Error;

use crate::logs::parse::Entry;
use crate::logs::store::{Limits, LogBuffer, Page, Record};
use crate::logs::{Follower, read_tail};

pub mod parse;
pub mod tz;

#[cfg(test)]
mod tests;

pub use parse::{LOGGER, PgParser};
pub use tz::Zones;

/// Lines read from the end of the file when a tail starts.
pub const INITIAL_LINES: usize = 2_000;

/// Entries a tail keeps in memory.
pub const BUFFER_ENTRIES: usize = 20_000;

/// Memory budget of a tail's entries.
pub const BUFFER_BYTES: usize = 32 * 1024 * 1024;

/// Most files [`detect`] lists.
const MAX_CANDIDATES: usize = 32;

/// A folder searched for log files, and the names taken from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchDir {
    /// The folder.
    pub dir: PathBuf,
    /// File names must start with this.
    pub prefix: &'static str,
}

impl SearchDir {
    fn new(dir: impl Into<PathBuf>, prefix: &'static str) -> Self {
        Self {
            dir: dir.into(),
            prefix,
        }
    }
}

/// The folders [`detect`] searches on this system.
#[must_use]
pub fn default_search_dirs() -> Vec<SearchDir> {
    if cfg!(target_os = "macos") {
        vec![
            SearchDir::new("/opt/homebrew/var/log", "postgres"),
            SearchDir::new("/usr/local/var/log", "postgres"),
        ]
    } else {
        vec![
            SearchDir::new("/var/log/postgresql", "postgresql"),
            SearchDir::new("/var/lib/pgsql/data/log", "postgresql"),
        ]
    }
}

/// A log file found by [`detect`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The file.
    pub path: PathBuf,
    /// When it was last written.
    pub modified: Option<SystemTime>,
}

/// Log files in `dirs`: regular files (not links) named `<prefix>*.log`,
/// the most recently written first. Folders that are missing or cannot be
/// listed are skipped.
#[must_use]
pub fn detect(dirs: &[SearchDir]) -> Vec<Candidate> {
    let mut found = Vec::new();
    for d in dirs {
        let Ok(entries) = fs::read_dir(&d.dir) else {
            continue;
        };
        for entry in entries.flatten().take(1024) {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !name.starts_with(d.prefix) || !name.ends_with(".log") {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.file_type().is_file() {
                continue;
            }
            found.push(Candidate {
                path: entry.path(),
                modified: meta.modified().ok(),
            });
        }
    }
    found.sort_by(|a, b| b.modified.cmp(&a.modified).then(a.path.cmp(&b.path)));
    found.truncate(MAX_CANDIDATES);
    found
}

/// Why a log path is refused.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum LogPathError {
    /// Not absolute.
    #[error("the PostgreSQL log path must be absolute")]
    Relative,
    /// Has `.` or `..` parts.
    #[error("the PostgreSQL log path must not contain . or .. parts")]
    Traversal,
    /// Does not name a `*.log` file.
    #[error("the PostgreSQL log must be a file named *.log")]
    NotLogName,
    /// Nothing there.
    #[error("no file at {0}")]
    Missing(PathBuf),
    /// A folder or other non-file.
    #[error("{0} is not a regular file")]
    NotFile(PathBuf),
    /// This user may not look at it.
    #[error("cannot read {0}: permission denied")]
    PermissionDenied(PathBuf),
    /// Cannot be looked at otherwise.
    #[error("cannot check {path}: {message}")]
    Io {
        /// The path.
        path: PathBuf,
        /// What failed.
        message: String,
    },
}

/// The checks on a configured path that need no file system: absolute,
/// no `.`/`..` parts, a `*.log` name.
///
/// # Errors
/// The first rule `path` breaks.
pub fn validate_log_path(path: &Path) -> Result<(), LogPathError> {
    if !path.is_absolute() {
        return Err(LogPathError::Relative);
    }
    let text = path.as_os_str().as_encoded_bytes();
    // `Path::components` drops `.` parts; look at the text too.
    let dotted = text
        .split(|&b| b == b'/')
        .any(|part| part == b"." || part == b"..");
    if dotted
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(LogPathError::Traversal);
    }
    if !is_log_name(path) {
        return Err(LogPathError::NotLogName);
    }
    Ok(())
}

fn is_log_name(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.len() > 4 && n.ends_with(".log"))
}

/// Checks a configured path ([`validate_log_path`]), resolves its links and
/// checks the result again: it must be a regular `*.log` file. Returns the
/// resolved path, which is what gets read.
///
/// # Errors
/// [`LogPathError`].
pub fn check_log_path(path: &Path) -> Result<PathBuf, LogPathError> {
    validate_log_path(path)?;
    let real = fs::canonicalize(path).map_err(|e| io_error(path, &e))?;
    validate_log_path(&real)?;
    let meta = fs::metadata(&real).map_err(|e| io_error(&real, &e))?;
    if !meta.is_file() {
        return Err(LogPathError::NotFile(real));
    }
    Ok(real)
}

fn io_error(path: &Path, e: &io::Error) -> LogPathError {
    if e.kind() == io::ErrorKind::NotFound {
        LogPathError::Missing(path.to_path_buf())
    } else if e.kind() == io::ErrorKind::PermissionDenied {
        LogPathError::PermissionDenied(path.to_path_buf())
    } else {
        LogPathError::Io {
            path: path.to_path_buf(),
            message: e.to_string(),
        }
    }
}

/// What to tell a user who may not read `path`: the group that may, when
/// the file has one, and how to join it.
#[must_use]
pub fn permission_hint(path: &Path) -> String {
    let group = fs::metadata(path)
        .ok()
        .filter(|m| m.mode() & 0o040 != 0)
        .and_then(|m| group_name(Path::new("/etc/group"), m.gid()));
    match group {
        Some(g) if cfg!(target_os = "macos") => format!(
            "{} can be read by the group {g}. Add your user to it, or choose a log file you can read.",
            path.display()
        ),
        Some(g) => format!(
            "{} can be read by the group {g}. Add your user to it (sudo usermod -aG {g} $USER, then log in again), or choose a log file you can read.",
            path.display()
        ),
        None => format!(
            "Your user may not read {}. Ask for read access, or choose a log file you can read.",
            path.display()
        ),
    }
}

/// The name of group `gid` in a `group(5)` file.
#[must_use]
pub fn group_name(group_file: &Path, gid: u32) -> Option<String> {
    let text = fs::read_to_string(group_file).ok()?;
    text.lines().find_map(|line| {
        let mut parts = line.split(':');
        let name = parts.next()?;
        let id: u32 = parts.nth(1)?.parse().ok()?;
        (id == gid && !name.is_empty()).then(|| name.to_owned())
    })
}

/// Reads and follows one PostgreSQL log file.
#[derive(Debug)]
pub struct PgLogTail {
    follower: Follower,
    parser: PgParser,
    buffer: LogBuffer,
}

impl PgLogTail {
    /// Starts with the last [`INITIAL_LINES`] lines of `path` (and of its
    /// rotated `.1` when the file is short), then follows it.
    ///
    /// # Errors
    /// I/O errors, e.g. [`io::ErrorKind::PermissionDenied`].
    pub fn open(path: &Path, zones: Zones) -> io::Result<Self> {
        Self::with_limits(
            path,
            zones,
            INITIAL_LINES,
            Limits {
                max_entries: BUFFER_ENTRIES,
                max_bytes: BUFFER_BYTES,
            },
        )
    }

    /// [`PgLogTail::open`] with other limits.
    ///
    /// # Errors
    /// I/O errors.
    pub fn with_limits(
        path: &Path,
        zones: Zones,
        initial: usize,
        limits: Limits,
    ) -> io::Result<Self> {
        // Fail early, and with the real reason, if it cannot be read.
        fs::File::open(path)?;
        let follower = Follower::from_end(path);
        let mut tail = Self {
            follower,
            parser: PgParser::new(zones),
            buffer: LogBuffer::new(limits),
        };
        for line in read_tail(path, initial)? {
            tail.push(&line);
        }
        if let Some(entry) = tail.parser.flush() {
            tail.buffer.push(entry);
        }
        Ok(tail)
    }

    fn push(&mut self, line: &str) {
        if let Some(entry) = self.parser.push(line) {
            self.store(entry);
        }
    }

    fn store(&mut self, entry: Entry) -> Arc<Record> {
        self.buffer.push(entry)
    }

    /// Reads what was written since the last call. An entry still waiting
    /// for more lines is completed when nothing new came.
    ///
    /// # Errors
    /// I/O errors other than the file being missing.
    pub fn poll(&mut self) -> io::Result<()> {
        let lines = self.follower.poll()?;
        if lines.is_empty() {
            if let Some(entry) = self.parser.flush() {
                self.store(entry);
            }
            return Ok(());
        }
        for line in &lines {
            self.push(line);
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

    /// The followed file.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.follower.path()
    }
}
