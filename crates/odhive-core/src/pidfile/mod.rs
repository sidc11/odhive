//! PID files: one record per running instance in the app state dir, so a new
//! OdHive process can find instances an earlier one left behind.
//!
//! Instances normally die with OdHive: the supervisor kills their process
//! group when its handle is dropped, and removes the record. A record outlives
//! its OdHive only when OdHive itself was killed hard (SIGKILL, OOM, power
//! loss). [`recover`] checks every record at startup:
//!
//! - **Running:** the process is still alive and is the one recorded (same pid
//!   *and* start time, so a reused pid never matches). Its ports are reserved
//!   again and an [`Attached`] handle can stop it. Its output went to pipes
//!   that closed with the old OdHive, so there are no logs for it.
//! - **Supervised:** the OdHive process that started it is still alive (two
//!   OdHive processes at once). The record is left alone.
//! - **Stale:** the process is gone, or its pid now belongs to another
//!   process. The record is removed. If the leader is gone but members of its
//!   group are not (workers, report renderers), they are killed.
//! - **Invalid:** the file cannot be read or fails validation. It is removed.
//!
//! Records are JSON, written atomically with mode `0600` in a `0700` folder,
//! named `<project>.<instance>.json`. Project names and instance ids never
//! contain `.`, so the name is unambiguous.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rustix::process::{Pid, Signal, kill_process_group, test_kill_process_group};
use serde::{Deserialize, Serialize};
use sysinfo::{ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System};
use thiserror::Error;
use tokio::time::Instant;

use crate::fsutil::{self, PathError};
use crate::names;
use crate::ports::{AllocError, PortOwner, PortRegistry, Reservation};
use crate::project::MIN_PORT;

#[cfg(test)]
mod tests;

/// Name of the folder holding the records, inside the app data dir.
pub const RUN_DIR_NAME: &str = "run";

/// Record format written by this build. Newer records are reported invalid.
pub const RECORD_VERSION: u32 = 1;

/// Records larger than this are rejected.
pub const MAX_RECORD_BYTES: u64 = 16 * 1024;

/// Extension of record files.
const EXTENSION: &str = "json";

/// How often an attached process is checked while it stops.
const STOP_POLL: Duration = Duration::from_millis(50);

/// How long a SIGKILLed process may take to disappear.
const KILL_WAIT: Duration = Duration::from_secs(5);

/// Errors from reading or writing records.
#[derive(Debug, Error)]
pub enum PidFileError {
    /// An I/O operation failed.
    #[error("{}: {source}", path.display())]
    Io {
        /// Path involved.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
    /// The record is not valid.
    #[error("invalid pid record: {0}")]
    Invalid(String),
    /// The record could not be serialized.
    #[error("could not serialize pid record: {0}")]
    Serialize(#[from] serde_json::Error),
}

impl From<PathError> for PidFileError {
    fn from(e: PathError) -> Self {
        Self::Io {
            path: e.path,
            source: e.source,
        }
    }
}

/// A process, identified by its pid and its start time. Pids are reused; the
/// pair is not, so a record never matches a newer process with the same pid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessId {
    /// Process id.
    pub pid: u32,
    /// Start time in seconds since the Unix epoch, as the OS reports it.
    pub start_time: u64,
}

impl ProcessId {
    /// The process with this pid, if one exists. Zombies are included.
    #[must_use]
    pub fn of(pid: u32) -> Option<Self> {
        lookup(pid).flatten().map(|(id, _)| id)
    }

    /// The current process.
    #[must_use]
    pub fn current() -> Option<Self> {
        Self::of(std::process::id())
    }

    /// Whether this exact process still runs (exists and is not a zombie).
    #[must_use]
    pub fn is_alive(&self) -> bool {
        lookup(self.pid)
            .flatten()
            .is_some_and(|(id, zombie)| id == *self && !zombie)
    }

