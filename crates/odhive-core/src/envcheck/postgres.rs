//! PostgreSQL: reachable the way Odoo connects by default, server version,
//! and whether the role may create databases.
//!
//! Without `db_host`, Odoo (through libpq) connects over the Unix socket as
//! the OS user. The check tries the usual socket folders, then TCP on
//! `localhost`, and stops at the first server that answers. It never sends
//! a password: a server that asks for one is reported as such.

use std::error::Error as _;
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio_postgres::error::SqlState;
use tokio_postgres::{Client, Config, NoTls};

use super::{Check, CheckId, Fix, Requirement, Status};
use crate::python::find_executable;
use crate::syslib::PackageManager;

/// Socket folders libpq builds use: Debian, Arch, Fedora (`/run/postgresql`,
/// also as `/var/run/postgresql`) and Homebrew / upstream (`/tmp`).
const SOCKET_DIRS: [&str; 3] = ["/run/postgresql", "/var/run/postgresql", "/tmp"];

/// Database connected to; `template1` when it does not exist.
const DATABASES: [&str; 2] = ["postgres", "template1"];

/// Where and as whom to connect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgTarget {
    /// Folders to look for the server's Unix socket in, tried first.
    pub socket_dirs: Vec<PathBuf>,
    /// TCP host tried after the sockets; `None` = sockets only.
    pub host: Option<String>,
    /// Server port (also names the socket file).
    pub port: u16,
    /// Role to connect as; `None` = the OS user.
    pub user: Option<String>,
}

impl PgTarget {
    /// Default port.
    pub const DEFAULT_PORT: u16 = 5432;

    /// libpq's defaults, overridden by `PGHOST`, `PGPORT` and `PGUSER` like
    /// Odoo's own connections would be.
    #[must_use]
    pub fn from_env() -> Self {
        let var = |k| std::env::var_os(k).filter(|v: &OsString| !v.is_empty());
        Self::from_vars(var("PGHOST"), var("PGPORT"), var("PGUSER"))
    }

    fn from_vars(host: Option<OsString>, port: Option<OsString>, user: Option<OsString>) -> Self {
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
        }
    }

    /// The socket file in `dir`, e.g. `/run/postgresql/.s.PGSQL.5432`.
    fn socket(&self, dir: &Path) -> PathBuf {
        dir.join(format!(".s.PGSQL.{}", self.port))
    }

    /// The role name shown to the user.
    fn user_label(&self) -> String {
        self.user
            .clone()
            .or_else(|| std::env::var("USER").ok())
            .or_else(|| std::env::var("LOGNAME").ok())
            .unwrap_or_else(|| "your user".to_owned())
    }
}

/// One place a server may listen.
#[derive(Debug, Clone)]
enum Place {
    Socket(PathBuf),
    Tcp(String),
}

impl Place {
    fn label(&self, port: u16) -> String {
        match self {
            Self::Socket(dir) => format!("Unix socket in {}", dir.display()),
            Self::Tcp(host) => format!("{host}:{port}"),
        }
    }
}

/// What a connection attempt ended with.
enum Attempt {
    /// Nothing listens there.
    Unreachable(String),
    /// A server answered and refused or failed the login.
    Refused(Refusal),
    /// Logged in.
    Connected(Client),
}

enum Refusal {
    /// It asked for a password, which the check never sends.
    NeedsPassword,
    /// `role "x" does not exist`.
    NoRole(String),
    /// Any other login failure (`pg_hba.conf`, starting up, …).
    Other(String),
}

pub(super) async fn check(
    target: &PgTarget,
    path_var: Option<&OsStr>,
    requirements: &[Requirement],
    pm: Option<PackageManager>,
    timeout: Duration,
) -> Check {
    let mut places: Vec<Place> = Vec::new();
    let mut seen_sockets = Vec::new();
    for dir in &target.socket_dirs {
        let socket = target.socket(dir);
        // `/var/run` is usually a link to `/run`: one server, tried once.
        let Ok(real) = std::fs::canonicalize(&socket) else {
            continue;
        };
        if !seen_sockets.contains(&real) {
            seen_sockets.push(real);
            places.push(Place::Socket(dir.clone()));
        }
    }
    places.extend(target.host.iter().cloned().map(Place::Tcp));

    let mut tried = Vec::new();
    for place in &places {
        let label = place.label(target.port);
        match attempt(target, place, timeout).await {
            Attempt::Unreachable(why) => tried.push(format!("{label}: {why}")),
            Attempt::Refused(r) => return refused(target, &label, r, pm),
            Attempt::Connected(client) => {
                let mut c = connected(target, &client, requirements, pm).await;
                c.location = Some(label);
                if matches!(place, Place::Tcp(_)) && !target.socket_dirs.is_empty() {
                    c.details.push(
                        "No Unix socket was found: set db_host (and db_port) in the project's Odoo options, as Odoo uses the socket otherwise"
                            .into(),
                    );
                    c.status = c.status.max(Status::Warning);
                }
                return c;
            }
        }
    }
    unreachable_check(target, tried, server_installed(path_var), pm)
}

