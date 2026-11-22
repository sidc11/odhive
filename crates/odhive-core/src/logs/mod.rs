//! Instance log files: `<project>/logs/<instance>.log`.
//!
//! The process that supervises an instance appends every output line to its
//! file; `odhive logs` reads and follows it from any other process. Files
//! rotate by size: `<instance>.log` → `.log.1` → … → `.log.<keep>`, the oldest
//! generation is deleted ([`Rotation`]).
//!
//! Files are created `0600` in a `0700` folder and are never opened through a
//! symlink.
//!
//! [`parse`] turns the lines into structured entries; [`store`] keeps the
//! recent ones in memory. Limits for both come from the project's `[logs]`
//! table ([`LogSettings`]).

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::fsutil::{DIR_MODE, FILE_MODE};
use crate::names;

pub mod parse;
pub mod store;

#[cfg(test)]
mod tests;

/// Name of the log folder inside a project.
pub const LOGS_DIR: &str = "logs";

const MIB: u64 = 1024 * 1024;

/// Size at which a log file rotates by default, in MiB.
pub const DEFAULT_FILE_MAX_MB: u32 = 16;

/// Size at which a log file rotates by default.
pub const DEFAULT_MAX_BYTES: u64 = DEFAULT_FILE_MAX_MB as u64 * MIB;

/// Rotated generations kept by default.
pub const DEFAULT_KEEP: u32 = 5;

/// Most rotated generations a project may keep.
pub const MAX_KEEP: u32 = 50;

/// Entries kept in memory per instance by default.
pub const DEFAULT_BUFFER_ENTRIES: usize = 50_000;

/// Memory budget of one instance's buffer by default, in MiB.
pub const DEFAULT_BUFFER_MB: u32 = 64;

/// Allowed range of `[logs] buffer_entries`.
pub const BUFFER_ENTRIES_RANGE: std::ops::RangeInclusive<usize> = 100..=1_000_000;
/// Allowed range of `[logs] buffer_mb`.
pub const BUFFER_MB_RANGE: std::ops::RangeInclusive<u32> = 1..=1024;
/// Allowed range of `[logs] file_max_mb`.
pub const FILE_MAX_MB_RANGE: std::ops::RangeInclusive<u32> = 1..=1024;
/// Allowed range of `[logs] file_keep`.
pub const FILE_KEEP_RANGE: std::ops::RangeInclusive<u32> = 1..=MAX_KEEP;

/// How a log file rotates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rotation {
    /// Size at which the file rotates (at least 1).
    pub max_bytes: u64,
    /// Rotated generations kept, `1..=MAX_KEEP`.
    pub keep: u32,
}

impl Default for Rotation {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_MAX_BYTES,
            keep: DEFAULT_KEEP,
        }
    }
}

/// The project's `[logs]` table.
///
/// ```toml
/// [logs]
/// buffer_entries = 50000   # entries kept in memory per instance
/// buffer_mb = 64           # memory budget per instance
/// file_max_mb = 16         # rotate the file at this size
/// file_keep = 5            # rotated files kept
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LogSettings {
    /// Entries kept in memory per instance.
    pub buffer_entries: usize,
    /// Memory budget of one instance's buffer, in MiB.
    pub buffer_mb: u32,
    /// Size at which a log file rotates, in MiB.
    pub file_max_mb: u32,
    /// Rotated log files kept per instance.
    pub file_keep: u32,
}

impl Default for LogSettings {
    fn default() -> Self {
        Self {
            buffer_entries: DEFAULT_BUFFER_ENTRIES,
            buffer_mb: DEFAULT_BUFFER_MB,
            file_max_mb: DEFAULT_FILE_MAX_MB,
            file_keep: DEFAULT_KEEP,
        }
    }
}

impl LogSettings {
    /// Whether every value is the default.
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Checks every value against its range.
    ///
    /// # Errors
    /// A message naming the first value out of range.
    pub fn validate(&self) -> Result<(), String> {
        fn check<T: PartialOrd + std::fmt::Display>(
            name: &str,
            v: T,
            range: &std::ops::RangeInclusive<T>,
        ) -> Result<(), String> {
            if range.contains(&v) {
                Ok(())
            } else {
                Err(format!(
                    "{name} must be between {} and {}, got {v}",
                    range.start(),
                    range.end()
                ))
            }
        }
        check("buffer_entries", self.buffer_entries, &BUFFER_ENTRIES_RANGE)?;
        check("buffer_mb", self.buffer_mb, &BUFFER_MB_RANGE)?;
        check("file_max_mb", self.file_max_mb, &FILE_MAX_MB_RANGE)?;
        check("file_keep", self.file_keep, &FILE_KEEP_RANGE)
    }

