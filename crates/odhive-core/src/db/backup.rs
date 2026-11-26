//! Backing up an Odoo database into the project's `backups/` folder, and the
//! backups found there.
//!
//! [`backup`] makes one of two kinds of file, never through a shell:
//!
//! - **zip**: Odoo's own backup (`odoo-bin db -c <conf> dump --format zip
//!   <db> <file>`): `dump.sql` (plain `pg_dump --no-owner`), the database's
//!   `filestore/` and `manifest.json` (Odoo version, modules), the file
//!   Odoo's database manager makes and restores. Odoo copies the filestore
//!   into a temporary folder first; it gets a private folder next to the
//!   backups (`TMPDIR`) so large filestores do not fill `/tmp`.
//! - **dump**: `pg_dump --no-owner --format=custom` only, no filestore. OdHive
//!   runs `pg_dump` itself: `odoo-bin db dump --format dump` does not look at
//!   `pg_dump`'s exit status, so a failed dump would look like a good one.
//!
//! The file is written in a private staging folder inside `backups/`, then
//! checked (a zip must be an Odoo backup OdHive can read, a dump an archive
//! `pg_restore --list` reads), made `0600` and moved into place as
//! `<db>_<YYYYMMDD>-<HHMMSS>.<zip|dump>` (UTC). A failed or cancelled backup
//! leaves nothing behind.
//!
//! [`list_backups`], [`open_backup`] and [`delete_backup`] work on the files
//! of `backups/` by name: plain files only (links are not followed), names
//! of letters, digits, `_`, `.` and `-` ending in `.zip` or `.dump`.

use std::fmt;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::PgTarget;
use super::archive::{self, DumpFormat, ExtractLimits};
use super::create::{OdooCommand, TAIL_LINES};
use super::list::pg_message;
use super::restore::{PgTools, pg_command, pg_env};
use crate::envcheck::postgres;
use crate::names::{self, NameError};
use crate::proc::{self, RunError};

/// Longest backup file name.
pub const MAX_BACKUP_NAME_LEN: usize = 255;

/// Prefix of the staging folders inside `backups/` (never listed).
const STAGING_PREFIX: &str = ".odhive-backup-";

/// What kind of backup.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum BackupFormat {
    /// Odoo's backup zip: `dump.sql`, `filestore/`, `manifest.json`.
    #[default]
    Zip,
    /// A `pg_dump` custom-format archive, without the filestore.
    Dump,
}

impl BackupFormat {
    /// File extension, without the dot.
    #[must_use]
    pub fn extension(self) -> &'static str {
        match self {
            Self::Zip => "zip",
            Self::Dump => "dump",
        }
    }

    /// The format a file name's extension stands for.
    #[must_use]
    pub fn of_file(name: &str) -> Option<Self> {
        let (stem, ext) = name.rsplit_once('.')?;
        if stem.is_empty() {
            return None;
        }
        match ext {
            "zip" => Some(Self::Zip),
            "dump" => Some(Self::Dump),
            _ => None,
        }
    }

    /// The archive format [`archive::detect`] must see in the file.
    fn dump_format(self) -> DumpFormat {
        match self {
            Self::Zip => DumpFormat::Zip,
            Self::Dump => DumpFormat::Custom,
        }
    }
}

/// A backup to make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupRequest {
    /// The database to back up.
    pub database: String,
    /// What kind of file.
    pub format: BackupFormat,
}

/// A step of [`backup`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum BackupStep {
    /// Dumping the database (and its filestore, for a zip).
    Dump,
    /// Checking the file.
    Check,
}

impl fmt::Display for BackupStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Dump => "dumping the database",
            Self::Check => "checking the backup",
        })
    }
}

/// Progress of [`backup`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum BackupEvent {
    /// A step started.
    Step {
        /// The step.
        step: BackupStep,
    },
    /// A line of `odoo-bin` or `pg_dump` output.
    Output {
        /// The line.
        line: String,
    },
}

/// A backup file in `backups/`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Backup {
    /// File name inside `backups/`.
    pub file: String,
    /// The database it was made from, read from an OdHive file name.
    pub database: Option<String>,
    /// What kind of file.
    pub format: BackupFormat,
    /// Size in bytes.
    pub size: u64,
    /// Last change, milliseconds since the Unix epoch.
    pub modified_at: u64,
}

