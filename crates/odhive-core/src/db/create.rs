//! Creating an Odoo database the way Odoo's own database manager does.
//!
//! [`create`] runs the project's `odoo-bin` three times, never through a
//! shell:
//!
//! 1. `odoo-bin db -c <conf> init <db> --language=… --username=… [--country=…] [--with-demo]`
//!    creates the database and installs `base` (Odoo's `exp_create_database`).
//! 2. `odoo-bin -c <conf> -d <db> -i <modules> --stop-after-init --no-http`
//!    installs the extra modules, when there are any.
//! 3. `odoo-bin shell -c <conf> -d <db> --no-http` runs [`FINALIZE_SCRIPT`]
//!    (fed on stdin): it checks that the login, language, country and modules
//!    are what was asked (Odoo's `db init` logs a failure but still exits 0)
//!    and sets the admin password.
//!
//! The admin password never appears on a command line (other local users can
//! read those): `db init` sets Odoo's default and step 3 replaces it with the
//! one asked, read from the child's environment. If any step fails, or the
//! work is dropped (job cancelled), the new database and its filestore are
//! removed with `odoo-bin db drop`.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::process::Command;

use super::PgTarget;
use crate::conf::MASK;
use crate::envcheck::postgres;
use crate::names::{self, NameError};
use crate::proc::{self, RunError};
use crate::project::EnvVars;

/// Language when none is asked.
pub const DEFAULT_LANGUAGE: &str = "en_US";
/// Admin login when none is asked.
pub const DEFAULT_LOGIN: &str = "admin";
/// Most extra modules installed at creation.
pub const MAX_MODULES: usize = 200;
/// Longest module name.
pub const MAX_MODULE_LEN: usize = 128;
/// Longest admin login.
pub const MAX_LOGIN_LEN: usize = 128;
/// Longest admin password.
pub const MAX_PASSWORD_LEN: usize = 1024;

/// Output lines kept for an error message.
const TAIL_LINES: usize = 40;

/// Environment variables [`FINALIZE_SCRIPT`] reads.
const ENV_PASSWORD: &str = "ODHIVE_DB_PASSWORD";
const ENV_LOGIN: &str = "ODHIVE_DB_LOGIN";
const ENV_LANG: &str = "ODHIVE_DB_LANG";
const ENV_COUNTRY: &str = "ODHIVE_DB_COUNTRY";
const ENV_MODULES: &str = "ODHIVE_DB_MODULES";

/// What [`FINALIZE_SCRIPT`] prints once it committed.
const READY_MARKER: &str = "odhive: database ready";

/// Run by `odoo-bin shell` (on stdin) as the last step. Fixed text: every
/// value comes from the environment.
pub const FINALIZE_SCRIPT: &str = r#"import os
admin = env.ref('base.user_admin')
problems = []
login = os.environ['ODHIVE_DB_LOGIN']
if (admin.login or '').lower() != login.lower():
    problems.append('the admin login is %r, not %r' % (admin.login, login))
lang = os.environ['ODHIVE_DB_LANG']
if admin.lang != lang:
    problems.append('the admin language is %r, not %r' % (admin.lang, lang))
country = os.environ.get('ODHIVE_DB_COUNTRY')
if country and (env.ref('base.main_company').country_id.code or '') != country:
    problems.append('the company country is not %s' % country)
wanted = [m for m in os.environ.get('ODHIVE_DB_MODULES', '').split(',') if m]
if wanted:
    found = env['ir.module.module'].search([('name', 'in', wanted), ('state', '=', 'installed')])
    missing = sorted(set(wanted) - set(found.mapped('name')))
    if missing:
        problems.append('not installed: ' + ', '.join(missing))
if problems:
    raise SystemExit('odhive: the database is not as asked: ' + '; '.join(problems))
admin.password = os.environ.pop('ODHIVE_DB_PASSWORD')
env.cr.commit()
print('odhive: database ready', flush=True)
"#;

/// A database to create.
#[derive(Clone, PartialEq, Eq)]
pub struct NewDatabase {
    /// Database name.
    pub name: String,
    /// Language, e.g. `en_US` or `fr_BE`; also the admin's.
    pub language: String,
    /// ISO country code of the main company, e.g. `BE`.
    pub country: Option<String>,
    /// Install demo data.
    pub demo: bool,
    /// Admin login.
    pub login: String,
    /// Admin password.
    pub password: String,
    /// Modules to install besides `base`.
    pub modules: Vec<String>,
}

