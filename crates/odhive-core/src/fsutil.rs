//! Private file helpers shared by every config file OdHive owns.
//!
//! Files are written atomically with mode `0600` (parent folders `0700`) and read
//! with a size cap; a file readable by group or others is tightened on read.

use std::fs::{self, DirBuilder, File, Permissions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Mode of files OdHive writes.
pub const FILE_MODE: u32 = 0o600;
/// Mode of folders OdHive creates.
pub const DIR_MODE: u32 = 0o700;

/// An I/O error together with the path it happened on.
#[derive(Debug)]
pub struct PathError {
    /// Path involved.
    pub path: PathBuf,
    /// Underlying error. A file over the size cap has kind [`io::ErrorKind::FileTooLarge`].
    pub source: io::Error,
}

impl PathError {
    fn new(path: &Path) -> impl FnOnce(io::Error) -> Self + '_ {
        move |source| Self {
            path: path.to_path_buf(),
            source,
        }
    }

    /// Whether the file exceeded the size cap.
    #[must_use]
    pub fn is_too_large(&self) -> bool {
        self.source.kind() == io::ErrorKind::FileTooLarge
    }
}

/// Reads a private text file of at most `max` bytes; `None` if it does not exist.
///
/// # Errors
/// I/O errors, or [`io::ErrorKind::FileTooLarge`] when the file exceeds `max`.
pub fn read_private(path: &Path, max: u64) -> Result<Option<String>, PathError> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(PathError::new(path)(e)),
    };
    let too_large = || {
        PathError::new(path)(io::Error::new(
            io::ErrorKind::FileTooLarge,
            format!("larger than {max} bytes"),
        ))
    };
    let meta = file.metadata().map_err(PathError::new(path))?;
    if meta.len() > max {
        return Err(too_large());
    }
    if meta.permissions().mode() & 0o077 != 0 {
        fs::set_permissions(path, Permissions::from_mode(FILE_MODE))
            .map_err(PathError::new(path))?;
    }
    let mut text = String::new();
    // `take` guards against the file growing between `metadata` and `read`.
    file.take(max + 1)
        .read_to_string(&mut text)
        .map_err(PathError::new(path))?;
    if text.len() as u64 > max {
        return Err(too_large());
    }
    Ok(Some(text))
}

/// Atomically replaces `path` with `contents` (temp file in the same folder, fsync,
/// rename). The file gets mode `0600`; a missing parent folder is created `0700`.
///
/// # Errors
/// Returns the failing path and I/O error. Nothing is left behind on failure.
pub fn write_private_atomic(path: &Path, contents: &[u8]) -> Result<(), PathError> {
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    DirBuilder::new()
        .recursive(true)
        .mode(DIR_MODE)
        .create(dir)
        .map_err(PathError::new(dir))?;

    let prefix = format!(
        ".{}.",
        path.file_name()
            .map_or_else(|| "odhive".into(), |n| n.to_string_lossy())
    );
    // NamedTempFile is created with mode 0600 and removed if we bail out early.
    let mut tmp = tempfile::Builder::new()
        .prefix(&prefix)
        .suffix(".tmp")
        .permissions(Permissions::from_mode(FILE_MODE))
        .tempfile_in(dir)
        .map_err(PathError::new(dir))?;
    tmp.write_all(contents).map_err(PathError::new(path))?;
    tmp.as_file().sync_all().map_err(PathError::new(path))?;
    tmp.persist(path).map_err(|e| PathError {
        path: path.to_path_buf(),
        source: e.error,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).expect("metadata").permissions().mode() & 0o777
    }

    #[test]
    fn write_then_read_is_private_and_leaves_no_temp_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("sub/a.toml");
        write_private_atomic(&file, b"one").expect("write");
        write_private_atomic(&file, b"two").expect("overwrite");
        assert_eq!(mode(&file), 0o600);
        assert_eq!(mode(&dir.path().join("sub")), 0o700);
        assert_eq!(
            read_private(&file, 10).expect("read").as_deref(),
            Some("two")
        );
        let names: Vec<_> = fs::read_dir(dir.path().join("sub"))
            .expect("read_dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(names, vec!["a.toml"]);
    }

    #[test]
    fn missing_file_is_none_and_loose_mode_is_tightened() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.toml");
        assert!(read_private(&file, 10).expect("read").is_none());
        fs::write(&file, "x").expect("write");
        fs::set_permissions(&file, Permissions::from_mode(0o644)).expect("chmod");
        read_private(&file, 10).expect("read");
        assert_eq!(mode(&file), 0o600);
    }

    #[test]
    fn oversized_file_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.toml");
        fs::write(&file, "12345678901").expect("write");
        assert!(
            read_private(&file, 10)
                .expect_err("too large")
                .is_too_large()
        );
    }
}