async fn attempt(target: &PgTarget, place: &Place, timeout: Duration) -> Attempt {
    for db in DATABASES {
        let mut cfg = Config::new();
        match place {
            Place::Socket(dir) => cfg.host_path(dir),
            Place::Tcp(host) => cfg.host(host),
        };
        cfg.port(target.port)
            .dbname(db)
            .application_name("odhive-envcheck")
            .connect_timeout(timeout);
        if let Some(user) = &target.user {
            cfg.user(user);
        }
        let result = tokio::time::timeout(timeout, cfg.connect(NoTls)).await;
        let err = match result {
            Err(_) => return Attempt::Unreachable(format!("no answer within {timeout:?}")),
            Ok(Ok((client, connection))) => {
                // Drives the connection until the client is dropped.
                tokio::spawn(async move {
                    let _ = connection.await;
                });
                return Attempt::Connected(client);
            }
            Ok(Err(e)) => e,
        };
        if let Some(db_err) = err.as_db_error() {
            let code = db_err.code();
            if *code == SqlState::INVALID_CATALOG_NAME {
                continue;
            }
            let message = db_err.message().to_owned();
            return Attempt::Refused(if *code == SqlState::INVALID_PASSWORD {
                Refusal::NeedsPassword
            } else if *code == SqlState::INVALID_AUTHORIZATION_SPECIFICATION
                && message.starts_with("role ")
                && message.ends_with(" does not exist")
            {
                Refusal::NoRole(message)
            } else {
                Refusal::Other(message)
            });
        }
        let source = err.source().map(ToString::to_string).unwrap_or_default();
        if source == "password missing" {
            return Attempt::Refused(Refusal::NeedsPassword);
        }
        if let Some(io) = err.source().and_then(|s| s.downcast_ref::<io::Error>()) {
            return Attempt::Unreachable(match io.kind() {
                io::ErrorKind::ConnectionRefused => "connection refused".into(),
                io::ErrorKind::NotFound => "no socket".into(),
                io::ErrorKind::TimedOut => format!("no answer within {timeout:?}"),
                _ => io.to_string(),
            });
        }
        return Attempt::Unreachable(err.to_string());
    }
    Attempt::Refused(Refusal::Other(
        "neither the postgres nor the template1 database exists".into(),
    ))
}

/// What the server and role look like once logged in.
struct ServerInfo {
    version_num: i32,
    version: String,
    role: String,
    create_db: bool,
    superuser: bool,
}

async fn server_info(client: &Client) -> Result<ServerInfo, tokio_postgres::Error> {
    let row = client
        .query_one(
            "SELECT current_setting('server_version_num')::int4, \
                    current_setting('server_version'), \
                    r.rolname::text, r.rolcreatedb, r.rolsuper \
             FROM pg_catalog.pg_roles r WHERE r.rolname = current_user",
            &[],
        )
        .await?;
    Ok(ServerInfo {
        version_num: row.try_get(0)?,
        version: row.try_get(1)?,
        role: row.try_get(2)?,
        create_db: row.try_get(3)?,
        superuser: row.try_get(4)?,
    })
}