impl fmt::Debug for NewDatabase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewDatabase")
            .field("name", &self.name)
            .field("language", &self.language)
            .field("country", &self.country)
            .field("demo", &self.demo)
            .field("login", &self.login)
            .field("password", &MASK)
            .field("modules", &self.modules)
            .finish()
    }
}

impl NewDatabase {
    /// `name` with the defaults: English, no country, no demo data,
    /// `admin`/`admin`, no extra module.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            language: DEFAULT_LANGUAGE.into(),
            country: None,
            demo: false,
            login: DEFAULT_LOGIN.into(),
            password: "admin".into(),
            modules: Vec::new(),
        }
    }

    /// Checks every field and returns it normalized: country upper-cased,
    /// modules without `base` and duplicates, in the order given.
    ///
    /// # Errors
    /// [`CreateError::Name`] or [`CreateError::Invalid`].
    pub fn validated(&self) -> Result<Self, CreateError> {
        validate_new_db_name(&self.name)?;
        if !is_language(&self.language) {
            return Err(invalid(
                "language",
                format!(
                    "{:?} is not a language code like en_US or fr_BE",
                    self.language
                ),
            ));
        }
        let country = match self.country.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(c) if c.len() == 2 && c.bytes().all(|b| b.is_ascii_alphabetic()) => {
                Some(c.to_ascii_uppercase())
            }
            Some(c) => {
                return Err(invalid(
                    "country",
                    format!("{c:?} is not a two-letter country code like BE"),
                ));
            }
        };
        if self.login.is_empty()
            || self.login.len() > MAX_LOGIN_LEN
            || self.login.trim() != self.login
            || self.login.chars().any(char::is_control)
        {
            return Err(invalid(
                "login",
                format!(
                    "use 1-{MAX_LOGIN_LEN} characters without control characters or leading/trailing spaces"
                ),
            ));
        }
        if self.password.is_empty()
            || self.password.len() > MAX_PASSWORD_LEN
            || self.password.contains('\0')
        {
            return Err(invalid(
                "password",
                format!("use 1-{MAX_PASSWORD_LEN} bytes without NUL"),
            ));
        }
        let mut modules: Vec<String> = Vec::new();
        for m in &self.modules {
            let m = m.trim();
            if !is_module_name(m) {
                return Err(invalid(
                    "modules",
                    format!(
                        "{m:?} is not a module name (lowercase letters, digits and '_', up to {MAX_MODULE_LEN})"
                    ),
                ));
            }
            if m != "base" && !modules.iter().any(|x| x == m) {
                modules.push(m.to_owned());
            }
        }
        if modules.len() > MAX_MODULES {
            return Err(invalid(
                "modules",
                format!("at most {MAX_MODULES} modules at once"),
            ));
        }
        Ok(Self {
            name: self.name.clone(),
            language: self.language.clone(),
            country,
            demo: self.demo,
            login: self.login.clone(),
            password: self.password.clone(),
            modules,
        })
    }

    /// `odoo-bin` arguments of step 1 (`db init`).
    fn init_args(&self, conf: &Path) -> Vec<String> {
        let mut args = vec![
            "db".into(),
            "-c".into(),
            conf.display().to_string(),
            "init".into(),
            format!("--language={}", self.language),
            format!("--username={}", self.login),
        ];
        if let Some(c) = &self.country {
            args.push(format!("--country={c}"));
        }
        if self.demo {
            args.push("--with-demo".into());
        }
        args.push(self.name.clone());
        args
    }

    /// `odoo-bin` arguments of step 2 (install the extra modules).
    fn install_args(&self, conf: &Path) -> Vec<String> {
        vec![
            "-c".into(),
            conf.display().to_string(),
            "-d".into(),
            self.name.clone(),
            "-i".into(),
            self.modules.join(","),
            "--stop-after-init".into(),
            "--no-http".into(),
        ]
    }

    /// The environment [`FINALIZE_SCRIPT`] reads.
    fn finalize_env(&self) -> Vec<(&'static str, String)> {
        let mut env = vec![
            (ENV_PASSWORD, self.password.clone()),
            (ENV_LOGIN, self.login.clone()),
            (ENV_LANG, self.language.clone()),
            (ENV_MODULES, self.modules.join(",")),
        ];
        if let Some(c) = &self.country {
            env.push((ENV_COUNTRY, c.clone()));
        }
        env
    }
}

