//! Telling what a database dump is, and unpacking Odoo's backup zips safely.
//!
//! [`detect`] reads a file's first bytes: an Odoo backup zip (`dump.sql`,
//! `filestore/`, `manifest.json`), a `pg_dump` custom or tar archive, or a
//! plain SQL script. Compressed files (gzip, zstd, xz, bzip2) are refused
//! with a hint to decompress them first.
//!
//! [`extract_odoo_zip`] unpacks only `dump.sql`, `manifest.json` and
//! `filestore/…`, after checking every entry of the archive: no absolute or
//! `..` paths, backslashes or NUL bytes (zip-slip), no symlinks, no entry
//! twice, no encrypted entry, and it stops a zip bomb by counting the bytes
//! it actually writes against a total limit and a per-entry compression
//! ratio (headers are not trusted).

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The SQL script in an Odoo backup zip.
pub const DUMP_SQL: &str = "dump.sql";
/// The description in an Odoo backup zip.
pub const MANIFEST_JSON: &str = "manifest.json";
/// The folder holding the attachments in an Odoo backup zip.
pub const FILESTORE_DIR: &str = "filestore";

/// Largest `manifest.json` read.
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
/// Bytes read by [`detect`].
const SNIFF_BYTES: usize = 8192;
/// Entries smaller than this are never refused for their ratio.
const RATIO_FLOOR: u64 = 1024 * 1024;

/// What a dump file is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DumpFormat {
    /// Odoo's backup zip: `dump.sql` (plain SQL), `filestore/`, `manifest.json`.
    Zip,
    /// `pg_dump -Fc` (custom format), restored with `pg_restore`.
    Custom,
    /// `pg_dump -Ft` (tar format), restored with `pg_restore`.
    Tar,
    /// A plain SQL script (`pg_dump` default), run with `psql`.
    Sql,
}

impl DumpFormat {
    /// Whether `psql` runs it (else `pg_restore`).
    #[must_use]
    pub fn uses_psql(self) -> bool {
        matches!(self, Self::Zip | Self::Sql)
    }
}

/// Limits of [`extract_odoo_zip`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtractLimits {
    /// Most bytes written in all.
    pub max_bytes: u64,
    /// Most an entry over 1 MiB may grow when unpacked (unpacked ÷ packed).
    pub max_ratio: u64,
    /// Most entries in the archive.
    pub max_entries: usize,
}

impl Default for ExtractLimits {
    /// 64 GiB, 200×, a million entries. SQL dumps unpack to 5-20× their
    /// packed size; zip bombs reach thousands.
    fn default() -> Self {
        Self {
            max_bytes: 64 * 1024 * 1024 * 1024,
            max_ratio: 200,
            max_entries: 1_000_000,
        }
    }
}

/// What `manifest.json` says (the fields OdHive reads).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Manifest {
    /// Odoo series that made the backup, e.g. `19.0` or `saas~19.1`.
    #[serde(default)]
    pub version: Option<String>,
    /// PostgreSQL server version it came from, e.g. `16.0`.
    #[serde(default)]
    pub pg_version: Option<String>,
    /// Name of the database it was made from.
    #[serde(default)]
    pub db_name: Option<String>,
}

/// What [`extract_odoo_zip`] wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extracted {
    /// `dump.sql`.
    pub dump_sql: PathBuf,
    /// `filestore/`, when the archive has one.
    pub filestore: Option<PathBuf>,
    /// `manifest.json`, when present and readable.
    pub manifest: Option<Manifest>,
    /// Files written under `filestore/`.
    pub filestore_files: u64,
    /// Bytes written in all.
    pub bytes: u64,
}

