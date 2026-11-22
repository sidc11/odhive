//! Single-instance lock of the OdHive background service.
//!
//! The service holds an exclusive `flock` on `<data dir>/service.lock` for as
//! long as it runs; the kernel drops the lock when the process ends, however
//! it ends, so a crash never leaves a stale lock behind. While it holds the
//! lock the service publishes where it listens in `<data dir>/service.json`
//! (mode `0600`), which the launcher and the CLI read to reach it.
//!
//! The state file is only trusted while the lock is held: [`probe`] checks
//! the lock first and reads the file afterwards.

use std::fs::File;
use std::io;
use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};

use rustix::fs::{FlockOperation, Mode, OFlags};
use serde::{Deserialize, Serialize};

use crate::fsutil::{self, DIR_MODE, FILE_MODE};

/// Name of the lock file inside the data directory.
pub const LOCK_FILE_NAME: &str = "service.lock";

/// Name of the state file inside the data directory.
pub const STATE_FILE_NAME: &str = "service.json";

/// Name of the background service's log file inside the data directory.
pub const LOG_FILE_NAME: &str = "service.log";

/// Largest state file that is read.
const MAX_STATE_LEN: u64 = 4096;

/// Errors taking the lock or reading and writing the state file.
#[derive(Debug, thiserror::Error)]
pub enum ServiceLockError {
    /// Opening, locking, reading or writing failed.
    #[error("{}: {source}", path.display())]
    Io {
        /// File involved.
        path: PathBuf,
        /// Underlying error.
        source: io::Error,
    },
    /// The state file does not hold valid JSON.
    #[error("{}: not an OdHive service state file: {reason}", path.display())]
    Invalid {
        /// The state file.
        path: PathBuf,
        /// What is wrong.
        reason: String,
    },
}

/// Where a running service can be reached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceState {
    /// Process id of the service.
    pub pid: u32,
    /// Port of its UI and API on the loopback interface.
    pub port: u16,
    /// OdHive version of the service.
    pub version: String,
}

/// What [`ServiceLock::acquire`] found.
#[derive(Debug)]
pub enum Acquired {
    /// The lock is ours until it is dropped.
    Locked(ServiceLock),
    /// Another process holds it. Its state is `None` while it starts up (it
    /// publishes the state only once it listens).
    Running(Option<ServiceState>),
}

/// The held single-instance lock. Dropping it removes the state file, then
/// releases the lock.
#[derive(Debug)]
pub struct ServiceLock {
    _file: File,
    state_path: PathBuf,
    published: bool,
}

impl ServiceLock {
    /// Takes the lock in `data_dir` without waiting.
    ///
    /// # Errors
    /// I/O errors, a lock path that is a symbolic link, or an unreadable
    /// state file of the running service.
    pub fn acquire(data_dir: &Path) -> Result<Acquired, ServiceLockError> {
        let file = open_lock(data_dir)?;
        if try_lock(&file, &data_dir.join(LOCK_FILE_NAME))? {
            Ok(Acquired::Locked(Self {
                _file: file,
                state_path: data_dir.join(STATE_FILE_NAME),
                published: false,
            }))
        } else {
            Ok(Acquired::Running(read_state(data_dir)?))
        }
    }

    /// Writes `state` for clients to find (atomically, mode `0600`).
    ///
    /// # Errors
    /// I/O errors writing the file.
    pub fn publish(&mut self, state: &ServiceState) -> Result<(), ServiceLockError> {
        let json = serde_json::to_vec_pretty(state).map_err(|e| ServiceLockError::Invalid {
            path: self.state_path.clone(),
            reason: e.to_string(),
        })?;
        fsutil::write_private_atomic(&self.state_path, &json).map_err(|e| {
            ServiceLockError::Io {
                path: e.path,
                source: e.source,
            }
        })?;
        self.published = true;
        Ok(())
    }
}

impl Drop for ServiceLock {
    fn drop(&mut self) {
        // Removed while the lock is still held: a new service can't have
        // written its own state yet.
        if self.published {
            let _ = std::fs::remove_file(&self.state_path);
        }
    }
}

/// Whether a service holds the lock in `data_dir`, and its state.
/// `None` = no service runs. A state file left by a service that is gone is
/// ignored.
///
/// # Errors
/// I/O errors, or an unreadable state file of the running service.
pub fn probe(data_dir: &Path) -> Result<Option<Option<ServiceState>>, ServiceLockError> {
    let lock_path = data_dir.join(LOCK_FILE_NAME);
    match std::fs::symlink_metadata(&lock_path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(ServiceLockError::Io {
                path: lock_path,
                source: e,
            });
        }
        Ok(_) => {}
    }
    match ServiceLock::acquire(data_dir)? {
        // Ours for a moment: nobody runs. Dropped right away (nothing published).
        Acquired::Locked(_) => Ok(None),
        Acquired::Running(state) => Ok(Some(state)),
    }
}