async fn connected(
    target: &PgTarget,
    client: &Client,
    requirements: &[Requirement],
    pm: Option<PackageManager>,
) -> Check {
    let id = CheckId::Postgresql;
    let info = match server_info(client).await {
        Ok(i) => i,
        Err(e) => {
            let mut c = Check::new(
                id,
                Status::Warning,
                "Connected, but the server version could not be read",
            );
            c.details.push(e.to_string());
            return c;
        }
    };
    // `16.4 (Ubuntu 16.4-1.pgdg24.04+1)` → `16.4`.
    let version = info
        .version
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned();
    let major = u32::try_from(info.version_num / 10_000).unwrap_or(0);
    let too_old: Vec<&Requirement> = requirements.iter().filter(|r| major < r.min_pg).collect();

    let mut c = Check::new(
        id,
        Status::Ok,
        format!("PostgreSQL {version}, role {}", info.role),
    );
    c.version = Some(version.clone());
    let mut fix: Option<Fix> = None;
    if !too_old.is_empty() {
        c.status = if too_old.len() == requirements.len() {
            Status::Error
        } else {
            Status::Warning
        };
        let needs: Vec<String> = too_old
            .iter()
            .map(|r| format!("{} needs {}+", r.label, r.min_pg))
            .collect();
        c.summary = format!("PostgreSQL {version} is too old: {}", needs.join(", "));
        let want = too_old.iter().map(|r| r.min_pg).max().unwrap_or(16).max(17);
        fix = Some(Fix {
            text: format!("Install PostgreSQL {want} or newer and move your databases to it."),
            commands: Vec::new(),
            url: Some("https://www.postgresql.org/download/".into()),
        });
    }
    if info.superuser {
        c.details.push(format!("Role {} is a superuser", info.role));
    } else if info.create_db {
        c.details
            .push(format!("Role {} may create databases", info.role));
    } else {
        c.status = c.status.max(Status::Warning);
        c.details.push(format!(
            "Role {} may not create databases, so Odoo cannot create or restore one",
            info.role
        ));
        if fix.is_none() {
            let sql = format!("ALTER ROLE {} CREATEDB", quote_ident(&info.role));
            fix = Some(Fix {
                text: format!("Allow role {} to create databases.", info.role),
                commands: vec![as_postgres(pm, &format!("psql -c {}", shell_quote(&sql)))],
                url: None,
            });
        }
    }
    if info.role == "postgres" {
        c.status = c.status.max(Status::Warning);
        c.details.push(
            "Odoo refuses to run as the postgres role: connect as your own role (set db_user)"
                .into(),
        );
        if fix.is_none() {
            fix = Some(create_role_fix(&target.user_label(), pm));
        }
    }
    c.fix = fix;
    c
}

fn refused(target: &PgTarget, label: &str, why: Refusal, pm: Option<PackageManager>) -> Check {
    let id = CheckId::Postgresql;
    let user = target.user_label();
    let mut c = match why {
        Refusal::NeedsPassword => {
            let mut c = Check::new(
                id,
                Status::Warning,
                format!("PostgreSQL asks role {user} for a password"),
            );
            c.details
                .push("The check never sends passwords, so the server version is unknown".into());
            c.fix = Some(Fix {
                text: "Set db_user and db_password in the project's Odoo options, or allow your role to log in over the local socket without a password (peer authentication in pg_hba.conf).".into(),
                commands: Vec::new(),
                url: Some("https://www.postgresql.org/docs/current/auth-pg-hba-conf.html".into()),
            });
            c
        }
        Refusal::NoRole(message) => {
            let mut c = Check::new(
                id,
                Status::Error,
                format!("PostgreSQL runs, but has no role {user}"),
            );
            c.details.push(message);
            c.fix = Some(create_role_fix(&user, pm));
            c
        }
        Refusal::Other(message) => {
            let mut c = Check::new(id, Status::Error, "PostgreSQL refused the connection");
            c.details.push(message);
            c.fix = Some(Fix {
                text:
                    "Check that the server has started and that pg_hba.conf lets your role connect."
                        .into(),
                commands: Vec::new(),
                url: Some("https://www.postgresql.org/docs/current/auth-pg-hba-conf.html".into()),
            });
            c
        }
    };
    c.location = Some(label.to_owned());
    c
}

fn unreachable_check(
    target: &PgTarget,
    tried: Vec<String>,
    installed: bool,
    pm: Option<PackageManager>,
) -> Check {
    let summary = if installed {
        "PostgreSQL is installed but not running"
    } else {
        "PostgreSQL is not reachable"
    };
    let mut c = Check::new(CheckId::Postgresql, Status::Error, summary);
    if tried.is_empty() && !target.socket_dirs.is_empty() {
        let dirs: Vec<String> = target
            .socket_dirs
            .iter()
            .map(|d| d.display().to_string())
            .collect();
        c.details.push(format!(
            "No server socket .s.PGSQL.{} in {}",
            target.port,
            dirs.join(", ")
        ));
    }
    c.details.extend(tried);
    c.fix = Some(server_fix(pm, installed, &target.user_label()));
    c
}

/// Whether a PostgreSQL server program is on `path_var` or in a usual place.
fn server_installed(path_var: Option<&OsStr>) -> bool {
    if path_var.is_some_and(|p| {
        find_executable(p, "postgres").is_some() || find_executable(p, "pg_ctl").is_some()
    }) {
        return true;
    }
    // Debian keeps the server off PATH; Homebrew's versioned formulae too.
    ["/usr/lib/postgresql", "/opt/homebrew/opt", "/usr/local/opt"]
        .iter()
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flatten()
        .flatten()
        .any(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            (name.starts_with("postgresql") || name.bytes().all(|b| b.is_ascii_digit()))
                && e.path().join("bin/postgres").is_file()
        })
}

