//! Instance log files: `<project>/logs/<instance>.log`.
//!
//! The process that supervises an instance appends every output line to its
//! file; `odhive logs` reads and follows it from any other process. One
//! rotated generation (`<instance>.log.1`) is kept once a file reaches
//! [`DEFAULT_MAX_BYTES`]. (Milestone 4 adds parsing, a ring buffer and more
//! generations on top of this.)
//!
//! Files are created `0600` in a `0700` folder and are never opened through a
//! symlink.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::fsutil::{DIR_MODE, FILE_MODE};
use crate::names;

#[cfg(test)]
mod tests;

/// Name of the log folder inside a project.
pub const LOGS_DIR: &str = "logs";

/// Size at which a log file is rotated.
pub const DEFAULT_MAX_BYTES: u64 = 16 * 1024 * 1024;

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

/// The rotated generation of `path` (`<file>.1`).
#[must_use]
pub fn rotated_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".1");
    PathBuf::from(name)
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
    max: u64,
}

impl LogFile {
    /// Opens (or creates) `path` for appending. Its folder is created `0700`.
    ///
    /// # Errors
    /// I/O errors, including `path` being a symlink.
    pub fn open(path: &Path, max: u64) -> io::Result<Self> {
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
            max: max.max(1),
        })
    }

    /// The file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends `line` and a newline. A line that would push the file past
    /// its limit goes to a fresh file; the old one becomes `<file>.1`.
    ///
    /// # Errors
    /// I/O errors.
    pub fn write_line(&mut self, line: &str) -> io::Result<()> {
        let add = line.len() as u64 + 1;
        if self.len > 0 && self.len + add > self.max {
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
        fs::rename(&self.path, rotated_path(&self.path))?;
        self.file = open_append(&self.path)?;
        self.len = 0;
        Ok(())
    }
}

/// The last `n` lines of `path`, continuing into `<file>.1` when the current
/// file has fewer. A missing file has no lines.
///
/// # Errors
/// I/O errors other than a missing file.
pub fn read_tail(path: &Path, n: usize) -> io::Result<Vec<String>> {
    let mut lines = tail_of(path, n)?;
    if lines.len() < n {
        let mut older = tail_of(&rotated_path(path), n - lines.len())?;
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