    /// File rotation.
    #[must_use]
    pub fn rotation(&self) -> Rotation {
        Rotation {
            max_bytes: u64::from(self.file_max_mb) * MIB,
            keep: self.file_keep.clamp(1, MAX_KEEP),
        }
    }

    /// Buffer limits.
    #[must_use]
    pub fn buffer_limits(&self) -> store::Limits {
        store::Limits {
            max_entries: self.buffer_entries,
            max_bytes: usize::try_from(u64::from(self.buffer_mb) * MIB).unwrap_or(usize::MAX),
        }
    }
}

/// Longest line kept by readers; longer lines are cut.
pub const MAX_LINE_BYTES: usize = 64 * 1024;

/// How far back [`read_tail`] looks, per file.
const TAIL_WINDOW: u64 = 8 * 1024 * 1024;

/// The log file of `instance` in `logs_dir`.
///
/// # Errors
/// [`io::ErrorKind::InvalidInput`] if `instance` is not a valid instance id.
pub fn log_path(logs_dir: &Path, instance: &str) -> io::Result<PathBuf> {
    names::validate_instance_id(instance)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
    Ok(logs_dir.join(format!("{instance}.log")))
}

/// Generation `n` of `path`: the file itself for 0, `<file>.<n>` after.
#[must_use]
pub fn generation_path(path: &Path, n: u32) -> PathBuf {
    if n == 0 {
        return path.to_path_buf();
    }
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{n}"));
    PathBuf::from(name)
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

fn rename_if_present(from: &Path, to: &Path) -> io::Result<()> {
    match fs::rename(from, to) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

fn nofollow() -> i32 {
    rustix::fs::OFlags::NOFOLLOW.bits().cast_signed()
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(FILE_MODE)
        .custom_flags(nofollow())
        .open(path)
}

fn open_read(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(nofollow())
        .open(path)
}

/// Appends lines to one instance log, rotating it when it grows too large.
#[derive(Debug)]
pub struct LogFile {
    path: PathBuf,
    file: File,
    len: u64,
    rotation: Rotation,
}

impl LogFile {
    /// Opens (or creates) `path` for appending. Its folder is created `0700`.
    ///
    /// # Errors
    /// I/O errors, including `path` being a symlink.
    pub fn open(path: &Path, rotation: Rotation) -> io::Result<Self> {
        if let Some(dir) = path.parent() {
            DirBuilder::new()
                .recursive(true)
                .mode(DIR_MODE)
                .create(dir)?;
        }
        let file = open_append(path)?;
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not a regular file", path.display()),
            ));
        }
        Ok(Self {
            path: path.to_path_buf(),
            file,
            len: meta.len(),
            rotation: Rotation {
                max_bytes: rotation.max_bytes.max(1),
                keep: rotation.keep.clamp(1, MAX_KEEP),
            },
        })
    }

    /// The file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends `line` and a newline. A line that would push the file past
    /// its limit goes to a fresh file; the old ones shift one generation.
    ///
    /// # Errors
    /// I/O errors.
    pub fn write_line(&mut self, line: &str) -> io::Result<()> {
        let add = line.len() as u64 + 1;
        if self.len > 0 && self.len + add > self.rotation.max_bytes {
            self.rotate()?;
        }
        let mut buf = Vec::with_capacity(line.len() + 1);
        buf.extend_from_slice(line.as_bytes());
        buf.push(b'\n');
        self.file.write_all(&buf)?;
        self.len += add;
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        let keep = self.rotation.keep;
        // Generations past `keep` (left by a larger earlier setting) go too.
        for n in (keep..=MAX_KEEP).rev() {
            remove_if_present(&generation_path(&self.path, n))?;
        }
        for n in (1..keep).rev() {
            rename_if_present(
                &generation_path(&self.path, n),
                &generation_path(&self.path, n + 1),
            )?;
        }
        fs::rename(&self.path, generation_path(&self.path, 1))?;
        self.file = open_append(&self.path)?;
        self.len = 0;
        Ok(())
    }
}

/// The last `n` lines of `path`, continuing into older generations
/// (`<file>.1`, `<file>.2`, …) while the newer ones have fewer. A missing
/// file has no lines; the first missing generation ends the search.
///
/// # Errors
/// I/O errors other than a missing file.
pub fn read_tail(path: &Path, n: usize) -> io::Result<Vec<String>> {
    let mut lines = tail_of(path, n)?;
    for g in 1..=MAX_KEEP {
        if lines.len() >= n {
            break;
        }
        let older_path = generation_path(path, g);
        if fs::symlink_metadata(&older_path).is_err() {
            break;
        }
        let mut older = tail_of(&older_path, n - lines.len())?;
        older.append(&mut lines);
        lines = older;
    }
    Ok(lines)
}