fn server_fix(pm: Option<PackageManager>, installed: bool, user: &str) -> Fix {
    use PackageManager::{Apt, Brew, Dnf, Pacman};
    let mut commands: Vec<String> = Vec::new();
    let text = match (pm, installed) {
        (Some(Brew), true) => {
            commands.push("brew services list".into());
            "Start PostgreSQL with brew services start <the postgresql formula listed>.".to_owned()
        }
        (Some(Pacman), true) => {
            commands.push("sudo systemctl enable --now postgresql".into());
            "Start the server. On a fresh install, create its data folder first: sudo -iu postgres initdb -D /var/lib/postgres/data".to_owned()
        }
        (Some(Dnf), true) => {
            commands.push("sudo systemctl enable --now postgresql".into());
            "Start the server. On a fresh install, create its data folder first: sudo postgresql-setup --initdb".to_owned()
        }
        (Some(_), true) => {
            commands.push("sudo systemctl enable --now postgresql".into());
            "Start the server.".to_owned()
        }
        (Some(Apt), false) => {
            commands.push(Apt.install_command(&["postgresql"]));
            commands.push(create_role_command(Some(Apt), user));
            "Install PostgreSQL (it starts on its own) and create a role for your user.".to_owned()
        }
        (Some(Pacman), false) => {
            commands.extend([
                Pacman.install_command(&["postgresql"]),
                "sudo -iu postgres initdb -D /var/lib/postgres/data".into(),
                "sudo systemctl enable --now postgresql".into(),
                create_role_command(Some(Pacman), user),
            ]);
            "Install PostgreSQL, create its data folder, start it and create a role for your user."
                .to_owned()
        }
        (Some(Dnf), false) => {
            commands.extend([
                Dnf.install_command(&["postgresql-server"]),
                "sudo postgresql-setup --initdb".into(),
                "sudo systemctl enable --now postgresql".into(),
                create_role_command(Some(Dnf), user),
            ]);
            "Install PostgreSQL, create its data folder, start it and create a role for your user."
                .to_owned()
        }
        (Some(Brew), false) => {
            commands.extend([
                Brew.install_command(&["postgresql@17"]),
                "brew services start postgresql@17".into(),
            ]);
            "Install and start PostgreSQL (Homebrew creates a role for your user).".to_owned()
        }
        (None, _) => "Install and start PostgreSQL, then create a role for your user that may create databases.".to_owned(),
    };
    Fix {
        text,
        commands,
        url: Some("https://www.postgresql.org/download/".into()),
    }
}

fn create_role_fix(user: &str, pm: Option<PackageManager>) -> Fix {
    Fix {
        text: format!("Create a role named {user} that may create databases."),
        commands: vec![create_role_command(pm, user)],
        url: None,
    }
}

fn create_role_command(pm: Option<PackageManager>, user: &str) -> String {
    as_postgres(pm, &format!("createuser --createdb {}", shell_quote(user)))
}

/// `command` run as the `postgres` OS user; Homebrew's server belongs to
/// the user who installed it.
fn as_postgres(pm: Option<PackageManager>, command: &str) -> String {
    if pm == Some(PackageManager::Brew) {
        command.to_owned()
    } else {
        format!("sudo -u postgres {command}")
    }
}

/// `s` as one word in a POSIX shell command shown to the user.
fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.@/=".contains(&b))
    {
        return s.to_owned();
    }
    let mut out = String::from("'");
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// `s` as an SQL identifier.
fn quote_ident(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        if ch == '"' {
            out.push('"');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

#[cfg(test)]
mod unit_tests {
    use super::*;

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
    fn commands_quote_names() {
        assert_eq!(shell_quote("sid"), "sid");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(quote_ident("sid"), "\"sid\"");
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(
            create_role_command(Some(PackageManager::Apt), "sid"),
            "sudo -u postgres createuser --createdb sid"
        );
        assert_eq!(
            create_role_command(Some(PackageManager::Brew), "a b"),
            "createuser --createdb 'a b'"
        );
    }

    #[test]
    fn every_manager_has_a_server_fix() {
        for installed in [false, true] {
            for pm in PackageManager::ALL.map(Some).into_iter().chain([None]) {
                let fix = server_fix(pm, installed, "sid");
                assert!(!fix.text.is_empty());
                assert_eq!(fix.commands.is_empty(), pm.is_none(), "{pm:?} {installed}");
            }
        }
    }
}