/// Why a dump cannot be used.
#[derive(Debug, Error)]
pub enum ArchiveError {
    /// The file is empty.
    #[error("the file is empty")]
    Empty,
    /// The file is compressed.
    #[error("the file is {0}-compressed: decompress it first, then restore the result")]
    Compressed(&'static str),
    /// Not a dump OdHive knows.
    #[error(
        "not a database dump: expected an Odoo backup zip, a pg_dump custom or tar archive, or a plain SQL script"
    )]
    Unknown,
    /// A zip without `dump.sql`.
    #[error("the zip has no dump.sql: it is not an Odoo backup")]
    NoDump,
    /// An entry that could write outside the folder or is not a plain file.
    #[error("unsafe entry {entry:?} in the zip: {why}")]
    Unsafe {
        /// Entry name (lossy).
        entry: String,
        /// Why.
        why: &'static str,
    },
    /// Over [`ExtractLimits::max_entries`].
    #[error("the zip has more than {0} entries")]
    TooManyEntries(usize),
    /// Over [`ExtractLimits::max_bytes`].
    #[error("the zip unpacks to more than {0} bytes")]
    TooLarge(u64),
    /// An entry grows more than [`ExtractLimits::max_ratio`] or more than
    /// its header says.
    #[error(
        "entry {entry:?} unpacks to far more than its packed size: refusing a possible zip bomb"
    )]
    Bomb {
        /// Entry name.
        entry: String,
    },
    /// The zip itself is broken or uses an unsupported feature.
    #[error("cannot read the zip: {0}")]
    Zip(String),
    /// Reading or writing failed.
    #[error("{action} {path}: {source}")]
    Io {
        /// What was done.
        action: &'static str,
        /// Path.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
    /// Stopped because the work was cancelled.
    #[error("cancelled")]
    Cancelled,
}

fn io_err<'a>(action: &'static str, path: &'a Path) -> impl FnOnce(io::Error) -> ArchiveError + 'a {
    move |source| ArchiveError::Io {
        action,
        path: path.to_path_buf(),
        source,
    }
}

/// What the file at `path` is, from its first bytes.
///
/// # Errors
/// [`ArchiveError::Empty`], [`ArchiveError::Compressed`],
/// [`ArchiveError::Unknown`] or [`ArchiveError::Io`].
pub fn detect(path: &Path) -> Result<DumpFormat, ArchiveError> {
    let mut head = Vec::with_capacity(SNIFF_BYTES);
    File::open(path)
        .map_err(io_err("cannot open", path))?
        .take(SNIFF_BYTES as u64)
        .read_to_end(&mut head)
        .map_err(io_err("cannot read", path))?;
    detect_bytes(&head)
}

fn detect_bytes(head: &[u8]) -> Result<DumpFormat, ArchiveError> {
    if head.is_empty() {
        return Err(ArchiveError::Empty);
    }
    if head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06") {
        return Ok(DumpFormat::Zip);
    }
    if head.starts_with(b"PGDMP") {
        return Ok(DumpFormat::Custom);
    }
    if head.get(257..262) == Some(b"ustar") {
        return Ok(DumpFormat::Tar);
    }
    for (magic, name) in [
        (&b"\x1f\x8b"[..], "gzip"),
        (b"\x28\xb5\x2f\xfd", "zstd"),
        (b"\xfd7zXZ\x00", "xz"),
        (b"BZh", "bzip2"),
    ] {
        if head.starts_with(magic) {
            return Err(ArchiveError::Compressed(name));
        }
    }
    if is_text(head) {
        return Ok(DumpFormat::Sql);
    }
    Err(ArchiveError::Unknown)
}

/// UTF-8 without NUL, allowing a character cut at the end of the sample.
fn is_text(head: &[u8]) -> bool {
    if head.contains(&0) {
        return false;
    }
    match std::str::from_utf8(head) {
        Ok(_) => true,
        Err(e) => e.error_len().is_none() && head.len() - e.valid_up_to() < 4,
    }
}

/// Where a zip entry would go, relative to the destination: `None` when
/// OdHive does not unpack it. Every entry is checked, unpacked or not.
fn entry_target(name: &str, is_dir: bool) -> Result<Option<PathBuf>, &'static str> {
    if name.is_empty() {
        return Err("empty name");
    }
    if name.contains('\0') {
        return Err("NUL byte in the name");
    }
    if name.contains('\\') {
        return Err("backslash in the name");
    }
    if name.starts_with('/') {
        return Err("absolute path");
    }
    let trimmed = if is_dir {
        name.strip_suffix('/').unwrap_or(name)
    } else {
        name
    };
    for part in trimmed.split('/') {
        match part {
            "" => return Err("empty path component"),
            "." | ".." => return Err("relative path component"),
            p if p.contains(':') && p.len() >= 2 && p.as_bytes()[1] == b':' => {
                return Err("drive letter");
            }
            _ => {}
        }
    }
    let rel = PathBuf::from(trimmed);
    // Belt and braces: only plain components may remain.
    if !rel.components().all(|c| matches!(c, Component::Normal(_))) {
        return Err("not a plain relative path");
    }
    let wanted = trimmed == DUMP_SQL
        || trimmed == MANIFEST_JSON
        || trimmed == FILESTORE_DIR
        || trimmed.starts_with("filestore/");
    Ok(wanted.then_some(rel))
}

