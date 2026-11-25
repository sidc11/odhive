//! PostgreSQL: reachable the way Odoo connects by default, server version,
//! and whether the role may create databases.
//!
//! Without `db_host`, Odoo (through libpq) connects over the Unix socket as
//! the OS user. The check tries the usual socket folders, then TCP on
//! `localhost`, and stops at the first server that answers. It sends a
//! password only when the target carries one (a project's `db_password`);
//! a server that asks for one otherwise is reported as such.

use std::error::Error as _;
use std::ffi::OsStr;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use tokio_postgres::config::SslMode as PqSslMode;
use tokio_postgres::error::SqlState;
use tokio_postgres::{Client, Config, NoTls};

use super::{Check, CheckId, Fix, Requirement, Status};
use crate::db::{PgTarget, ServerFacts, SslMode};
use crate::python::find_executable;
use crate::syslib::PackageManager;

/// Database connected to; `template1` when it does not exist.
const DATABASES: [&str; 2] = ["postgres", "template1"];

/// One place a server may listen.
#[derive(Debug, Clone)]
pub(crate) enum Place {
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

pub(crate) enum Refusal {
    /// It asked for a password and none is set.
    NeedsPassword,
    /// It rejected the password sent.
    WrongPassword,
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
    probe(target, path_var, requirements, pm, timeout).await.0
}

/// [`check`], plus what the server said about itself and the role when the
/// login worked.
pub(crate) async fn probe(
    target: &PgTarget,
    path_var: Option<&OsStr>,
    requirements: &[Requirement],
    pm: Option<PackageManager>,
    timeout: Duration,
) -> (Check, Option<ServerFacts>) {
    match connect(target, timeout).await {
        Err(e) => (failure_check(target, e, path_var, pm), None),
        Ok(conn) => {
            let (mut c, facts) = connected(target, &conn.client, requirements, pm).await;
            c.location = Some(conn.label());
            if matches!(conn.place, Place::Tcp(_)) && !target.socket_dirs.is_empty() {
                c.details.push(
                    "No Unix socket was found: set db_host (and db_port) in the project's Odoo options, as Odoo uses the socket otherwise"
                        .into(),
                );
                c.status = c.status.max(Status::Warning);
            }
            (c, facts)
        }
    }
}

/// A login to the server's maintenance database, and where it was made.
pub(crate) struct Connection {
    pub(crate) client: Client,
    pub(crate) place: Place,
    port: u16,
}

impl Connection {
    /// Where the server was reached, e.g. `Unix socket in /run/postgresql`.
    pub(crate) fn label(&self) -> String {
        self.place.label(self.port)
    }
}

/// Why [`connect`] found no usable server.
pub(crate) enum ConnectError {
    /// `db_sslmode` demands TLS.
    Tls(SslMode),
    /// A server answered at `label` and refused the login.
    Refused { label: String, why: Refusal },
    /// Nothing answered; one line per place tried.
    Unreachable(Vec<String>),
}

/// Logs in to the maintenance database (`postgres`, else `template1`) at the
/// first place a server answers: sockets first, then TCP.
pub(crate) async fn connect(
    target: &PgTarget,
    timeout: Duration,
) -> Result<Connection, ConnectError> {
    if target.ssl_mode.requires_tls() {
        return Err(ConnectError::Tls(target.ssl_mode));
    }
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
    for place in places {
        let label = place.label(target.port);
        match attempt(target, &place, &DATABASES, timeout).await {
            Attempt::Unreachable(why) => tried.push(format!("{label}: {why}")),
            Attempt::Refused(why) => return Err(ConnectError::Refused { label, why }),
            Attempt::Connected(client) => {
                return Ok(Connection {
                    client,
                    place,
                    port: target.port,
                });
            }
        }
    }
    Err(ConnectError::Unreachable(tried))
}

/// Logs in to database `db` at `place` (where [`connect`] reached the
/// server). The name is a connection parameter, never part of a query.
pub(crate) async fn connect_db(
    target: &PgTarget,
    place: &Place,
    db: &str,
    timeout: Duration,
) -> Result<Client, String> {
    match attempt(target, place, &[db], timeout).await {
        Attempt::Connected(client) => Ok(client),
        Attempt::Unreachable(why) => Err(why),
        Attempt::Refused(Refusal::NeedsPassword) => Err("the server asks for a password".into()),
        Attempt::Refused(Refusal::WrongPassword) => Err("the server rejected the password".into()),
        Attempt::Refused(Refusal::NoRole(m) | Refusal::Other(m)) => Err(m),
    }
}

/// The finding for a failed [`connect`].
pub(crate) fn failure_check(
    target: &PgTarget,
    err: ConnectError,
    path_var: Option<&OsStr>,
    pm: Option<PackageManager>,
) -> Check {
    match err {
        ConnectError::Tls(mode) => tls_required(mode),
        ConnectError::Refused { label, why } => refused(target, &label, why, pm),
        ConnectError::Unreachable(tried) => {
            unreachable_check(target, tried, server_installed(path_var), pm)
        }
    }
}

fn tls_required(mode: SslMode) -> Check {
    let mut c = Check::new(
        CheckId::Postgresql,
        Status::Error,
        format!(
            "db_sslmode = {} needs TLS, which OdHive does not support yet",
            mode.as_str()
        ),
    );
    c.fix = Some(Fix {
        text: "Set db_sslmode to prefer (the default) or disable in the project's Odoo options; a local development server does not need TLS.".into(),
        commands: Vec::new(),
        url: None,
    });
    c
}

/// Tries `dbs` in turn (the next one when a database does not exist).
async fn attempt(target: &PgTarget, place: &Place, dbs: &[&str], timeout: Duration) -> Attempt {
    for db in dbs.iter().copied() {
        let mut cfg = Config::new();
        match place {
            Place::Socket(dir) => cfg.host_path(dir),
            Place::Tcp(host) => cfg.host(host),
        };
        cfg.port(target.port)
            .dbname(db)
            .application_name("odhive")
            .connect_timeout(timeout)
            .ssl_mode(if target.ssl_mode == SslMode::Disable {
                PqSslMode::Disable
            } else {
                PqSslMode::Prefer
            });
        if let Some(user) = &target.user {
            cfg.user(user);
        }
        if let Some(password) = &target.password {
            cfg.password(password);
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
                if target.password.is_some() {
                    Refusal::WrongPassword
                } else {
                    Refusal::NeedsPassword
                }
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
    Attempt::Refused(Refusal::Other(match dbs {
        [db] => format!("database {db} does not exist"),
        _ => format!("none of the databases {} exists", dbs.join(", ")),
    }))
}

async fn server_info(client: &Client) -> Result<ServerFacts, tokio_postgres::Error> {
    let row = client
        .query_one(
            "SELECT current_setting('server_version_num')::int4, \
                    current_setting('server_version'), \
                    r.rolname::text, r.rolcreatedb, r.rolsuper \
             FROM pg_catalog.pg_roles r WHERE r.rolname = current_user",
            &[],
        )
        .await?;
    let version: String = row.try_get(1)?;
    // `16.4 (Ubuntu 16.4-1.pgdg24.04+1)` → `16.4`.
    let version = version
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned();
    let create_db: bool = row.try_get(3)?;
    let superuser: bool = row.try_get(4)?;
    Ok(ServerFacts {
        version_num: row.try_get(0)?,
        version,
        role: row.try_get(2)?,
        can_create_db: create_db || superuser,
        superuser,
    })
}

async fn connected(
    target: &PgTarget,
    client: &Client,
    requirements: &[Requirement],
    pm: Option<PackageManager>,
) -> (Check, Option<ServerFacts>) {
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
            return (c, None);
        }
    };
    let version = info.version.clone();
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
    } else if info.can_create_db {
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
    (c, Some(info))
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
                .push("No password is set, so the server version is unknown".into());
            c.fix = Some(Fix {
                text: "Set db_user and db_password in the project's Odoo options, or allow your role to log in over the local socket without a password (peer authentication in pg_hba.conf).".into(),
                commands: Vec::new(),
                url: Some("https://www.postgresql.org/docs/current/auth-pg-hba-conf.html".into()),
            });
            c
        }
        Refusal::WrongPassword => {
            let mut c = Check::new(
                id,
                Status::Error,
                format!("PostgreSQL rejected the password for role {user}"),
            );
            c.fix = Some(Fix {
                text: "Check db_user and db_password in the project's Odoo options.".into(),
                commands: Vec::new(),
                url: None,
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
