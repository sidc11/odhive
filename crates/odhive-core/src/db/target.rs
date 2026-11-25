//! Where and as whom to connect to PostgreSQL.
//!
//! Odoo passes `db_host`, `db_port`, `db_user`, `db_password` and
//! `db_sslmode` from its config to libpq when they are set; libpq falls back
//! to `PGHOST`, `PGPORT`, `PGUSER` and `PGPASSWORD`, then to the Unix socket
//! and the OS user. [`PgTarget::for_project`] follows the same order.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::conf::{MASK, OdooOptions};
use crate::project::EnvVars;

/// Socket folders libpq builds use: Debian, Arch, Fedora (`/run/postgresql`,
/// also as `/var/run/postgresql`) and Homebrew / upstream (`/tmp`).
pub const SOCKET_DIRS: [&str; 3] = ["/run/postgresql", "/var/run/postgresql", "/tmp"];

/// Odoo's `db_sslmode`. Without a TLS stack OdHive connects in plain text,
/// so the modes that demand TLS are refused rather than downgraded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SslMode {
    /// Never TLS.
    Disable,
    /// TLS only if the server insists.
    Allow,
    /// TLS if the server offers it (Odoo's default).
    #[default]
    Prefer,
    /// TLS or no connection.
    Require,
    /// TLS with a verified CA.
    VerifyCa,
    /// TLS with a verified CA and host name.
    VerifyFull,
}

impl SslMode {
    /// The mode named `s` (libpq's spelling), if known.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "disable" => Self::Disable,
            "allow" => Self::Allow,
            "prefer" => Self::Prefer,
            "require" => Self::Require,
            "verify-ca" => Self::VerifyCa,
            "verify-full" => Self::VerifyFull,
            _ => return None,
        })
    }

    /// libpq's spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disable => "disable",
            Self::Allow => "allow",
            Self::Prefer => "prefer",
            Self::Require => "require",
            Self::VerifyCa => "verify-ca",
            Self::VerifyFull => "verify-full",
        }
    }

    /// Whether the connection must use TLS.
    #[must_use]
    pub fn requires_tls(self) -> bool {
        matches!(self, Self::Require | Self::VerifyCa | Self::VerifyFull)
    }
}

/// Where and as whom to connect. The password is masked in `Debug`.
#[derive(Clone, PartialEq, Eq)]
pub struct PgTarget {
    /// Folders to look for the server's Unix socket in, tried first.
    pub socket_dirs: Vec<PathBuf>,
    /// TCP host tried after the sockets; `None` = sockets only.
    pub host: Option<String>,
    /// Server port (also names the socket file).
    pub port: u16,
    /// Role to connect as; `None` = the OS user.
    pub user: Option<String>,
    /// Password sent when the server asks for one; `None` = none sent.
    pub password: Option<String>,
    /// TLS policy.
    pub ssl_mode: SslMode,
}

impl fmt::Debug for PgTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgTarget")
            .field("socket_dirs", &self.socket_dirs)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("password", &self.password.as_ref().map(|_| MASK))
            .field("ssl_mode", &self.ssl_mode)
            .finish()
    }
}

impl PgTarget {
    /// Default port.
    pub const DEFAULT_PORT: u16 = 5432;

    /// Nowhere: no socket folder, no host. Connecting fails at once.
    #[must_use]
    pub fn nowhere() -> Self {
        Self {
            socket_dirs: Vec::new(),
            host: None,
            port: Self::DEFAULT_PORT,
            user: None,
            password: None,
            ssl_mode: SslMode::Prefer,
        }
    }

    /// libpq's defaults, overridden by `PGHOST`, `PGPORT` and `PGUSER` like
    /// Odoo's own connections would be. Never carries a password.
    #[must_use]
    pub fn from_env() -> Self {
        let var = |k| std::env::var_os(k).filter(|v: &OsString| !v.is_empty());
        Self::from_vars(var("PGHOST"), var("PGPORT"), var("PGUSER"))
    }

    /// What a project's Odoo connects to: its Odoo options, then its `[env]`,
    /// then this process's environment, then libpq's defaults.
    #[must_use]
    pub fn for_project(options: &OdooOptions, env: &EnvVars) -> Self {
        Self::for_project_with(options, env, |k| std::env::var_os(k))
    }