/// Errors from [`backup`] and the backup file functions.
#[derive(Debug, Error)]
pub enum BackupError {
    /// The database name is invalid.
    #[error(transparent)]
    Name(#[from] NameError),
    /// No database has this name (or the project does not list it).
    #[error("no database named {0:?}")]
    NotFound(String),
    /// A backup file name OdHive does not accept.
    #[error(
        "invalid backup file name {0:?}: use letters, digits, '_', '.' or '-', ending in .zip or .dump"
    )]
    InvalidFile(String),
    /// No backup file has this name.
    #[error("no backup named {0:?}")]
    FileNotFound(String),
    /// A backup with the new file's name exists.
    #[error("a backup named {0:?} already exists")]
    FileExists(String),
    /// The PostgreSQL server cannot be used.
    #[error("PostgreSQL: {0}")]
    Server(String),
    /// A tool could not be started.
    #[error("could not run {program}: {source}")]
    Spawn {
        /// Program.
        program: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// A step failed; nothing was kept.
    #[error("{step} failed (exit code {code:?}); nothing was kept. Last output:\n{tail}")]
    Failed {
        /// The step.
        step: BackupStep,
        /// Exit code, if a tool exited normally.
        code: Option<i32>,
        /// Last output lines, or what the check found.
        tail: String,
    },
    /// A file operation failed.
    #[error("{action} {}: {source}", path.display())]
    Io {
        /// What was done.
        action: &'static str,
        /// Path.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
}

fn io_err(action: &'static str, path: &Path) -> impl FnOnce(std::io::Error) -> BackupError {
    let path = path.to_path_buf();
    move |source| BackupError::Io {
        action,
        path,
        source,
    }
}

/// Checks a backup file name and returns its format: 1-255 letters, digits,
/// `_`, `.` or `-`, not starting with `.` or `-`, ending in `.zip` or
/// `.dump`. Rules out paths (`/`, `..`) and hidden files.
///
/// # Errors
/// [`BackupError::InvalidFile`].
pub fn validate_backup_name(name: &str) -> Result<BackupFormat, BackupError> {
    let ok = !name.is_empty()
        && name.len() <= MAX_BACKUP_NAME_LEN
        && !name.starts_with(['.', '-'])
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
    ok.then(|| BackupFormat::of_file(name))
        .flatten()
        .ok_or_else(|| BackupError::InvalidFile(name.to_owned()))
}

/// `<db>_<YYYYMMDD>-<HHMMSS>.<ext>` for a backup made `unix_secs` seconds
/// after the epoch (UTC).
#[must_use]
pub fn backup_file_name(database: &str, format: BackupFormat, unix_secs: u64) -> String {
    let days = i64::try_from(unix_secs / 86_400).unwrap_or(i64::MAX);
    let (y, m, d) = civil_from_days(days);
    let rest = unix_secs % 86_400;
    format!(
        "{database}_{y:04}{m:02}{d:02}-{:02}{:02}{:02}.{}",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60,
        format.extension()
    )
}

/// The database an OdHive backup file name was made from.
fn database_of(file: &str) -> Option<String> {
    let (stem, _) = file.rsplit_once('.')?;
    let (db, stamp) = stem.rsplit_once('_')?;
    let (date, time) = stamp.split_once('-')?;
    let digits = |s: &str, n| s.len() == n && s.bytes().all(|b| b.is_ascii_digit());
    (digits(date, 8) && digits(time, 6) && names::validate_db_name(db).is_ok())
        .then(|| db.to_owned())
}

/// Days since 1970-01-01 → (year, month, day), proleptic Gregorian.
pub(super) fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let m = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

pub(super) fn unix_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

impl BackupRequest {
    /// `odoo-bin db dump` arguments for a zip into `file`.
    fn odoo_args(&self, conf: &Path, file: &Path) -> Vec<String> {
        vec![
            "db".into(),
            "-c".into(),
            conf.display().to_string(),
            "dump".into(),
            "--format".into(),
            "zip".into(),
            // A checked name never starts with `-`; the path is absolute.
            self.database.clone(),
            file.display().to_string(),
        ]
    }

    /// `pg_dump` arguments for a custom-format archive into `file`, as
    /// Odoo dumps (`--no-owner`).
    fn pg_dump_args(&self, file: &Path) -> Vec<String> {
        vec![
            "--no-owner".into(),
            "--format=custom".into(),
            format!("--file={}", file.display()),
            format!("--dbname={}", self.database),
        ]
    }
}

/// Backs up `req.database` (see the module docs) from the server `target`
/// points at, which must be the one `odoo`'s config connects to, into
/// `backups_dir` (created `0700` when missing). `tools` gives `pg_dump` and
/// `pg_restore`. Reports progress to `on_event`.
///
/// When the returned future is dropped (job cancelled) the running tool is
/// killed and the staging folder removed.
///
/// # Errors
/// See [`BackupError`].
pub async fn backup(
    target: &PgTarget,
    odoo: &OdooCommand,
    tools: &PgTools,
    req: &BackupRequest,
    backups_dir: &Path,
    timeout: Duration,
    mut on_event: impl FnMut(BackupEvent),
) -> Result<Backup, BackupError> {
    names::validate_db_name(&req.database)?;
    let conn = postgres::connect(target, timeout)
        .await
        .map_err(|e| BackupError::Server(postgres::failure_check(target, e, None, None).summary))?;
    let found: bool = conn
        .client
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_database WHERE datname = $1)",
            &[&req.database],
        )
        .await
        .and_then(|r| r.try_get(0))
        .map_err(|e| BackupError::Server(pg_message(&e)))?;
    if !found {
        return Err(BackupError::NotFound(req.database.clone()));
    }
    let place = conn.place.clone();
    drop(conn);

    ensure_dir(backups_dir)?;
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let file = backup_file_name(&req.database, req.format, secs);
    let dest = backups_dir.join(&file);
    if dest.symlink_metadata().is_ok() {
        return Err(BackupError::FileExists(file));
    }
    // Removed when this function returns or its future is dropped.
    let staging = tempfile::Builder::new()
        .prefix(STAGING_PREFIX)
        .tempdir_in(backups_dir)
        .map_err(io_err("cannot create a staging folder in", backups_dir))?;
    let work = staging.path().join(&file);

    on_event(BackupEvent::Step {
        step: BackupStep::Dump,
    });
    let (program, cmd) = match req.format {
        BackupFormat::Zip => {
            let tmp = staging.path().join("tmp");
            fs::DirBuilder::new()
                .mode(0o700)
                .create(&tmp)
                .map_err(io_err("cannot create", &tmp))?;
            let mut cmd = odoo.command(&req.odoo_args(&odoo.conf, &work));
            cmd.env("TMPDIR", &tmp);
            (odoo.python.clone(), cmd)
        }
        BackupFormat::Dump => {
            let env = pg_env(target, &place);
            let mut cmd = pg_command(odoo, &tools.pg_dump, &env);
            cmd.args(req.pg_dump_args(&work));
            (tools.pg_dump.clone(), cmd)
        }
    };
    run(&program, BackupStep::Dump, cmd, &mut on_event).await?;

    on_event(BackupEvent::Step {
        step: BackupStep::Check,
    });
    check_file(tools, odoo, req.format, &work).await?;
    let meta = fs::symlink_metadata(&work).map_err(io_err("cannot read", &work))?;
    fs::set_permissions(&work, fs::Permissions::from_mode(0o600))
        .map_err(io_err("cannot make private", &work))?;
    if dest.symlink_metadata().is_ok() {
        return Err(BackupError::FileExists(file));
    }
    fs::rename(&work, &dest).map_err(io_err("cannot move the backup to", &dest))?;
    drop(staging);
    Ok(Backup {
        database: Some(req.database.clone()),
        format: req.format,
        size: meta.len(),
        modified_at: meta.modified().map_or(0, unix_ms),
        file,
    })
}