    /// Whether the process group led by this process may be signalled: no
    /// other process holds its pid. That is true while the leader runs or is a
    /// zombie, and after it is gone, because a process group id is never
    /// handed out again while the group has members. False when the process
    /// table cannot be read.
    fn owns_group(&self) -> bool {
        lookup(self.pid).is_some_and(|found| found.is_none_or(|(id, _)| id == *self))
    }

    fn rustix_pid(&self) -> Option<Pid> {
        i32::try_from(self.pid).ok().and_then(Pid::from_raw)
    }
}

/// Looks up `pid`: its identity and whether it is a zombie, `Some(None)` if
/// there is no such process, `None` if the process table cannot be read.
fn lookup(pid: u32) -> Option<Option<(ProcessId, bool)>> {
    let spid = sysinfo::Pid::from_u32(pid);
    let mut sys = System::new().ok()?;
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[spid]),
        true,
        ProcessRefreshKind::nothing(),
    );
    Some(sys.process(spid).map(|p| {
        (
            ProcessId {
                pid,
                start_time: p.start_time(),
            },
            matches!(p.status(), ProcessStatus::Zombie | ProcessStatus::Dead),
        )
    }))
}

/// What is known about one running instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PidRecord {
    /// Format version, see [`RECORD_VERSION`].
    pub version: u32,
    /// Project name.
    pub project: String,
    /// Instance id.
    pub instance: String,
    /// The `odoo-bin` process; it leads its own process group.
    pub process: ProcessId,
    /// HTTP port.
    pub http_port: u16,
    /// Gevent port.
    pub gevent_port: u16,
    /// Database, if one was passed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub db: Option<String>,
    /// The `odoo-bin` script that runs.
    pub odoo_bin: PathBuf,
    /// The OdHive process supervising it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervisor: Option<ProcessId>,
}

impl PidRecord {
    /// The instance this record is about.
    #[must_use]
    pub fn owner(&self) -> PortOwner {
        PortOwner::new(&self.project, &self.instance)
    }

    /// Checks the version, names, pid and ports. A record that fails is never
    /// acted on: a pid of 0 or 1 would signal every process or init.
    ///
    /// # Errors
    /// [`PidFileError::Invalid`] naming the first problem.
    pub fn validate(&self) -> Result<(), PidFileError> {
        let bad = |m: String| Err(PidFileError::Invalid(m));
        if self.version == 0 || self.version > RECORD_VERSION {
            return bad(format!("unsupported version {}", self.version));
        }
        names::validate_project_name(&self.project)
            .and_then(|()| names::validate_instance_id(&self.instance))
            .map_err(|e| PidFileError::Invalid(e.to_string()))?;
        if let Some(db) = &self.db {
            names::validate_db_name(db).map_err(|e| PidFileError::Invalid(e.to_string()))?;
        }
        let pids = std::iter::once(self.process).chain(self.supervisor);
        for p in pids {
            if p.pid <= 1 || i32::try_from(p.pid).is_err() {
                return bad(format!("pid {} is out of range", p.pid));
            }
        }
        for port in [self.http_port, self.gevent_port] {
            if port < MIN_PORT {
                return bad(format!("port {port} is out of range"));
            }
        }
        if self.http_port == self.gevent_port {
            return bad("http and gevent ports are the same".into());
        }
        Ok(())
    }
}

/// The folder holding the records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunDir(PathBuf);

/// One file found by [`RunDir::list`].
#[derive(Debug)]
pub struct Listed {
    /// The file.
    pub path: PathBuf,
    /// Its record, or why it could not be used.
    pub record: Result<PidRecord, PidFileError>,
}