/// Odoo's own rule for new database names (`DB_NAME_RE`,
/// `^[a-zA-Z0-9][a-zA-Z0-9_.-]+$`) on top of OdHive's.
///
/// # Errors
/// [`CreateError::Name`] or [`CreateError::Invalid`].
pub fn validate_new_db_name(name: &str) -> Result<(), CreateError> {
    names::validate_db_name(name)?;
    let first_ok = name
        .bytes()
        .next()
        .is_some_and(|b| b.is_ascii_alphanumeric());
    if !first_ok || name.len() < 2 {
        return Err(invalid(
            "name",
            format!("{name:?}: Odoo wants at least 2 characters, starting with a letter or digit"),
        ));
    }
    Ok(())
}

/// `ll`, `lll`, `ll_CC`, optionally `@variant` (e.g. `sr@latin`).
fn is_language(s: &str) -> bool {
    let (code, variant) = match s.split_once('@') {
        Some((c, v)) => (c, Some(v)),
        None => (s, None),
    };
    let (lang, region) = match code.split_once('_') {
        Some((l, r)) => (l, Some(r)),
        None => (code, None),
    };
    (2..=3).contains(&lang.len())
        && lang.bytes().all(|b| b.is_ascii_lowercase())
        && region.is_none_or(|r| r.len() == 2 && r.bytes().all(|b| b.is_ascii_uppercase()))
        && variant.is_none_or(|v| {
            (1..=16).contains(&v.len()) && v.bytes().all(|b| b.is_ascii_lowercase())
        })
}

fn is_module_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_MODULE_LEN
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn invalid(field: &'static str, message: String) -> CreateError {
    CreateError::Invalid { field, message }
}

/// The modules in `modules` that no folder of `addons_paths` holds (a
/// subfolder with `__manifest__.py`).
#[must_use]
pub fn missing_modules(modules: &[String], addons_paths: &[PathBuf]) -> Vec<String> {
    modules
        .iter()
        .filter(|m| {
            !addons_paths
                .iter()
                .any(|dir| dir.join(m).join("__manifest__.py").is_file())
        })
        .cloned()
        .collect()
}

/// How to run the project's `odoo-bin`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OdooCommand {
    /// The project's venv Python.
    pub python: PathBuf,
    /// `odoo-bin` of the Community source.
    pub odoo_bin: PathBuf,
    /// The project's `odoo.conf`.
    pub conf: PathBuf,
    /// Working directory (the project folder).
    pub cwd: PathBuf,
    /// The project's `[env]`.
    pub env: EnvVars,
}

impl OdooCommand {
    fn command(&self, args: &[String]) -> Command {
        let mut cmd = Command::new(&self.python);
        cmd.arg(&self.odoo_bin).args(args).current_dir(&self.cwd);
        for key in proc::PYTHON_ENV_OVERRIDES {
            cmd.env_remove(key);
        }
        cmd.env("PYTHONUNBUFFERED", "1");
        cmd.envs(&self.env.0);
        cmd
    }
}

/// A step of [`create`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CreateStep {
    /// Creating the database and installing `base`.
    Initialize,
    /// Installing the extra modules.
    InstallModules,
    /// Checking the result and setting the admin password.
    Finalize,
    /// Removing the half-made database after a failure.
    Cleanup,
}

impl fmt::Display for CreateStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Initialize => "creating the database",
            Self::InstallModules => "installing modules",
            Self::Finalize => "finishing the database",
            Self::Cleanup => "removing the half-made database",
        })
    }
}

/// Progress of [`create`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum CreateEvent {
    /// A step started.
    Step {
        /// The step.
        step: CreateStep,
    },
    /// Odoo started installing a module (`Loading module x (i/n)`).
    Module {
        /// Module name.
        name: String,
        /// Its position, from 1.
        index: u32,
        /// Modules Odoo loads in this step.
        total: u32,
    },
    /// A line of `odoo-bin` output.
    Output {
        /// The line.
        line: String,
    },
}

