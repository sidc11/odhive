//! Platform checks that guard process startup.

use thiserror::Error;

/// Errors returned by platform checks.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PlatformError {
    /// OdHive was started with an effective user id of 0.
    #[error("refusing to run as root: start OdHive as a regular user")]
    RunningAsRoot,
}

/// Returns the effective user id of the current process.
#[must_use]
pub fn effective_uid() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// Fails if `euid` is the root user. Takes the id as an argument so it can be tested.
///
/// # Errors
/// Returns [`PlatformError::RunningAsRoot`] when `euid` is `0`.
pub fn ensure_not_root(euid: u32) -> Result<(), PlatformError> {
    if euid == 0 {
        Err(PlatformError::RunningAsRoot)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_is_rejected() {
        assert_eq!(ensure_not_root(0), Err(PlatformError::RunningAsRoot));
    }

    #[test]
    fn regular_user_is_allowed() {
        assert_eq!(ensure_not_root(1000), Ok(()));
        assert_eq!(ensure_not_root(501), Ok(()));
    }

    #[test]
    fn effective_uid_matches_this_process() {
        // Sanity: the test process itself is never expected to run as root in CI.
        let uid = effective_uid();
        assert_eq!(ensure_not_root(uid).is_ok(), uid != 0);
    }
}