impl RunDir {
    /// Records live in `dir` (normally [`crate::config::AppPaths::run_dir`]).
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self(dir.into())
    }

    /// The folder.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// The file for `owner`.
    ///
    /// # Errors
    /// [`PidFileError::Invalid`] if the owner's names are not valid.
    pub fn file_for(&self, owner: &PortOwner) -> Result<PathBuf, PidFileError> {
        names::validate_project_name(&owner.project)
            .and_then(|()| names::validate_instance_id(&owner.instance))
            .map_err(|e| PidFileError::Invalid(e.to_string()))?;
        Ok(self
            .0
            .join(format!("{}.{}.{EXTENSION}", owner.project, owner.instance)))
    }

    /// Writes `record`, replacing any record of the same instance.
    ///
    /// # Errors
    /// Validation, serialization or I/O errors.
    pub fn write(&self, record: &PidRecord) -> Result<(), PidFileError> {
        record.validate()?;
        let path = self.file_for(&record.owner())?;
        let mut json = serde_json::to_vec_pretty(record)?;
        json.push(b'\n');
        fsutil::write_private_atomic(&path, &json)?;
        Ok(())
    }

    /// Reads the record of `owner`, if there is one.
    ///
    /// # Errors
    /// I/O errors, or [`PidFileError::Invalid`] for a bad file.
    pub fn read(&self, owner: &PortOwner) -> Result<Option<PidRecord>, PidFileError> {
        let path = self.file_for(owner)?;
        let Some(text) = fsutil::read_private(&path, MAX_RECORD_BYTES)? else {
            return Ok(None);
        };
        parse(&path, &text).map(Some)
    }

    /// Removes the record of `owner` if it still describes `process`, so a
    /// late cleanup never removes the record of a newer process. Returns
    /// whether a file was removed.
    ///
    /// # Errors
    /// I/O errors. A missing or unreadable record is left as it is.
    pub fn remove(&self, owner: &PortOwner, process: ProcessId) -> Result<bool, PidFileError> {
        match self.read(owner) {
            Ok(Some(r)) if r.process == process => self.remove_file(&self.file_for(owner)?),
            Ok(_) | Err(PidFileError::Invalid(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Every record file, valid or not. Temp files and other names are skipped.
    /// A missing folder has no records.
    ///
    /// # Errors
    /// I/O errors while reading the folder.
    pub fn list(&self) -> Result<Vec<Listed>, PidFileError> {
        let io_err = |source| PidFileError::Io {
            path: self.0.clone(),
            source,
        };
        let entries = match fs::read_dir(&self.0) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io_err(e)),
        };
        let mut found = Vec::new();
        for entry in entries {
            let path = entry.map_err(io_err)?.path();
            let is_record = path.extension().is_some_and(|e| e == EXTENSION)
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| !n.starts_with('.'));
            if !is_record {
                continue;
            }
            let record = self.read_path(&path);
            found.push(Listed { path, record });
        }
        found.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(found)
    }

    fn read_path(&self, path: &Path) -> Result<PidRecord, PidFileError> {
        if fs::symlink_metadata(path)
            .map_err(|source| PidFileError::Io {
                path: path.to_path_buf(),
                source,
            })?
            .file_type()
            .is_symlink()
        {
            return Err(PidFileError::Invalid("record is a symlink".into()));
        }
        let text =
            fsutil::read_private(path, MAX_RECORD_BYTES)?.ok_or_else(|| PidFileError::Io {
                path: path.to_path_buf(),
                source: io::ErrorKind::NotFound.into(),
            })?;
        let record = parse(path, &text)?;
        if self.file_for(&record.owner())? != path {
            return Err(PidFileError::Invalid(format!(
                "file name does not match instance {}",
                record.owner()
            )));
        }
        Ok(record)
    }

    fn remove_file(&self, path: &Path) -> Result<bool, PidFileError> {
        match fs::remove_file(path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(PidFileError::Io {
                path: path.to_path_buf(),
                source,
            }),
        }
    }
}

fn parse(path: &Path, text: &str) -> Result<PidRecord, PidFileError> {
    let record: PidRecord = serde_json::from_str(text)
        .map_err(|e| PidFileError::Invalid(format!("{}: {e}", path.display())))?;
    record.validate()?;
    Ok(record)
}