/// What [`create`] made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct CreatedDatabase {
    /// Database name.
    pub name: String,
    /// Language.
    pub language: String,
    /// Country of the main company.
    pub country: Option<String>,
    /// Whether demo data was installed.
    pub demo: bool,
    /// Admin login.
    pub login: String,
    /// Extra modules installed.
    pub modules: Vec<String>,
}

/// Errors from [`create`].
#[derive(Debug, Error)]
pub enum CreateError {
    /// The database name is invalid.
    #[error(transparent)]
    Name(#[from] NameError),
    /// A field is invalid.
    #[error("invalid {field}: {message}")]
    Invalid {
        /// Field name.
        field: &'static str,
        /// Why.
        message: String,
    },
    /// A database with this name exists.
    #[error("a database named {0:?} already exists")]
    Exists(String),
    /// The PostgreSQL server cannot be used.
    #[error("PostgreSQL: {0}")]
    Server(String),
    /// `odoo-bin` could not be started.
    #[error("could not run {program}: {source}")]
    Spawn {
        /// Program.
        program: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// A step failed; the database was removed.
    #[error("{step} failed (exit code {code:?}); the database was removed. Last output:\n{tail}")]
    Failed {
        /// The step.
        step: CreateStep,
        /// Exit code, if it exited normally.
        code: Option<i32>,
        /// Last output lines.
        tail: String,
    },
}

/// Whether a database named `name` exists on `target`'s server (any owner).
///
/// # Errors
/// [`CreateError::Server`] when the server cannot be used.
pub async fn exists(target: &PgTarget, name: &str, timeout: Duration) -> Result<bool, CreateError> {
    let conn = postgres::connect(target, timeout).await.map_err(|e| {
        let c = postgres::failure_check(target, e, None, None);
        let mut msg = c.summary;
        if let Some(d) = c.details.first() {
            msg.push_str(": ");
            msg.push_str(d);
        }
        CreateError::Server(msg)
    })?;
    let row = conn
        .client
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_database WHERE datname = $1)",
            &[&name],
        )
        .await
        .map_err(|e| CreateError::Server(e.to_string()))?;
    row.try_get(0)
        .map_err(|e| CreateError::Server(e.to_string()))
}

/// Creates `new` (see the module docs) on the server `target` points at,
/// which must be the one `odoo`'s config connects to. Checks the request and
/// that no database has this name first. Reports progress to `on_event`.
///
/// On failure, and when the returned future is dropped, the database is
/// removed again (on drop, in a background task).
///
/// # Errors
/// See [`CreateError`].
pub async fn create(
    target: &PgTarget,
    odoo: &OdooCommand,
    new: &NewDatabase,
    timeout: Duration,
    mut on_event: impl FnMut(CreateEvent),
) -> Result<CreatedDatabase, CreateError> {
    let new = new.validated()?;
    if exists(target, &new.name, timeout).await? {
        return Err(CreateError::Exists(new.name.clone()));
    }
    // From here on a failure or a drop removes the database.
    let mut guard = DropGuard(Some((odoo.clone(), new.name.clone())));
    let outcome = run_steps(odoo, &new, &mut on_event).await;
    guard.0 = None;
    match outcome {
        Ok(()) => Ok(CreatedDatabase {
            name: new.name,
            language: new.language,
            country: new.country,
            demo: new.demo,
            login: new.login,
            modules: new.modules,
        }),
        Err(e) => {
            on_event(CreateEvent::Step {
                step: CreateStep::Cleanup,
            });
            if let Err(why) = drop_database(odoo, &new.name, &mut on_event).await {
                tracing::warn!(db = %new.name, "could not remove the half-made database: {why}");
            }
            Err(e)
        }
    }
}

async fn run_steps(
    odoo: &OdooCommand,
    new: &NewDatabase,
    on_event: &mut impl FnMut(CreateEvent),
) -> Result<(), CreateError> {
    run_step(
        odoo,
        CreateStep::Initialize,
        odoo.command(&new.init_args(&odoo.conf)),
        None,
        on_event,
    )
    .await?;
    if !new.modules.is_empty() {
        run_step(
            odoo,
            CreateStep::InstallModules,
            odoo.command(&new.install_args(&odoo.conf)),
            None,
            on_event,
        )
        .await?;
    }
    let mut cmd = odoo.command(&[
        "shell".into(),
        "-c".into(),
        odoo.conf.display().to_string(),
        "-d".into(),
        new.name.clone(),
        "--no-http".into(),
    ]);
    cmd.envs(new.finalize_env());
    let lines = run_step(
        odoo,
        CreateStep::Finalize,
        cmd,
        Some(FINALIZE_SCRIPT.as_bytes().to_vec()),
        on_event,
    )
    .await?;
    if !lines.ready {
        return Err(CreateError::Failed {
            step: CreateStep::Finalize,
            code: Some(0),
            tail: lines.tail,
        });
    }
    Ok(())
}