/// Creates `dir` (`0700`) when missing; refuses a link or a file there.
fn ensure_dir(dir: &Path) -> Result<(), BackupError> {
    match dir.symlink_metadata() {
        Ok(m) if m.is_dir() => Ok(()),
        Ok(_) => Err(BackupError::Io {
            action: "not a folder:",
            path: dir.to_path_buf(),
            source: std::io::Error::from(std::io::ErrorKind::NotADirectory),
        }),
        Err(_) => fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(io_err("cannot create", dir)),
    }
}

async fn run(
    program: &Path,
    step: BackupStep,
    cmd: tokio::process::Command,
    on_event: &mut impl FnMut(BackupEvent),
) -> Result<(), BackupError> {
    let done = proc::run_streaming(cmd, TAIL_LINES, |line| {
        on_event(BackupEvent::Output { line });
    })
    .await
    .map_err(|e| match e {
        RunError::Spawn(source) => BackupError::Spawn {
            program: program.to_path_buf(),
            source,
        },
        RunError::Io(e) => BackupError::Failed {
            step,
            code: None,
            tail: e.to_string(),
        },
    })?;
    if done.status.success() {
        Ok(())
    } else {
        Err(BackupError::Failed {
            step,
            code: done.status.code(),
            tail: done.tail,
        })
    }
}