/// Creates (or empties) the background service's log file in `data_dir`,
/// mode `0600`; a symbolic link there is refused.
///
/// # Errors
/// I/O errors.
pub fn create_log(data_dir: &Path) -> Result<File, ServiceLockError> {
    make_dir(data_dir)?;
    let path = data_dir.join(LOG_FILE_NAME);
    let fd = rustix::fs::open(
        &path,
        OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(FILE_MODE as _),
    )
    .map_err(|e| ServiceLockError::Io {
        path,
        source: e.into(),
    })?;
    Ok(File::from(fd))
}

fn make_dir(dir: &Path) -> Result<(), ServiceLockError> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(DIR_MODE)
        .create(dir)
        .map_err(|source| ServiceLockError::Io {
            path: dir.to_path_buf(),
            source,
        })
}

fn open_lock(data_dir: &Path) -> Result<File, ServiceLockError> {
    let path = data_dir.join(LOCK_FILE_NAME);
    make_dir(data_dir)?;
    let fd = rustix::fs::open(
        &path,
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(FILE_MODE as _),
    )
    .map_err(|e| ServiceLockError::Io {
        path,
        source: e.into(),
    })?;
    Ok(File::from(fd))
}

fn try_lock(file: &File, path: &Path) -> Result<bool, ServiceLockError> {
    match rustix::fs::flock(file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(true),
        Err(rustix::io::Errno::WOULDBLOCK) => Ok(false),
        Err(e) => Err(ServiceLockError::Io {
            path: path.to_path_buf(),
            source: e.into(),
        }),
    }
}

fn read_state(data_dir: &Path) -> Result<Option<ServiceState>, ServiceLockError> {
    let path = data_dir.join(STATE_FILE_NAME);
    let Some(text) =
        fsutil::read_private(&path, MAX_STATE_LEN).map_err(|e| ServiceLockError::Io {
            path: e.path,
            source: e.source,
        })?
    else {
        return Ok(None);
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| ServiceLockError::Invalid {
            path,
            reason: e.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(port: u16) -> ServiceState {
        ServiceState {
            pid: std::process::id(),
            port,
            version: "0.1.0".into(),
        }
    }

    #[test]
    fn only_one_holder_at_a_time() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data = dir.path().join("data");
        assert!(probe(&data).expect("probe").is_none(), "no lock file yet");

        let Acquired::Locked(mut lock) = ServiceLock::acquire(&data).expect("acquire") else {
            panic!("free lock not taken");
        };
        // flock locks belong to the open file: a second open in the same
        // process conflicts just like another process would.
        assert!(matches!(
            ServiceLock::acquire(&data).expect("again"),
            Acquired::Running(None)
        ));
        assert_eq!(probe(&data).expect("probe"), Some(None), "starting up");

        lock.publish(&state(7069)).expect("publish");
        assert_eq!(probe(&data).expect("probe"), Some(Some(state(7069))));
        let mode = std::os::unix::fs::PermissionsExt::mode(
            &std::fs::metadata(data.join(STATE_FILE_NAME))
                .expect("meta")
                .permissions(),
        );
        assert_eq!(mode & 0o777, 0o600);

        drop(lock);
        assert!(!data.join(STATE_FILE_NAME).exists(), "state removed");
        assert!(probe(&data).expect("probe").is_none(), "released");
        assert!(matches!(
            ServiceLock::acquire(&data).expect("retake"),
            Acquired::Locked(_)
        ));
    }

    #[test]
    fn a_state_file_without_a_lock_holder_is_ignored() {
        let dir = tempfile::tempdir().expect("tempdir");
        fsutil::write_private_atomic(
            &dir.path().join(STATE_FILE_NAME),
            br#"{"pid":1,"port":1,"version":"x"}"#,
        )
        .expect("write");
        std::fs::write(dir.path().join(LOCK_FILE_NAME), b"").expect("lock file");
        assert!(probe(dir.path()).expect("probe").is_none());
    }

    #[test]
    fn a_symlinked_lock_file_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("elsewhere");
        std::fs::write(&target, b"").expect("write");
        std::os::unix::fs::symlink(&target, dir.path().join(LOCK_FILE_NAME)).expect("symlink");
        assert!(matches!(
            ServiceLock::acquire(dir.path()),
            Err(ServiceLockError::Io { .. })
        ));
    }

    #[test]
    fn the_log_file_is_private_and_emptied() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data = dir.path().join("data");
        std::io::Write::write_all(&mut create_log(&data).expect("create"), b"old").expect("w");
        drop(create_log(&data).expect("again"));
        let meta = std::fs::metadata(data.join(LOG_FILE_NAME)).expect("meta");
        assert_eq!(meta.len(), 0);
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o777,
            0o600
        );
    }

    #[test]
    fn a_broken_state_file_of_a_running_service_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let Acquired::Locked(_lock) = ServiceLock::acquire(dir.path()).expect("acquire") else {
            panic!("not taken");
        };
        fsutil::write_private_atomic(&dir.path().join(STATE_FILE_NAME), b"{").expect("write");
        assert!(matches!(
            probe(dir.path()),
            Err(ServiceLockError::Invalid { .. })
        ));
    }
}