/// Counts bytes read and stops past `limit`.
struct Counted<R> {
    inner: R,
    read: u64,
    limit: u64,
}

impl<R: Read> Read for Counted<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read += n as u64;
        if self.read > self.limit {
            return Err(io::Error::other(OverLimit));
        }
        Ok(n)
    }
}

#[derive(Debug)]
struct OverLimit;

impl std::fmt::Display for OverLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("over the limit")
    }
}

impl std::error::Error for OverLimit {}

fn is_over_limit(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|i| i.is::<OverLimit>())
}

/// Checks every entry of the Odoo backup zip at `zip_path` (names, kinds,
/// declared sizes) without unpacking anything: what [`extract_odoo_zip`]
/// checks first, cheap enough to do before accepting an upload.
///
/// # Errors
/// See [`ArchiveError`].
pub fn check_odoo_zip(zip_path: &Path, limits: ExtractLimits) -> Result<(), ArchiveError> {
    let mut zip = open_zip(zip_path)?;
    plan(&mut zip, limits).map(|_| ())
}

fn open_zip(zip_path: &Path) -> Result<zip::ZipArchive<File>, ArchiveError> {
    let file = File::open(zip_path).map_err(io_err("cannot open", zip_path))?;
    zip::ZipArchive::new(file).map_err(|e| ArchiveError::Zip(e.to_string()))
}

/// Entries to unpack: index, relative path, whether a folder.
type Plan = Vec<(usize, PathBuf, bool)>;

/// Pass 1: names, kinds and declared sizes of every entry.
fn plan(zip: &mut zip::ZipArchive<File>, limits: ExtractLimits) -> Result<Plan, ArchiveError> {
    if zip.len() > limits.max_entries {
        return Err(ArchiveError::TooManyEntries(limits.max_entries));
    }
    let mut plan: Plan = Vec::new();
    let mut seen = HashSet::new();
    let mut declared: u64 = 0;
    let mut has_dump = false;
    for i in 0..zip.len() {
        let entry = zip
            .by_index_raw(i)
            .map_err(|e| ArchiveError::Zip(e.to_string()))?;
        let lossy = String::from_utf8_lossy(entry.name_raw()).into_owned();
        let unsafe_entry = |why| ArchiveError::Unsafe {
            entry: lossy.clone(),
            why,
        };
        let name = entry
            .name()
            .map_err(|_| unsafe_entry("name is not valid text"))?
            .into_owned();
        if entry.is_symlink() {
            return Err(unsafe_entry("symbolic link"));
        }
        let is_dir = entry.is_dir();
        if !is_dir && !entry.is_file() {
            return Err(unsafe_entry("not a regular file"));
        }
        let Some(rel) = entry_target(&name, is_dir).map_err(unsafe_entry)? else {
            continue;
        };
        if !seen.insert(rel.clone()) {
            return Err(unsafe_entry("appears twice"));
        }
        if entry.encrypted() {
            return Err(unsafe_entry("encrypted"));
        }
        if !is_dir {
            declared = declared.saturating_add(entry.size());
            if declared > limits.max_bytes {
                return Err(ArchiveError::TooLarge(limits.max_bytes));
            }
            if entry.size() > RATIO_FLOOR
                && entry.size() / entry.compressed_size().max(1) > limits.max_ratio
            {
                return Err(ArchiveError::Bomb { entry: name });
            }
            has_dump |= rel == Path::new(DUMP_SQL);
        }
        if rel == Path::new(DUMP_SQL) && is_dir || rel == Path::new(FILESTORE_DIR) && !is_dir {
            return Err(unsafe_entry("unexpected kind of entry"));
        }
        plan.push((i, rel, is_dir));
    }
    if !has_dump {
        return Err(ArchiveError::NoDump);
    }
    Ok(plan)
}