    fn for_project_with(
        options: &OdooOptions,
        env: &EnvVars,
        process: impl Fn(&str) -> Option<OsString>,
    ) -> Self {
        let var = |key: &str, env_key: &str| -> Option<OsString> {
            conf_value(options, key)
                .map(OsString::from)
                .or_else(|| env.0.get(env_key).map(OsString::from))
                .or_else(|| process(env_key))
                .filter(|v| !v.is_empty())
        };
        let mut t = Self::from_vars(
            var("db_host", "PGHOST"),
            var("db_port", "PGPORT"),
            var("db_user", "PGUSER"),
        );
        t.password = var("db_password", "PGPASSWORD").and_then(|p| p.into_string().ok());
        // Odoo always passes db_sslmode (default `prefer`), so PGSSLMODE has no say.
        t.ssl_mode = conf_value(options, "db_sslmode")
            .and_then(SslMode::parse)
            .unwrap_or_default();
        t
    }

    pub(crate) fn from_vars(
        host: Option<OsString>,
        port: Option<OsString>,
        user: Option<OsString>,
    ) -> Self {
        let port = port
            .and_then(|p| p.to_str().and_then(|p| p.parse().ok()))
            .filter(|p| *p != 0)
            .unwrap_or(Self::DEFAULT_PORT);
        let user = user.and_then(|u| u.into_string().ok());
        let (socket_dirs, host) = match host {
            Some(h) if Path::new(&h).is_absolute() => (vec![PathBuf::from(h)], None),
            Some(h) => (Vec::new(), h.into_string().ok()),
            None => (
                SOCKET_DIRS.iter().map(PathBuf::from).collect(),
                Some("localhost".to_owned()),
            ),
        };
        Self {
            socket_dirs,
            host,
            port,
            user,
            password: None,
            ssl_mode: SslMode::Prefer,
        }
    }

    /// The socket file in `dir`, e.g. `/run/postgresql/.s.PGSQL.5432`.
    pub(crate) fn socket(&self, dir: &Path) -> PathBuf {
        dir.join(format!(".s.PGSQL.{}", self.port))
    }

    /// The role name shown to the user.
    pub(crate) fn user_label(&self) -> String {
        self.user
            .clone()
            .or_else(|| std::env::var("USER").ok())
            .or_else(|| std::env::var("LOGNAME").ok())
            .unwrap_or_else(|| "your user".to_owned())
    }

    /// The settings, without the password.
    #[must_use]
    pub fn settings(&self) -> ConnectionSettings {
        ConnectionSettings {
            socket_dirs: self
                .socket_dirs
                .iter()
                .map(|d| d.display().to_string())
                .collect(),
            host: self.host.clone(),
            port: self.port,
            user: self.user_label(),
            password_set: self.password.is_some(),
            ssl_mode: self.ssl_mode,
        }
    }
}

/// `key` from the Odoo options, unless unset (empty or `False`, which Odoo
/// reads as unset for these options).
fn conf_value<'a>(options: &'a OdooOptions, key: &str) -> Option<&'a str> {
    options
        .get(key)
        .map(str::trim)
        .filter(|v| !v.is_empty() && *v != "False")
}