struct StepOutput {
    tail: String,
    ready: bool,
}

async fn run_step(
    odoo: &OdooCommand,
    step: CreateStep,
    cmd: Command,
    input: Option<Vec<u8>>,
    on_event: &mut impl FnMut(CreateEvent),
) -> Result<StepOutput, CreateError> {
    on_event(CreateEvent::Step { step });
    let mut ready = false;
    let done = proc::run_streaming_with_input(cmd, input, TAIL_LINES, |line| {
        if line.trim() == READY_MARKER {
            ready = true;
        }
        if let Some(event) = module_progress(&line) {
            on_event(event);
        }
        on_event(CreateEvent::Output { line });
    })
    .await
    .map_err(|e| match e {
        RunError::Spawn(source) => CreateError::Spawn {
            program: odoo.python.clone(),
            source,
        },
        RunError::Io(e) => CreateError::Failed {
            step,
            code: None,
            tail: e.to_string(),
        },
    })?;
    if !done.status.success() {
        return Err(CreateError::Failed {
            step,
            code: done.status.code(),
            tail: done.tail,
        });
    }
    Ok(StepOutput {
        tail: done.tail,
        ready,
    })
}

/// `… odoo.modules.loading: Loading module sale (12/42)` → module event.
fn module_progress(line: &str) -> Option<CreateEvent> {
    let rest = line.split_once("Loading module ")?.1;
    let (name, counts) = rest.split_once(" (")?;
    let (index, total) = counts.strip_suffix(')')?.split_once('/')?;
    if !is_module_name(name) {
        return None;
    }
    Some(CreateEvent::Module {
        name: name.to_owned(),
        index: index.parse().ok()?,
        total: total.parse().ok()?,
    })
}

/// `odoo-bin db -c <conf> drop <name>`: the database and its filestore.
async fn drop_database(
    odoo: &OdooCommand,
    name: &str,
    on_event: &mut impl FnMut(CreateEvent),
) -> Result<(), String> {
    let cmd = odoo.command(&[
        "db".into(),
        "-c".into(),
        odoo.conf.display().to_string(),
        "drop".into(),
        name.to_owned(),
    ]);
    let done = proc::run_streaming(cmd, TAIL_LINES, |line| {
        on_event(CreateEvent::Output { line });
    })
    .await
    .map_err(|e| format!("{e:?}"))?;
    if done.status.success() {
        Ok(())
    } else {
        Err(done.tail)
    }
}

/// Removes the database in a background task when dropped armed (the
/// create was cancelled mid-way).
struct DropGuard(Option<(OdooCommand, String)>);