/// Checks every entry of the Odoo backup zip at `zip_path` and unpacks the
/// ones Odoo restores into `dest`, which must not exist yet (it is created
/// `0700`; files are `0600`). Checks `cancel` between chunks.
///
/// # Errors
/// See [`ArchiveError`]. `dest` may hold a partial result: remove it.
pub fn extract_odoo_zip(
    zip_path: &Path,
    dest: &Path,
    limits: ExtractLimits,
    cancel: &AtomicBool,
) -> Result<Extracted, ArchiveError> {
    let mut zip = open_zip(zip_path)?;
    let plan = plan(&mut zip, limits)?;

    // Pass 2: unpack, counting what is really written.
    let mut dirs = fs::DirBuilder::new();
    dirs.mode(0o700).recursive(true);
    fs::DirBuilder::new()
        .mode(0o700)
        .create(dest)
        .map_err(io_err("cannot create", dest))?;
    let mut written: u64 = 0;
    let mut filestore_files = 0;
    let mut manifest = None;
    for (i, rel, is_dir) in plan {
        if cancel.load(Ordering::Relaxed) {
            return Err(ArchiveError::Cancelled);
        }
        let path = dest.join(&rel);
        if is_dir {
            dirs.create(&path).map_err(io_err("cannot create", &path))?;
            continue;
        }
        if let Some(parent) = path.parent() {
            dirs.create(parent)
                .map_err(io_err("cannot create", parent))?;
        }
        let entry = zip
            .by_index(i)
            .map_err(|e| ArchiveError::Zip(e.to_string()))?;
        let name = rel.display().to_string();
        let packed = entry.compressed_size().max(1);
        let entry_limit = entry
            .size()
            .min(packed.saturating_mul(limits.max_ratio).max(RATIO_FLOOR));
        let total_left = limits.max_bytes - written;
        let mut reader = Counted {
            inner: entry,
            read: 0,
            limit: entry_limit.min(total_left),
        };
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(io_err("cannot create", &path))?;
        copy_cancellable(&mut reader, &mut out, cancel).map_err(|e| {
            if is_over_limit(&e) {
                if reader.read > total_left {
                    ArchiveError::TooLarge(limits.max_bytes)
                } else {
                    ArchiveError::Bomb {
                        entry: name.clone(),
                    }
                }
            } else if e.kind() == io::ErrorKind::Interrupted && cancel.load(Ordering::Relaxed) {
                ArchiveError::Cancelled
            } else if e.kind() == io::ErrorKind::InvalidData || e.kind() == io::ErrorKind::Other {
                ArchiveError::Zip(format!("{name}: {e}"))
            } else {
                ArchiveError::Io {
                    action: "cannot write",
                    path: path.clone(),
                    source: e,
                }
            }
        })?;
        written += reader.read;
        if rel.starts_with(FILESTORE_DIR) {
            filestore_files += 1;
        }
        if rel == Path::new(MANIFEST_JSON) && reader.read <= MAX_MANIFEST_BYTES {
            manifest = fs::read(&path)
                .ok()
                .and_then(|b| serde_json::from_slice::<Manifest>(&b).ok());
        }
    }
    let filestore = dest.join(FILESTORE_DIR);
    Ok(Extracted {
        dump_sql: dest.join(DUMP_SQL),
        filestore: filestore.is_dir().then_some(filestore),
        manifest,
        filestore_files,
        bytes: written,
    })
}

