//! Validation of user-chosen identifiers. Shared by every layer (core, API, UI).

use thiserror::Error;

/// Longest project name or instance id.
pub const MAX_NAME_LEN: usize = 64;
/// Longest PostgreSQL database name (`NAMEDATALEN - 1`).
pub const MAX_DB_NAME_LEN: usize = 63;

/// An identifier failed validation.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum NameError {
    /// Project name or instance id does not match `^[a-z0-9][a-z0-9_-]{0,63}$`.
    #[error(
        "invalid {kind} {value:?}: use 1-64 lowercase letters, digits, '_' or '-', starting with a letter or digit"
    )]
    Name {
        /// What was being validated.
        kind: &'static str,
        /// Rejected value.
        value: String,
    },
    /// Database name does not match `^[A-Za-z0-9_.-]{1,63}$`.
    #[error("invalid database name {0:?}: use 1-63 letters, digits, '_', '.' or '-'")]
    Database(String),
    /// Environment variable name does not match `^[A-Za-z_][A-Za-z0-9_]*$`.
    #[error("invalid environment variable name {0:?}")]
    EnvKey(String),
}

/// Validates a project name: `^[a-z0-9][a-z0-9_-]{0,63}$`.
///
/// # Errors
/// Returns [`NameError::Name`] when `name` does not match.
pub fn validate_project_name(name: &str) -> Result<(), NameError> {
    validate_slug("project name", name)
}

/// Validates an instance id (same rule as project names).
///
/// # Errors
/// Returns [`NameError::Name`] when `id` does not match.
pub fn validate_instance_id(id: &str) -> Result<(), NameError> {
    validate_slug("instance id", id)
}

/// Validates a database name: `^[A-Za-z0-9_.-]{1,63}$`. Database names are always
/// passed as arguments, never interpolated into SQL; this is defence in depth.
///
/// # Errors
/// Returns [`NameError::Database`] when `name` does not match.
pub fn validate_db_name(name: &str) -> Result<(), NameError> {
    let ok = !name.is_empty()
        && name.len() <= MAX_DB_NAME_LEN
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(NameError::Database(name.to_owned()))
    }
}

/// Validates an environment variable name: `^[A-Za-z_][A-Za-z0-9_]*$`.
///
/// # Errors
/// Returns [`NameError::EnvKey`] when `key` does not match.
pub fn validate_env_key(key: &str) -> Result<(), NameError> {
    let mut bytes = key.bytes();
    let ok = bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(NameError::EnvKey(key.to_owned()))
    }
}

fn validate_slug(kind: &'static str, value: &str) -> Result<(), NameError> {
    let mut bytes = value.bytes();
    let ok = value.len() <= MAX_NAME_LEN
        && bytes
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(NameError::Name {
            kind,
            value: value.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_names() {
        for ok in ["a", "acme", "acme-19", "my_shop", "0day", &"a".repeat(64)] {
            assert!(validate_project_name(ok).is_ok(), "{ok:?}");
        }
        for bad in [
            "",
            "-acme",
            "_acme",
            "Acme",
            "ac me",
            "acme.",
            "../etc",
            "a/b",
            "ăcme",
            "acme\n",
            &"a".repeat(65),
        ] {
            assert!(validate_project_name(bad).is_err(), "{bad:?}");
        }
        assert!(validate_instance_id("main").is_ok());
        assert!(matches!(
            validate_instance_id("Main"),
            Err(NameError::Name {
                kind: "instance id",
                ..
            })
        ));
    }

    #[test]
    fn db_names() {
        for ok in ["acme", "acme_main", "Acme-19.0", "a", &"d".repeat(63)] {
            assert!(validate_db_name(ok).is_ok(), "{ok:?}");
        }
        for bad in [
            "",
            "-acme",
            "a b",
            "a;DROP TABLE x",
            "a'b",
            "a\"b",
            "a/b",
            "a\0b",
            &"d".repeat(64),
        ] {
            assert!(validate_db_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn env_keys() {
        for ok in ["PATH", "_X", "ODOO_RC", "a1"] {
            assert!(validate_env_key(ok).is_ok(), "{ok:?}");
        }
        for bad in ["", "1A", "A-B", "A=B", "A B", "Ä"] {
            assert!(validate_env_key(bad).is_err(), "{bad:?}");
        }
    }
}