/// The new file must be what was asked for: an Odoo backup zip OdHive can
/// read (with `dump.sql`, safe entries), or an archive `pg_restore` lists.
async fn check_file(
    tools: &PgTools,
    odoo: &OdooCommand,
    format: BackupFormat,
    file: &Path,
) -> Result<(), BackupError> {
    let failed = |tail: String| BackupError::Failed {
        step: BackupStep::Check,
        code: Some(0),
        tail,
    };
    let path = file.to_path_buf();
    let found = tokio::task::spawn_blocking(move || -> Result<DumpFormat, String> {
        let found = archive::detect(&path).map_err(|e| e.to_string())?;
        if found == DumpFormat::Zip {
            // Our own file: only its shape is checked, not its size.
            let limits = ExtractLimits {
                max_bytes: u64::MAX,
                max_ratio: u64::MAX,
                max_entries: usize::MAX,
            };
            archive::check_odoo_zip(&path, limits).map_err(|e| e.to_string())?;
        }
        Ok(found)
    })
    .await
    .map_err(|e| failed(e.to_string()))?
    .map_err(|why| failed(format!("the backup is not usable: {why}")))?;
    if found != format.dump_format() {
        return Err(failed(format!(
            "expected a {} backup, the file is {found:?}",
            format.extension()
        )));
    }
    if format == BackupFormat::Dump {
        let mut cmd = pg_command(odoo, &tools.pg_restore, &[]);
        cmd.arg("--list").arg(file);
        // The table of contents is not progress: kept only for an error.
        run(&tools.pg_restore, BackupStep::Check, cmd, &mut |_| {}).await?;
    }
    Ok(())
}

/// The backups in `dir`, newest first. A missing folder has none. Files
/// whose names [`validate_backup_name`] refuses, links and folders are left
/// out.
///
/// # Errors
/// [`BackupError::Io`] when the folder cannot be read.
pub fn list_backups(dir: &Path) -> Result<Vec<Backup>, BackupError> {
    match dir.symlink_metadata() {
        Ok(m) if m.is_dir() => {}
        // Missing, a link or a file: no backups.
        Ok(_) => return Ok(Vec::new()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_err("cannot read", dir)(e)),
    }
    let entries = fs::read_dir(dir).map_err(io_err("cannot read", dir))?;
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(io_err("cannot read", dir))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(format) = validate_backup_name(&name) else {
            continue;
        };
        // `DirEntry::metadata` does not follow links.
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        out.push(Backup {
            database: database_of(&name),
            format,
            size: meta.len(),
            modified_at: meta.modified().map_or(0, unix_ms),
            file: name,
        });
    }
    out.sort_by(|a, b| {
        b.modified_at
            .cmp(&a.modified_at)
            .then_with(|| a.file.cmp(&b.file))
    });
    Ok(out)
}