/// Removes the record when dropped, if it still describes the same process.
#[derive(Debug)]
pub(crate) struct PidGuard {
    run: RunDir,
    owner: PortOwner,
    process: ProcessId,
}

impl PidGuard {
    /// Writes `record` and returns a guard that removes it again.
    pub(crate) fn write(run: &RunDir, record: &PidRecord) -> Result<Self, PidFileError> {
        run.write(record)?;
        Ok(Self {
            run: run.clone(),
            owner: record.owner(),
            process: record.process,
        })
    }
}

impl Drop for PidGuard {
    fn drop(&mut self) {
        if let Err(e) = self.run.remove(&self.owner, self.process) {
            tracing::warn!(instance = %self.owner, error = %e, "could not remove pid file");
        }
    }
}

/// What [`recover`] found for one record.
#[derive(Debug)]
pub enum Recovered {
    /// The instance still runs; it is attached again.
    Running {
        /// Handle to it.
        instance: Attached,
        /// Its ports could not be reserved again (another record claims them).
        ports_error: Option<AllocError>,
    },
    /// Another OdHive process that is still alive supervises it. Left alone.
    Supervised {
        /// The record.
        record: PidRecord,
        /// The supervising OdHive process.
        by: ProcessId,
    },
    /// The process is gone or its pid was reused. The record was removed.
    Stale {
        /// The record.
        record: PidRecord,
        /// Members of its group were still alive and were killed.
        killed_leftovers: bool,
    },
    /// The file was unusable and was removed.
    Invalid {
        /// The file.
        path: PathBuf,
        /// Why.
        reason: String,
    },
}

/// Checks every record in `run` (see the module docs) and reserves the ports
/// of instances that still run in `registry`. Call it once at startup, before
/// starting instances.
///
/// # Errors
/// Only when the folder cannot be read. Problems with single records are
/// reported as [`Recovered::Invalid`].
pub fn recover(run: &RunDir, registry: &PortRegistry) -> Result<Vec<Recovered>, PidFileError> {
    let me = ProcessId::current();
    let mut out = Vec::new();
    for Listed { path, record } in run.list()? {
        let record = match record {
            Ok(r) => r,
            Err(e) => {
                if let Err(rm) = run.remove_file(&path) {
                    tracing::warn!(path = %path.display(), error = %rm, "could not remove pid file");
                }
                out.push(Recovered::Invalid {
                    path,
                    reason: e.to_string(),
                });
                continue;
            }
        };
        out.push(recover_one(run, registry, record, me));
    }
    Ok(out)
}

fn recover_one(
    run: &RunDir,
    registry: &PortRegistry,
    mut record: PidRecord,
    me: Option<ProcessId>,
) -> Recovered {
    if let Some(by) = record.supervisor.filter(ProcessId::is_alive) {
        return Recovered::Supervised { record, by };
    }
    if !record.process.is_alive() {
        let killed_leftovers = kill_leftovers(record.process);
        if let Err(e) = run.remove(&record.owner(), record.process) {
            tracing::warn!(instance = %record.owner(), error = %e, "could not remove pid file");
        }
        return Recovered::Stale {
            record,
            killed_leftovers,
        };
    }
    let (ports, ports_error) =
        match registry.reserve(record.owner(), record.http_port, record.gevent_port) {
            Ok(r) => (Some(r), None),
            Err(e) => (None, Some(e)),
        };
    record.supervisor = me;
    if let Err(e) = run.write(&record) {
        tracing::warn!(instance = %record.owner(), error = %e, "could not update pid file");
    }
    Recovered::Running {
        instance: Attached {
            record,
            run: run.clone(),
            ports,
        },
        ports_error,
    }
}

/// Kills what is left of the group of a leader that is gone. Returns whether
/// a group was found.
fn kill_leftovers(leader: ProcessId) -> bool {
    let Some(pgid) = leader.rustix_pid() else {
        return false;
    };
    if !leader.owns_group() || test_kill_process_group(pgid).is_err() {
        return false;
    }
    // ESRCH (emptied in the meantime) is fine.
    let _ = kill_process_group(pgid, Signal::KILL);
    true
}