/// A [`PgTarget`] as shown to the user: never the password itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct ConnectionSettings {
    /// Socket folders tried first (empty when `db_host` names a TCP host).
    pub socket_dirs: Vec<String>,
    /// TCP host tried after the sockets.
    pub host: Option<String>,
    /// Server port.
    pub port: u16,
    /// Role connected as.
    pub user: String,
    /// Whether a password is set (`db_password` or `PGPASSWORD`).
    pub password_set: bool,
    /// `db_sslmode`.
    pub ssl_mode: SslMode,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(pairs: &[(&str, &str)]) -> OdooOptions {
        OdooOptions(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        )
    }

    fn env(pairs: &[(&str, &str)]) -> EnvVars {
        EnvVars(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        )
    }

    fn none(_: &str) -> Option<OsString> {
        None
    }

    #[test]
    fn target_follows_libpq_variables() {
        let d = PgTarget::from_vars(None, None, None);
        assert_eq!(d.port, 5432);
        assert_eq!(d.host.as_deref(), Some("localhost"));
        assert_eq!(d.socket_dirs.len(), 3);
        assert_eq!(d.user, None);

        let s = PgTarget::from_vars(
            Some("/srv/pg".into()),
            Some("5433".into()),
            Some("odoo".into()),
        );
        assert_eq!(s.socket_dirs, [PathBuf::from("/srv/pg")]);
        assert_eq!(s.host, None);
        assert_eq!(s.port, 5433);
        assert_eq!(s.user.as_deref(), Some("odoo"));
        assert_eq!(
            s.socket(Path::new("/srv/pg")),
            Path::new("/srv/pg/.s.PGSQL.5433")
        );

        let t = PgTarget::from_vars(Some("db.local".into()), Some("nope".into()), None);
        assert!(t.socket_dirs.is_empty());
        assert_eq!(t.host.as_deref(), Some("db.local"));
        assert_eq!(t.port, 5432);
        assert_eq!(PgTarget::from_vars(None, Some("0".into()), None).port, 5432);
    }

    #[test]
    fn project_options_win_over_env_then_process() {
        let process = |k: &str| match k {
            "PGHOST" => Some(OsString::from("proc.host")),
            "PGUSER" => Some(OsString::from("proc_user")),
            "PGPASSWORD" => Some(OsString::from("proc-secret")),
            "PGPORT" => Some(OsString::from("6000")),
            _ => None,
        };
        let o = opts(&[
            ("db_host", "conf.host"),
            ("db_password", "conf-secret"),
            ("db_sslmode", "disable"),
        ]);
        let e = env(&[("PGUSER", "env_user"), ("PGPORT", "5999")]);
        let t = PgTarget::for_project_with(&o, &e, process);
        assert_eq!(t.host.as_deref(), Some("conf.host"));
        assert!(t.socket_dirs.is_empty());
        assert_eq!(t.user.as_deref(), Some("env_user"));
        assert_eq!(t.port, 5999);
        assert_eq!(t.password.as_deref(), Some("conf-secret"));
        assert_eq!(t.ssl_mode, SslMode::Disable);

        let t = PgTarget::for_project_with(&OdooOptions::default(), &EnvVars::default(), process);
        assert_eq!(t.host.as_deref(), Some("proc.host"));
        assert_eq!(t.user.as_deref(), Some("proc_user"));
        assert_eq!(t.password.as_deref(), Some("proc-secret"));
        assert_eq!(t.port, 6000);
    }

    #[test]
    fn false_and_empty_options_are_unset() {
        let o = opts(&[
            ("db_host", "False"),
            ("db_port", "False"),
            ("db_user", ""),
            ("db_password", "False"),
            ("db_sslmode", "bogus"),
        ]);
        let t = PgTarget::for_project_with(&o, &EnvVars::default(), none);
        assert_eq!(t, PgTarget::from_vars(None, None, None));
        assert_eq!(t.ssl_mode, SslMode::Prefer);
    }

    #[test]
    fn socket_dir_option_and_port() {
        let o = opts(&[
            ("db_host", "/srv/pg"),
            ("db_port", "5434"),
            ("db_user", "odoo"),
        ]);
        let t = PgTarget::for_project_with(&o, &EnvVars::default(), none);
        assert_eq!(t.socket_dirs, [PathBuf::from("/srv/pg")]);
        assert_eq!(t.host, None);
        assert_eq!(t.port, 5434);
        assert_eq!(t.password, None);
    }

    #[test]
    fn password_never_shown() {
        let o = opts(&[("db_password", "hunter2"), ("db_user", "odoo")]);
        let t = PgTarget::for_project_with(&o, &EnvVars::default(), none);
        assert!(!format!("{t:?}").contains("hunter2"));
        let s = t.settings();
        assert!(s.password_set);
        assert!(!serde_json::to_string(&s).unwrap().contains("hunter2"));
        assert!(!PgTarget::nowhere().settings().password_set);
    }

    #[test]
    fn ssl_modes() {
        for m in [
            SslMode::Disable,
            SslMode::Allow,
            SslMode::Prefer,
            SslMode::Require,
            SslMode::VerifyCa,
            SslMode::VerifyFull,
        ] {
            assert_eq!(SslMode::parse(m.as_str()), Some(m));
            assert_eq!(
                serde_json::to_value(m).unwrap(),
                serde_json::Value::from(m.as_str())
            );
        }
        assert!(SslMode::Require.requires_tls());
        assert!(!SslMode::Prefer.requires_tls());
        assert_eq!(SslMode::parse("PREFER"), None);
    }
}