fn copy_cancellable(r: &mut impl Read, w: &mut impl Write, cancel: &AtomicBool) -> io::Result<()> {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        let n = match r.read(&mut buf) {
            Ok(0) => return w.flush(),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted && !cancel.load(Ordering::Relaxed) => {
                continue;
            }
            Err(e) => return Err(e),
        };
        w.write_all(&buf[..n])?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zip::write::SimpleFileOptions;

    fn write_zip(path: &Path, build: impl FnOnce(&mut zip::ZipWriter<File>)) {
        let mut w = zip::ZipWriter::new(File::create(path).expect("create"));
        build(&mut w);
        w.finish().expect("finish");
    }

    fn add(w: &mut zip::ZipWriter<File>, name: &str, data: &[u8]) {
        w.start_file(name, SimpleFileOptions::default())
            .expect("start");
        w.write_all(data).expect("write");
    }

    fn extract(
        zip: &Path,
        limits: ExtractLimits,
    ) -> (tempfile::TempDir, Result<Extracted, ArchiveError>) {
        let out = tempfile::tempdir().expect("tempdir");
        let r = extract_odoo_zip(zip, &out.path().join("x"), limits, &AtomicBool::new(false));
        (out, r)
    }

    #[test]
    fn formats_are_told_apart_by_their_first_bytes() {
        assert!(matches!(detect_bytes(b""), Err(ArchiveError::Empty)));
        assert_eq!(detect_bytes(b"PK\x03\x04rest").unwrap(), DumpFormat::Zip);
        assert_eq!(detect_bytes(b"PGDMP\x01\x10").unwrap(), DumpFormat::Custom);
        let mut tar = vec![0u8; 512];
        tar[..9].copy_from_slice(b"toc.dat\0\0");
        tar[257..263].copy_from_slice(b"ustar\0");
        assert_eq!(detect_bytes(&tar).unwrap(), DumpFormat::Tar);
        assert_eq!(
            detect_bytes(b"--\n-- PostgreSQL database dump\n--\n").unwrap(),
            DumpFormat::Sql
        );
        // A multi-byte character cut by the sample is still text.
        assert_eq!(
            detect_bytes("SELECT 'é".as_bytes().split_last().unwrap().1).unwrap(),
            DumpFormat::Sql
        );
        assert!(matches!(
            detect_bytes(b"\x1f\x8b\x08\x00"),
            Err(ArchiveError::Compressed("gzip"))
        ));
        assert!(matches!(
            detect_bytes(b"\x28\xb5\x2f\xfd"),
            Err(ArchiveError::Compressed("zstd"))
        ));
        assert!(matches!(
            detect_bytes(b"\x7fELF\x02\x01\x00"),
            Err(ArchiveError::Unknown)
        ));
        assert!(matches!(
            detect_bytes(&[0xff, 0xfe, 0x41]),
            Err(ArchiveError::Unknown)
        ));
    }

    #[test]
    fn an_odoo_backup_unpacks_only_what_odoo_restores() {
        let dir = tempfile::tempdir().expect("tempdir");
        let zip = dir.path().join("b.zip");
        write_zip(&zip, |w| {
            add(w, DUMP_SQL, b"CREATE TABLE t ();\n");
            add(
                w,
                MANIFEST_JSON,
                br#"{"odoo_dump": "1", "db_name": "prod", "version": "19.0", "pg_version": "16.4", "modules": {"base": "19.0.1.3"}}"#,
            );
            w.add_directory("filestore/ab/", SimpleFileOptions::default())
                .expect("dir");
            add(w, "filestore/ab/ab12", b"attachment");
            add(w, "filestore/cd/cd34", b"another");
            add(w, "notes/readme.txt", b"ignored");
        });
        let (out, r) = extract(&zip, ExtractLimits::default());
        let x = r.expect("extract");
        let root = out.path().join("x");
        assert_eq!(x.dump_sql, root.join(DUMP_SQL));
        assert_eq!(fs::read(&x.dump_sql).unwrap(), b"CREATE TABLE t ();\n");
        assert_eq!(
            x.filestore.as_deref(),
            Some(root.join("filestore").as_path())
        );
        assert_eq!(x.filestore_files, 2);
        assert_eq!(
            fs::read(root.join("filestore/ab/ab12")).unwrap(),
            b"attachment"
        );
        assert!(!root.join("notes").exists());
        let m = x.manifest.expect("manifest");
        assert_eq!(m.version.as_deref(), Some("19.0"));
        assert_eq!(m.db_name.as_deref(), Some("prod"));
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&root), 0o700);
        assert_eq!(mode(&root.join(DUMP_SQL)), 0o600);
    }

    #[test]
    fn zip_slip_entries_are_refused_anywhere_in_the_archive() {
        let dir = tempfile::tempdir().expect("tempdir");
        for evil in [
            "../../evil",
            "filestore/../../evil",
            "/etc/evil",
            "filestore/ab/./x",
            "filestore//x",
            "filestore\\..\\evil",
            "C:/evil",
            "notes/../../evil",
        ] {
            let zip = dir.path().join("evil.zip");
            write_zip(&zip, |w| {
                add(w, DUMP_SQL, b"SELECT 1;\n");
                add(w, evil, b"pwned");
            });
            let checked = check_odoo_zip(&zip, ExtractLimits::default());
            assert!(
                matches!(checked, Err(ArchiveError::Unsafe { .. })),
                "{evil:?}: {checked:?}"
            );
            let (out, r) = extract(&zip, ExtractLimits::default());
            assert!(
                matches!(r, Err(ArchiveError::Unsafe { .. })),
                "{evil:?}: {r:?}"
            );
            assert!(!dir.path().join("evil").exists());
            assert!(!out.path().join("evil").exists());
        }
    }

    #[test]
    fn symlinks_duplicates_and_zips_without_a_dump_are_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let zip = dir.path().join("link.zip");
        write_zip(&zip, |w| {
            add(w, DUMP_SQL, b"SELECT 1;\n");
            w.add_symlink("filestore/ab", "/etc", SimpleFileOptions::default())
                .expect("symlink");
        });
        let (_out, r) = extract(&zip, ExtractLimits::default());
        assert!(
            matches!(
                r,
                Err(ArchiveError::Unsafe {
                    why: "symbolic link",
                    ..
                })
            ),
            "{r:?}"
        );

        // A link outside what is unpacked is refused too.
        write_zip(&zip, |w| {
            add(w, DUMP_SQL, b"SELECT 1;\n");
            w.add_symlink("other", "/etc", SimpleFileOptions::default())
                .expect("symlink");
        });
        let (_out, r) = extract(&zip, ExtractLimits::default());
        assert!(matches!(r, Err(ArchiveError::Unsafe { .. })), "{r:?}");

        let zip = dir.path().join("nodump.zip");
        write_zip(&zip, |w| add(w, "filestore/ab/x", b"x"));
        let (_out, r) = extract(&zip, ExtractLimits::default());
        assert!(matches!(r, Err(ArchiveError::NoDump)), "{r:?}");

        // Not a zip at all past its magic.
        let bad = dir.path().join("bad.zip");
        fs::write(&bad, b"PK\x03\x04garbage").unwrap();
        let (_out, r) = extract(&bad, ExtractLimits::default());
        assert!(matches!(r, Err(ArchiveError::Zip(_))), "{r:?}");
    }

    #[test]
    fn zip_bombs_are_stopped_by_ratio_and_total_size() {
        let dir = tempfile::tempdir().expect("tempdir");
        let deflate =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        // 8 MiB of zeros deflates ~1000×.
        let zip = dir.path().join("bomb.zip");
        write_zip(&zip, |w| {
            w.start_file(DUMP_SQL, deflate).expect("start");
            let zeros = vec![0u8; 1024 * 1024];
            for _ in 0..8 {
                w.write_all(&zeros).expect("write");
            }
        });
        let (_out, r) = extract(&zip, ExtractLimits::default());
        assert!(matches!(r, Err(ArchiveError::Bomb { .. })), "{r:?}");

        // Total size: many small honest entries over the limit.
        let zip = dir.path().join("big.zip");
        write_zip(&zip, |w| {
            add(w, DUMP_SQL, b"SELECT 1;\n");
            for i in 0..10 {
                add(w, &format!("filestore/f{i}"), &[b'x'; 1000]);
            }
        });
        let limits = ExtractLimits {
            max_bytes: 5000,
            ..ExtractLimits::default()
        };
        let (_out, r) = extract(&zip, limits);
        assert!(matches!(r, Err(ArchiveError::TooLarge(5000))), "{r:?}");
        let limits = ExtractLimits {
            max_entries: 5,
            ..ExtractLimits::default()
        };
        let (_out, r) = extract(&zip, limits);
        assert!(matches!(r, Err(ArchiveError::TooManyEntries(5))), "{r:?}");
    }

    #[test]
    fn a_lying_header_is_caught_by_counting_written_bytes() {
        // An entry whose header claims fewer bytes than it holds: the reader
        // is capped at the declared size (the zip crate also checks CRC).
        let dir = tempfile::tempdir().expect("tempdir");
        let zip = dir.path().join("x.zip");
        write_zip(&zip, |w| add(w, DUMP_SQL, &[b'a'; 4096]));
        let mut bytes = fs::read(&zip).unwrap();
        // Patch the uncompressed size (offset 22) in the local header and the
        // central directory entry to 100.
        let local = 22;
        bytes[local..local + 4].copy_from_slice(&100u32.to_le_bytes());
        let cd = bytes
            .windows(4)
            .position(|w| w == b"PK\x01\x02")
            .expect("central directory");
        bytes[cd + 24..cd + 28].copy_from_slice(&100u32.to_le_bytes());
        fs::write(&zip, bytes).unwrap();
        let (out, r) = extract(&zip, ExtractLimits::default());
        assert!(r.is_err(), "{r:?}");
        let written = fs::metadata(out.path().join("x").join(DUMP_SQL)).map_or(0, |m| m.len());
        assert!(written <= 100, "{written}");
    }

    #[test]
    fn cancelling_stops_the_unpacking() {
        let dir = tempfile::tempdir().expect("tempdir");
        let zip = dir.path().join("b.zip");
        write_zip(&zip, |w| add(w, DUMP_SQL, b"SELECT 1;\n"));
        let out = tempfile::tempdir().expect("tempdir");
        let r = extract_odoo_zip(
            &zip,
            &out.path().join("x"),
            ExtractLimits::default(),
            &AtomicBool::new(true),
        );
        assert!(matches!(r, Err(ArchiveError::Cancelled)), "{r:?}");
    }
}