/// Opens backup `file` of `dir` for reading: a plain file, not through a
/// link (nor a link to `dir`).
///
/// # Errors
/// [`BackupError::InvalidFile`], [`BackupError::FileNotFound`] or
/// [`BackupError::Io`].
pub fn open_backup(dir: &Path, file: &str) -> Result<(fs::File, Backup), BackupError> {
    use rustix::fs::{Mode, OFlags};
    let format = validate_backup_name(file)?;
    let not_found = || BackupError::FileNotFound(file.to_owned());
    if !dir.symlink_metadata().is_ok_and(|m| m.is_dir()) {
        return Err(not_found());
    }
    let path = dir.join(file);
    // NOFOLLOW: a link is refused; NONBLOCK: a FIFO does not hang the open.
    let fd = match rustix::fs::open(
        &path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(e) if e == rustix::io::Errno::NOENT || e == rustix::io::Errno::LOOP => {
            return Err(not_found());
        }
        Err(e) => return Err(io_err("cannot open", &path)(e.into())),
    };
    let f = fs::File::from(fd);
    let meta = f.metadata().map_err(io_err("cannot read", &path))?;
    if !meta.is_file() {
        return Err(not_found());
    }
    Ok((
        f,
        Backup {
            database: database_of(file),
            format,
            size: meta.len(),
            modified_at: meta.modified().map_or(0, unix_ms),
            file: file.to_owned(),
        },
    ))
}