impl Drop for DropGuard {
    fn drop(&mut self) {
        let Some((odoo, name)) = self.0.take() else {
            return;
        };
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(db = %name, "cancelled create left the database behind");
            return;
        };
        rt.spawn(async move {
            // The killed odoo-bin's connections close on their own; Odoo's
            // drop also ends any that are left.
            if let Err(why) = drop_database(&odoo, &name, &mut |_| {}).await {
                tracing::warn!(db = %name, "could not remove the cancelled database: {why}");
            }
        });
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn requests_are_checked_and_normalized() {
        let mut n = NewDatabase::new("shop_main");
        n.country = Some(" be ".into());
        n.modules = vec![
            "sale".into(),
            "base".into(),
            " stock ".into(),
            "sale".into(),
        ];
        let v = n.validated().expect("valid");
        assert_eq!(v.country.as_deref(), Some("BE"));
        assert_eq!(v.modules, ["sale", "stock"]);

        let bad = |f: fn(&mut NewDatabase)| {
            let mut n = NewDatabase::new("shop");
            f(&mut n);
            n.validated().expect_err("invalid")
        };
        for name in [
            "a", "_x", ".x", "-x", "a;b", "a b", "a'b", "a\"b", "x--;", "",
        ] {
            assert!(validate_new_db_name(name).is_err(), "{name:?} accepted");
        }
        assert!(validate_new_db_name("x1").is_ok());
        assert!(matches!(
            bad(|n| n.name = "x;DROP".into()),
            CreateError::Name(_)
        ));
        assert!(matches!(
            bad(|n| n.language = String::new()),
            CreateError::Invalid {
                field: "language",
                ..
            }
        ));
        for lang in ["EN", "en-US", "en_us", "english", "e", "en_US@", "en_US@X1"] {
            let mut n = NewDatabase::new("shop");
            n.language = lang.into();
            assert!(n.validated().is_err(), "{lang:?} accepted");
        }
        for lang in ["en_US", "fr", "fil_PH", "sr@latin", "sr_RS@latin"] {
            let mut n = NewDatabase::new("shop");
            n.language = lang.into();
            assert!(n.validated().is_ok(), "{lang:?} refused");
        }
        assert!(matches!(
            bad(|n| n.country = Some("BEL".into())),
            CreateError::Invalid {
                field: "country",
                ..
            }
        ));
        for login in ["", " admin", "ad\nmin"] {
            let mut n = NewDatabase::new("shop");
            n.login = login.into();
            assert!(n.validated().is_err(), "{login:?} accepted");
        }
        assert!(matches!(
            bad(|n| n.password = String::new()),
            CreateError::Invalid {
                field: "password",
                ..
            }
        ));
        for m in ["Sale", "sale-x", "../sale", "sale,stock", "a b"] {
            let mut n = NewDatabase::new("shop");
            n.modules = vec![m.into()];
            assert!(n.validated().is_err(), "{m:?} accepted");
        }
        let mut n = NewDatabase::new("shop");
        n.modules = (0..=MAX_MODULES).map(|i| format!("m{i}")).collect();
        assert!(n.validated().is_err());
    }

    #[test]
    fn the_password_is_never_an_argument_nor_in_debug() {
        let mut n = NewDatabase::new("shop");
        n.password = "s3cret-Pa55".into();
        n.login = "-x@example.com".into();
        n.country = Some("BE".into());
        n.demo = true;
        n.modules = vec!["sale".into(), "stock".into()];
        let conf = Path::new("/p/odoo.conf");
        let init = n.init_args(conf);
        assert_eq!(
            init,
            [
                "db",
                "-c",
                "/p/odoo.conf",
                "init",
                "--language=en_US",
                "--username=-x@example.com",
                "--country=BE",
                "--with-demo",
                "shop"
            ]
        );
        let install = n.install_args(conf);
        assert_eq!(install[4..6], ["-i", "sale,stock"]);
        for a in init.iter().chain(&install) {
            assert!(!a.contains("s3cret"), "{a}");
        }
        assert!(!format!("{n:?}").contains("s3cret"));
        assert!(
            n.finalize_env()
                .iter()
                .any(|(k, v)| *k == ENV_PASSWORD && v == "s3cret-Pa55")
        );
        assert!(!FINALIZE_SCRIPT.contains("s3cret"));
    }

    #[test]
    fn module_progress_is_read_from_odoo_lines() {
        let l = "2026-11-25 10:00:00,123 4242 INFO shop odoo.modules.loading: Loading module sale (12/42)";
        assert_eq!(
            module_progress(l),
            Some(CreateEvent::Module {
                name: "sale".into(),
                index: 12,
                total: 42
            })
        );
        for l in [
            "Loading module sale",
            "Loading module sale (x/42)",
            "Loading module Sale (1/2)",
            "INFO odoo: nothing",
        ] {
            assert_eq!(module_progress(l), None, "{l}");
        }
    }

    #[test]
    fn missing_modules_are_found_on_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let addons = dir.path().join("addons");
        std::fs::create_dir_all(addons.join("sale")).expect("mkdir");
        std::fs::write(addons.join("sale/__manifest__.py"), "{}").expect("write");
        std::fs::create_dir_all(addons.join("empty")).expect("mkdir");
        let mods = vec!["sale".to_owned(), "empty".to_owned(), "nope".to_owned()];
        assert_eq!(missing_modules(&mods, &[addons]), ["empty", "nope"]);
    }
}