/// How an attached instance was stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct AttachedStop {
    /// It was already gone.
    pub already_gone: bool,
    /// It ignored SIGINT and was killed.
    pub killed: bool,
}

/// An instance started by an earlier OdHive process that still runs. It is
/// not a child of this process, so there is no exit code and no output; it
/// can be watched and stopped. Dropping the handle leaves it running and
/// releases its ports.
#[derive(Debug)]
pub struct Attached {
    record: PidRecord,
    run: RunDir,
    ports: Option<Reservation>,
}

impl Attached {
    /// Its record.
    #[must_use]
    pub fn record(&self) -> &PidRecord {
        &self.record
    }

    /// The instance.
    #[must_use]
    pub fn owner(&self) -> PortOwner {
        self.record.owner()
    }

    /// Its port reservation, if the ports could be reserved.
    #[must_use]
    pub fn reservation(&self) -> Option<&Reservation> {
        self.ports.as_ref()
    }

    /// Whether the process still runs.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.record.process.is_alive()
    }

    /// Stops it like the supervisor does: SIGINT to the group, SIGKILL to the
    /// group after `timeout`, then kills what is left of the group. Removes
    /// the record and releases the ports.
    ///
    /// # Errors
    /// I/O errors while removing the record (the process is stopped anyway).
    pub async fn stop(self, timeout: Duration) -> Result<AttachedStop, PidFileError> {
        stop_recorded(&self.run, &self.record, timeout).await
    }
}

/// Stops the process of `record`, whoever supervises it: SIGINT to its group,
/// SIGKILL to the group after `timeout`, then kills what is left of the group
/// and removes the record. A supervising OdHive sees an ordinary exit.
///
/// Only the exact recorded process is signalled (pid *and* start time): a
/// record whose pid now belongs to another process is treated as gone.
///
/// # Errors
/// Validation errors for a bad record (nothing is signalled), or I/O errors
/// while removing the record (the process is stopped anyway).
pub async fn stop_recorded(
    run: &RunDir,
    record: &PidRecord,
    timeout: Duration,
) -> Result<AttachedStop, PidFileError> {
    record.validate()?;
    let leader = record.process;
    let mut result = AttachedStop {
        already_gone: !leader.is_alive(),
        killed: false,
    };
    if let Some(pgid) = leader.rustix_pid()
        && !result.already_gone
    {
        let _ = kill_process_group(pgid, Signal::INT);
        if !wait_dead(leader, timeout).await && leader.owns_group() {
            result.killed = true;
            let _ = kill_process_group(pgid, Signal::KILL);
            wait_dead(leader, KILL_WAIT).await;
        }
    }
    kill_leftovers(leader);
    run.remove(&record.owner(), leader)?;
    Ok(result)
}

/// Where a record's process stands, without changing anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RecordState {
    /// The recorded process runs. `supervisor` is the OdHive process that
    /// captures its output, if that one is still alive.
    Running {
        /// Process id.
        pid: u32,
        /// Supervising OdHive process, if alive.
        supervisor: Option<u32>,
    },
    /// The process is gone or its pid was reused; the record is left over.
    Stale {
        /// Process id it had.
        pid: u32,
    },
}

impl PidRecord {
    /// Checks the process table for this record. Read-only.
    #[must_use]
    pub fn state(&self) -> RecordState {
        if self.process.is_alive() {
            RecordState::Running {
                pid: self.process.pid,
                supervisor: self.supervisor.filter(ProcessId::is_alive).map(|p| p.pid),
            }
        } else {
            RecordState::Stale {
                pid: self.process.pid,
            }
        }
    }
}

/// Waits until `p` is no longer alive; false on timeout.
async fn wait_dead(p: ProcessId, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !p.is_alive() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(STOP_POLL).await;
    }
}