/// Deletes backup `file` of `dir` (a plain file only).
///
/// # Errors
/// [`BackupError::InvalidFile`], [`BackupError::FileNotFound`] or
/// [`BackupError::Io`].
pub fn delete_backup(dir: &Path, file: &str) -> Result<(), BackupError> {
    validate_backup_name(file)?;
    let not_found = || BackupError::FileNotFound(file.to_owned());
    if !dir.symlink_metadata().is_ok_and(|m| m.is_dir()) {
        return Err(not_found());
    }
    let path = dir.join(file);
    match path.symlink_metadata() {
        Ok(m) if m.is_file() => fs::remove_file(&path).map_err(io_err("cannot delete", &path)),
        _ => Err(not_found()),
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn file_names_carry_the_database_and_utc_time() {
        // 2026-11-26 14:03:09 UTC
        let name = backup_file_name("shop.main", BackupFormat::Zip, 1_795_701_789);
        assert_eq!(name, "shop.main_20261126-140309.zip");
        assert_eq!(database_of(&name).as_deref(), Some("shop.main"));
        assert_eq!(
            backup_file_name("a_b", BackupFormat::Dump, 0),
            "a_b_19700101-000000.dump"
        );
        assert_eq!(
            backup_file_name("x", BackupFormat::Dump, 951_782_400),
            "x_20000229-000000.dump"
        );
        assert_eq!(
            database_of("a_b_19700101-000000.dump").as_deref(),
            Some("a_b")
        );
        assert_eq!(database_of("mine.zip"), None);
        assert_eq!(database_of("x_2026112-140309.zip"), None);
        for name in ["shop_20261126-140309.zip", "a_19700101-000000.dump"] {
            assert!(validate_backup_name(name).is_ok());
        }
    }

    #[test]
    fn backup_names_cannot_be_paths_or_hidden() {
        assert_eq!(validate_backup_name("a.zip").unwrap(), BackupFormat::Zip);
        assert_eq!(
            validate_backup_name("a.b.dump").unwrap(),
            BackupFormat::Dump
        );
        for bad in [
            "",
            ".zip",
            "..",
            "../x.zip",
            "a/b.zip",
            "/etc/passwd",
            ".hidden.zip",
            "-x.zip",
            "a b.zip",
            "a\0.zip",
            "a.ZIP",
            "a.sql",
            "a.zip/",
            "a;rm.zip",
            "a\\b.zip",
        ] {
            assert!(validate_backup_name(bad).is_err(), "{bad:?} accepted");
        }
        assert!(validate_backup_name(&format!("{}.zip", "a".repeat(252))).is_err());
        assert!(validate_backup_name(&format!("{}.zip", "a".repeat(251))).is_ok());
    }

    #[test]
    fn tool_arguments_put_the_database_last_and_name_no_secret() {
        let req = BackupRequest {
            database: "shop".into(),
            format: BackupFormat::Zip,
        };
        assert_eq!(
            req.odoo_args(Path::new("/p/odoo.conf"), Path::new("/p/backups/s/x.zip")),
            [
                "db",
                "-c",
                "/p/odoo.conf",
                "dump",
                "--format",
                "zip",
                "shop",
                "/p/backups/s/x.zip"
            ]
        );
        assert_eq!(
            req.pg_dump_args(Path::new("/p/b/x.dump")),
            [
                "--no-owner",
                "--format=custom",
                "--file=/p/b/x.dump",
                "--dbname=shop"
            ]
        );
    }

    #[test]
    fn listing_keeps_plain_backup_files_newest_first() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("backups");
        assert!(list_backups(&dir).unwrap().is_empty());
        fs::create_dir(&dir).unwrap();
        let old = dir.join("shop_20250101-000000.zip");
        fs::write(&old, b"old").unwrap();
        let f = fs::File::options().write(true).open(&old).unwrap();
        f.set_modified(UNIX_EPOCH + Duration::from_secs(1_000))
            .unwrap();
        fs::write(dir.join("mine.dump"), b"newer").unwrap();
        fs::write(dir.join("notes.txt"), b"x").unwrap();
        fs::create_dir(dir.join(".odhive-backup-x")).unwrap();
        fs::create_dir(dir.join("folder.zip")).unwrap();
        fs::write(tmp.path().join("secret"), b"s").unwrap();
        symlink(tmp.path().join("secret"), dir.join("link.zip")).unwrap();
        let got = list_backups(&dir).unwrap();
        let names: Vec<_> = got.iter().map(|b| b.file.as_str()).collect();
        assert_eq!(names, ["mine.dump", "shop_20250101-000000.zip"]);
        assert_eq!(got[0].database, None);
        assert_eq!(got[0].format, BackupFormat::Dump);
        assert_eq!(got[0].size, 5);
        assert_eq!(got[1].database.as_deref(), Some("shop"));
        assert_eq!(got[1].modified_at, 1_000_000);

        // A `backups` link to another folder lists nothing.
        let elsewhere = tmp.path().join("linked");
        symlink(&dir, &elsewhere).unwrap();
        assert!(list_backups(&elsewhere).unwrap().is_empty());
    }

    #[test]
    fn opening_and_deleting_refuse_links_and_other_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("backups");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("a.zip"), b"zip").unwrap();
        fs::write(tmp.path().join("secret"), b"s").unwrap();
        symlink(tmp.path().join("secret"), dir.join("link.zip")).unwrap();
        fs::create_dir(dir.join("folder.zip")).unwrap();
        // rustix has no mkfifoat on Apple targets; there `fifo.zip` is simply
        // missing (still not found).
        #[cfg(not(target_vendor = "apple"))]
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            dir.join("fifo.zip"),
            rustix::fs::Mode::from_raw_mode(0o600),
        )
        .unwrap();

        let (_, b) = open_backup(&dir, "a.zip").unwrap();
        assert_eq!((b.size, b.format), (3, BackupFormat::Zip));
        for name in ["link.zip", "folder.zip", "fifo.zip", "none.zip"] {
            assert!(
                matches!(open_backup(&dir, name), Err(BackupError::FileNotFound(_))),
                "{name} opened"
            );
            assert!(
                matches!(delete_backup(&dir, name), Err(BackupError::FileNotFound(_))),
                "{name} deleted"
            );
        }
        for name in ["../secret", "secret", ".odhive-backup-x"] {
            assert!(matches!(
                open_backup(&dir, name),
                Err(BackupError::InvalidFile(_))
            ));
            assert!(matches!(
                delete_backup(&dir, name),
                Err(BackupError::InvalidFile(_))
            ));
        }
        let linked = tmp.path().join("linked");
        symlink(&dir, &linked).unwrap();
        assert!(matches!(
            open_backup(&linked, "a.zip"),
            Err(BackupError::FileNotFound(_))
        ));
        assert!(fs::read(tmp.path().join("secret")).is_ok());
        delete_backup(&dir, "a.zip").unwrap();
        assert!(!dir.join("a.zip").exists());
        assert!(dir.join("link.zip").symlink_metadata().is_ok());
    }
}