fn tail_of(path: &Path, n: usize) -> io::Result<Vec<String>> {
    if n == 0 {
        return Ok(Vec::new());
    }
    let mut file = match open_read(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let len = file.metadata()?.len();
    let start = len.saturating_sub(TAIL_WINDOW);
    file.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    file.take(len - start).read_to_end(&mut buf)?;
    let mut text: &[u8] = &buf;
    if start > 0 {
        // The window began mid-line; drop the partial first line.
        let cut = text
            .iter()
            .position(|&b| b == b'\n')
            .map_or(text.len(), |i| i + 1);
        text = &text[cut..];
    }
    let mut lines: Vec<String> = text.split(|&b| b == b'\n').map(to_line).collect();
    if text.last() == Some(&b'\n') || text.is_empty() {
        lines.pop();
    }
    let skip = lines.len().saturating_sub(n);
    Ok(lines.split_off(skip))
}

fn to_line(bytes: &[u8]) -> String {
    let bytes = &bytes[..bytes.len().min(MAX_LINE_BYTES)];
    let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
    String::from_utf8_lossy(bytes).into_owned()
}

/// Follows a log file like `tail -F`: new lines as they are written, across
/// rotation and while the file does not exist yet.
#[derive(Debug)]
pub struct Follower {
    path: PathBuf,
    file: Option<(File, u64, u64)>,
    pos: u64,
    partial: Vec<u8>,
}

impl Follower {
    /// Starts at the current end of `path` (or at its start once it appears).
    #[must_use]
    pub fn from_end(path: &Path) -> Self {
        let mut f = Self {
            path: path.to_path_buf(),
            file: None,
            pos: 0,
            partial: Vec::new(),
        };
        if let Ok(file) = open_read(path)
            && let Ok(meta) = file.metadata()
        {
            f.pos = meta.len();
            f.file = Some((file, meta.dev(), meta.ino()));
        }
        f
    }

    /// The followed file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Complete lines written since the last call.
    ///
    /// # Errors
    /// I/O errors other than the file being missing.
    pub fn poll(&mut self) -> io::Result<Vec<String>> {
        let mut out = Vec::new();
        let current = match fs::symlink_metadata(&self.path) {
            Ok(m) if m.file_type().is_file() => Some((m.dev(), m.ino())),
            Ok(_) => None,
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        let replaced = match (&self.file, current) {
            (Some((_, dev, ino)), Some(now)) => (*dev, *ino) != now,
            (Some(_), None) => true,
            (None, _) => false,
        };
        // Finish the old file before switching to the new one.
        self.read_new(&mut out)?;
        if replaced || self.file.is_none() {
            if replaced {
                self.flush_partial(&mut out);
            }
            self.file = None;
            self.pos = 0;
            if current.is_some()
                && let Ok(file) = open_read(&self.path)
            {
                let meta = file.metadata()?;
                self.file = Some((file, meta.dev(), meta.ino()));
                self.read_new(&mut out)?;
            }
        }
        Ok(out)
    }

    fn read_new(&mut self, out: &mut Vec<String>) -> io::Result<()> {
        let Some((file, _, _)) = &mut self.file else {
            return Ok(());
        };
        let len = file.metadata()?.len();
        if len < self.pos {
            // Truncated in place: start over.
            self.pos = 0;
            self.partial.clear();
        }
        if len == self.pos {
            return Ok(());
        }
        file.seek(SeekFrom::Start(self.pos))?;
        let mut buf = Vec::new();
        file.take(len - self.pos).read_to_end(&mut buf)?;
        self.pos += buf.len() as u64;
        for chunk in buf.split_inclusive(|&b| b == b'\n') {
            if let Some(line) = chunk.strip_suffix(b"\n") {
                self.partial.extend_from_slice(line);
                out.push(to_line(&self.partial));
                self.partial.clear();
            } else if self.partial.len() < MAX_LINE_BYTES {
                let room = MAX_LINE_BYTES - self.partial.len();
                self.partial
                    .extend_from_slice(&chunk[..chunk.len().min(room)]);
            }
        }
        Ok(())
    }

    fn flush_partial(&mut self, out: &mut Vec<String>) {
        if !self.partial.is_empty() {
            out.push(to_line(&self.partial));
            self.partial.clear();
        }
    }
}
