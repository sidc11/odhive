//! Project model: `<project>/odhive.toml`.
//!
//! ```toml
//! version = 1
//! name = "acme"
//! addons_paths = ["addons", "/home/dev/src/acme-addons"]
//!
//! [sources]
//! community = "odoo-19.0"
//! enterprise = "enterprise-19.0"
//!
//! [env]
//! PGHOST = "localhost"
//!
//! [logs]                    # all optional; see logs::LogSettings
//! buffer_entries = 50000
//! file_keep = 5
//!
//! [[instances]]
//! id = "main"
//! http_port = 8069
//! db = "acme"
//! workers = 0
//! extra_args = ["--dev=xml"]
//! reload_mode = "auto"      # auto | restart | off
//! autofix_mode = "auto"     # auto | ask | off
//! sql_log = false           # true = log every SQL query (odoo.sql_db:DEBUG)
//!
//! [instances.env]           # on top of [env], for this instance only
//! ODOO_STAGE = "dev"
//! ```

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::conf::{self, ConfError, OdooOptions};
use crate::config::{Edition, GlobalConfig, SourceEntry};
use crate::fsutil;
use crate::logs::LogSettings;
use crate::names::{
    NameError, validate_db_name, validate_env_key, validate_instance_id, validate_project_name,
};

/// File name of the project settings inside the project folder.
pub const PROJECT_FILE: &str = "odhive.toml";
/// Schema version written by this build.
pub const PROJECT_VERSION: u32 = 1;
/// Project files larger than this are rejected.
pub const MAX_PROJECT_BYTES: u64 = 256 * 1024;
/// Upper bound for `workers` per instance.
pub const MAX_WORKERS: u32 = 64;
/// Lowest port an instance may use (no privileged ports).
pub const MIN_PORT: u16 = 1024;
/// Id of the instance created with a new project.
pub const DEFAULT_INSTANCE: &str = "main";
/// Default custom addons folder, relative to the project folder.
pub const DEFAULT_ADDONS_DIR: &str = "addons";

/// `odoo-bin` options OdHive sets itself; `extra_args` must not override them.
pub const MANAGED_OPTIONS: &[&str] = &[
    "-c",
    "--config",
    "-s",
    "--save",
    "-p",
    "--http-port",
    "--gevent-port",
    "--longpolling-port",
    "-d",
    "--database",
    "--addons-path",
    "-D",
    "--data-dir",
    "--logfile",
    "--workers",
];

/// Errors returned while reading, writing or validating a project.
#[derive(Debug, Error)]
pub enum ProjectError {
    /// An I/O operation on `path` failed.
    #[error("{path}: {source}")]
    Io {
        /// Path involved.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// No `odhive.toml` in the folder.
    #[error("{0}: no {PROJECT_FILE} found")]
    NotFound(PathBuf),
    /// The project file exceeds [`MAX_PROJECT_BYTES`].
    #[error("{0}: project file is larger than {MAX_PROJECT_BYTES} bytes")]
    TooLarge(PathBuf),
    /// Not valid TOML or does not match the schema.
    #[error("{path}: invalid project file: {source}")]
    Parse {
        /// Path of the file.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: Box<toml::de::Error>,
    },
    /// The project could not be serialized.
    #[error("could not serialize project: {0}")]
    Serialize(#[from] toml::ser::Error),
    /// Written by a newer OdHive.
    #[error(
        "project version {0} is newer than supported version {PROJECT_VERSION}; upgrade OdHive"
    )]
    UnsupportedVersion(u32),
    /// A name, id, database or env key is invalid.
    #[error(transparent)]
    Name(#[from] NameError),
    /// An Odoo option in `[odoo_options]` / `[odoo_sections]` is invalid.
    #[error(transparent)]
    Conf(#[from] ConfError),
    /// A project needs at least one instance.
    #[error("a project needs at least one instance")]
    NoInstances,
    /// Two instances share an id.
    #[error("duplicate instance id {0:?}")]
    DuplicateInstance(String),
    /// A port below [`MIN_PORT`].
    #[error("instance {instance:?}: port {port} is privileged; use {MIN_PORT} or higher")]
    PrivilegedPort {
        /// Instance id.
        instance: String,
        /// Offending port.
        port: u16,
    },
    /// The same port is configured twice.
    #[error("port {0} is used more than once in this project")]
    PortClash(u16),
    /// Two instances share a database.
    #[error("database {0:?} is used by more than one instance")]
    DatabaseClash(String),
    /// `workers` exceeds [`MAX_WORKERS`].
    #[error("instance {instance:?}: workers must be at most {MAX_WORKERS}, got {workers}")]
    TooManyWorkers {
        /// Instance id.
        instance: String,
        /// Configured value.
        workers: u32,
    },
    /// The derived `<project>_<instance>` database name is too long.
    #[error("instance {0:?}: default database name is too long; set `db` explicitly")]
    DerivedDbTooLong(String),
    /// `extra_args` overrides an option OdHive manages.
    #[error("instance {instance:?}: {arg:?} is managed by OdHive and cannot be set in extra_args")]
    ManagedOption {
        /// Instance id.
        instance: String,
        /// Offending argument.
        arg: String,
    },
    /// An argument or env value contains a NUL byte or is empty.
    #[error("{0}: contains a NUL byte or is empty")]
    InvalidValue(String),
    /// A `[logs]` value is out of range.
    #[error("[logs]: {0}")]
    LogSettings(String),
    /// A relative addons path leaves the project folder.
    #[error("addons path {0:?} must be absolute or stay inside the project folder")]
    BadAddonsPath(PathBuf),
    /// `python` is set but relative.
    #[error("python must be an absolute path, got {0:?}")]
    RelativePython(PathBuf),
    /// A referenced source is not registered.
    #[error("source {0:?} is not registered")]
    UnknownSource(String),
    /// A referenced source has the wrong edition.
    #[error("source {id:?} is not an Odoo {expected:?} source")]
    WrongEdition {
        /// Source id.
        id: String,
        /// Edition the slot requires.
        expected: Edition,
    },
    /// Community and Enterprise sources are different Odoo versions.
    #[error("community {community} and enterprise {enterprise} are different Odoo versions")]
    VersionMismatch {
        /// Community version.
        community: String,
        /// Enterprise version.
        enterprise: String,
    },
}

/// What happens when files of the project's custom modules change (see
/// [`crate::reload`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ReloadMode {
    /// `.py` → restart, data files and manifests → restart with
    /// `-u <module>`, `static/` → browser refresh only.
    #[default]
    Auto,
    /// Code and data changes → plain restart (no module update); `static/`
    /// → browser refresh only.
    Restart,
    /// Files are not watched.
    Off,
}

/// What happens on `ModuleNotFoundError`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum AutofixMode {
    /// Install packages from trusted sources automatically; ask for unknown ones.
    #[default]
    Auto,
    /// Always ask before installing.
    Ask,
    /// Never install; only report.
    Off,
}

/// Environment variables for `odoo-bin`. Values are masked in `Debug` output.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EnvVars(pub BTreeMap<String, String>);

impl fmt::Debug for EnvVars {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map()
            .entries(self.0.keys().map(|k| (k, "***")))
            .finish()
    }
}

impl EnvVars {
    /// Whether no variables are set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Registered sources a project runs on (ids from the global config).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectSources {
    /// Community source id, e.g. `odoo-19.0`.
    pub community: String,
    /// Optional Enterprise source id, e.g. `enterprise-19.0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enterprise: Option<String>,
}

/// One `odoo-bin` process of a project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instance {
    /// Instance id, unique within the project.
    pub id: String,
    /// HTTP port; `None` = allocated automatically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_port: Option<u16>,
    /// Gevent (websocket) port; `None` = allocated automatically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gevent_port: Option<u16>,
    /// Database; `None` = `<project>_<instance>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub db: Option<String>,
    /// Odoo `--workers` (0 = threaded, best for development).
    #[serde(default)]
    pub workers: u32,
    /// Extra `odoo-bin` arguments (passed as separate argv entries, never via a shell).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_args: Vec<String>,
    /// Reload behavior on file changes.
    #[serde(default)]
    pub reload_mode: ReloadMode,
    /// Missing-dependency behavior.
    #[serde(default)]
    pub autofix_mode: AutofixMode,
    /// Log every SQL query Odoo runs (`--log-handler=odoo.sql_db:DEBUG`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sql_log: bool,
    /// Environment variables for this instance only; they override the
    /// project's `[env]` key by key.
    #[serde(default, skip_serializing_if = "EnvVars::is_empty")]
    pub env: EnvVars,
}

impl Instance {
    /// A new instance with automatic ports and default modes.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            http_port: None,
            gevent_port: None,
            db: None,
            workers: 0,
            extra_args: Vec::new(),
            reload_mode: ReloadMode::default(),
            autofix_mode: AutofixMode::default(),
            sql_log: false,
            env: EnvVars::default(),
        }
    }

    /// The environment of this instance's `odoo-bin`: `project` (the
    /// project's `[env]`) overridden by the instance's own variables.
    #[must_use]
    pub fn merged_env(&self, project: &EnvVars) -> EnvVars {
        let mut env = project.clone();
        env.0
            .extend(self.env.0.iter().map(|(k, v)| (k.clone(), v.clone())));
        env
    }

    /// Database this instance uses: `db`, or `<project>_<instance>`.
    #[must_use]
    pub fn db_name(&self, project: &str) -> String {
        self.db
            .clone()
            .unwrap_or_else(|| format!("{project}_{}", self.id))
    }
}

/// Contents of `odhive.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectConfig {
    /// Schema version, see [`PROJECT_VERSION`].
    pub version: u32,
    /// Project name (`^[a-z0-9][a-z0-9_-]{0,63}$`).
    pub name: String,
    /// Python interpreter used to create the venv; `None` = detect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub python: Option<PathBuf>,
    /// Custom addons folders: relative to the project folder, or absolute.
    #[serde(default)]
    pub addons_paths: Vec<PathBuf>,
    /// Odoo sources.
    pub sources: ProjectSources,
    /// Environment variables for every instance.
    #[serde(default, skip_serializing_if = "EnvVars::is_empty")]
    pub env: EnvVars,
    /// Odoo options written to `odoo.conf` (secrets masked in `Debug`).
    #[serde(default, skip_serializing_if = "OdooOptions::is_empty")]
    pub odoo_options: OdooOptions,
    /// Extra `odoo.conf` sections read by third-party modules.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub odoo_sections: BTreeMap<String, OdooOptions>,
    /// Log buffer and file limits.
    #[serde(default, skip_serializing_if = "LogSettings::is_default")]
    pub logs: LogSettings,
    /// Instances (at least one).
    pub instances: Vec<Instance>,
}

impl ProjectConfig {
    /// A new project with `addons/` and one `main` instance.
    #[must_use]
    pub fn new(name: impl Into<String>, community: impl Into<String>) -> Self {
        Self {
            version: PROJECT_VERSION,
            name: name.into(),
            python: None,
            addons_paths: vec![PathBuf::from(DEFAULT_ADDONS_DIR)],
            sources: ProjectSources {
                community: community.into(),
                enterprise: None,
            },
            env: EnvVars::default(),
            odoo_options: OdooOptions::default(),
            odoo_sections: BTreeMap::new(),
            logs: LogSettings::default(),
            instances: vec![Instance::new(DEFAULT_INSTANCE)],
        }
    }

    /// Checks everything that can be checked without the global config.
    ///
    /// # Errors
    /// Returns the first violated rule.
    pub fn validate(&self) -> Result<(), ProjectError> {
        if self.version > PROJECT_VERSION {
            return Err(ProjectError::UnsupportedVersion(self.version));
        }
        validate_project_name(&self.name)?;
        if let Some(py) = &self.python
            && !py.is_absolute()
        {
            return Err(ProjectError::RelativePython(py.clone()));
        }
        for p in &self.addons_paths {
            check_addons_path(p)?;
        }
        validate_env(&self.env, "env")?;
        conf::validate_model(&self.odoo_options, &self.odoo_sections)?;
        self.logs.validate().map_err(ProjectError::LogSettings)?;
        self.validate_instances()
    }

    fn validate_instances(&self) -> Result<(), ProjectError> {
        if self.instances.is_empty() {
            return Err(ProjectError::NoInstances);
        }
        let mut ids = HashSet::new();
        let mut ports = HashSet::new();
        let mut dbs = HashSet::new();
        for inst in &self.instances {
            validate_instance_id(&inst.id)?;
            if !ids.insert(inst.id.as_str()) {
                return Err(ProjectError::DuplicateInstance(inst.id.clone()));
            }
            for port in [inst.http_port, inst.gevent_port].into_iter().flatten() {
                if port < MIN_PORT {
                    return Err(ProjectError::PrivilegedPort {
                        instance: inst.id.clone(),
                        port,
                    });
                }
                if !ports.insert(port) {
                    return Err(ProjectError::PortClash(port));
                }
            }
            let db = inst.db_name(&self.name);
            if inst.db.is_none() && db.len() > crate::names::MAX_DB_NAME_LEN {
                return Err(ProjectError::DerivedDbTooLong(inst.id.clone()));
            }
            validate_db_name(&db)?;
            if !dbs.insert(db.clone()) {
                return Err(ProjectError::DatabaseClash(db));
            }
            if inst.workers > MAX_WORKERS {
                return Err(ProjectError::TooManyWorkers {
                    instance: inst.id.clone(),
                    workers: inst.workers,
                });
            }
            validate_env(&inst.env, &format!("instance {:?} env", inst.id))?;
            for arg in &inst.extra_args {
                if arg.is_empty() || arg.contains('\0') {
                    return Err(ProjectError::InvalidValue(format!(
                        "instance {:?} extra_args",
                        inst.id
                    )));
                }
                if is_managed_option(arg) {
                    return Err(ProjectError::ManagedOption {
                        instance: inst.id.clone(),
                        arg: arg.clone(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Checks the referenced sources against the global registry and returns them.
    ///
    /// # Errors
    /// Fails on unknown ids, wrong editions or mismatched versions.
    pub fn resolve_sources<'a>(
        &self,
        cfg: &'a GlobalConfig,
    ) -> Result<(&'a SourceEntry, Option<&'a SourceEntry>), ProjectError> {
        let find = |id: &str, expected: Edition| {
            let s = cfg
                .sources
                .iter()
                .find(|s| s.id == id)
                .ok_or_else(|| ProjectError::UnknownSource(id.to_owned()))?;
            if s.edition == expected {
                Ok(s)
            } else {
                Err(ProjectError::WrongEdition {
                    id: id.to_owned(),
                    expected,
                })
            }
        };
        let community = find(&self.sources.community, Edition::Community)?;
        let enterprise = match &self.sources.enterprise {
            Some(id) => {
                let e = find(id, Edition::Enterprise)?;
                if e.odoo_version != community.odoo_version {
                    return Err(ProjectError::VersionMismatch {
                        community: community.odoo_version.to_string(),
                        enterprise: e.odoo_version.to_string(),
                    });
                }
                Some(e)
            }
            None => None,
        };
        Ok((community, enterprise))
    }

    /// Custom addons paths with relative entries resolved against `project_dir`.
    #[must_use]
    pub fn resolved_addons_paths(&self, project_dir: &Path) -> Vec<PathBuf> {
        self.addons_paths
            .iter()
            .map(|p| {
                if p.is_absolute() {
                    p.clone()
                } else {
                    project_dir.join(p)
                }
            })
            .collect()
    }

    /// Parses and validates project TOML. `path` is only used in error messages.
    ///
    /// # Errors
    /// Returns a parse or validation error.
    pub fn from_toml(text: &str, path: &Path) -> Result<Self, ProjectError> {
        let p: Self = toml::from_str(text).map_err(|e| ProjectError::Parse {
            path: path.to_path_buf(),
            source: Box::new(e),
        })?;
        p.validate()?;
        Ok(p)
    }

    /// Validates and serializes to TOML.
    ///
    /// # Errors
    /// Returns a validation or serialization error.
    pub fn to_toml(&self) -> Result<String, ProjectError> {
        self.validate()?;
        Ok(toml::to_string_pretty(self)?)
    }

    /// Loads `<project_dir>/odhive.toml`.
    ///
    /// # Errors
    /// Returns [`ProjectError::NotFound`] if the file is missing, or I/O, size,
    /// parse and validation errors.
    pub fn load(project_dir: &Path) -> Result<Self, ProjectError> {
        let path = project_dir.join(PROJECT_FILE);
        match fsutil::read_private(&path, MAX_PROJECT_BYTES) {
            Ok(Some(text)) => Self::from_toml(&text, &path),
            Ok(None) => Err(ProjectError::NotFound(project_dir.to_path_buf())),
            Err(e) if e.is_too_large() => Err(ProjectError::TooLarge(e.path)),
            Err(e) => Err(ProjectError::Io {
                path: e.path,
                source: e.source,
            }),
        }
    }

    /// Atomically writes `<project_dir>/odhive.toml` with mode `0600`.
    ///
    /// # Errors
    /// Returns validation, serialization or I/O errors, and
    /// [`ProjectError::TooLarge`] for a file [`ProjectConfig::load`] would
    /// refuse.
    pub fn save(&self, project_dir: &Path) -> Result<(), ProjectError> {
        let text = self.to_toml()?;
        let path = project_dir.join(PROJECT_FILE);
        if text.len() as u64 > MAX_PROJECT_BYTES {
            return Err(ProjectError::TooLarge(path));
        }
        fsutil::write_private_atomic(&path, text.as_bytes()).map_err(|e| ProjectError::Io {
            path: e.path,
            source: e.source,
        })
    }
}

/// Whether `arg` sets one of [`MANAGED_OPTIONS`] (`--opt`, `--opt=v`, `-o`, `-ov`).
#[must_use]
pub fn is_managed_option(arg: &str) -> bool {
    MANAGED_OPTIONS.iter().any(|opt| {
        if opt.starts_with("--") {
            arg == *opt
                || arg
                    .strip_prefix(opt)
                    .is_some_and(|rest| rest.starts_with('='))
        } else {
            !arg.starts_with("--") && arg.starts_with(opt)
        }
    })
}

fn validate_env(env: &EnvVars, what: &str) -> Result<(), ProjectError> {
    for (k, v) in &env.0 {
        validate_env_key(k)?;
        if v.contains('\0') {
            return Err(ProjectError::InvalidValue(format!("{what} {k}")));
        }
    }
    Ok(())
}

fn check_addons_path(p: &Path) -> Result<(), ProjectError> {
    if p.as_os_str().is_empty() || p.as_os_str().as_encoded_bytes().contains(&0) {
        return Err(ProjectError::BadAddonsPath(p.to_path_buf()));
    }
    if p.is_absolute() {
        return Ok(());
    }
    if p.components()
        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
    {
        Ok(())
    } else {
        Err(ProjectError::BadAddonsPath(p.to_path_buf()))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::config::SourceEntry;

    fn sample() -> ProjectConfig {
        let mut p = ProjectConfig::new("acme", "odoo-19.0");
        p.sources.enterprise = Some("enterprise-19.0".into());
        p.python = Some("/usr/bin/python3.12".into());
        p.addons_paths.push("/home/dev/src/acme-addons".into());
        p.env.0.insert("PGHOST".into(), "localhost".into());
        p.env.0.insert("SECRET_TOKEN".into(), "hunter2".into());
        p.odoo_options
            .0
            .insert("admin_passwd".into(), "master-pw".into());
        p.odoo_options.0.insert("log_level".into(), "debug".into());
        let mut qj = OdooOptions::default();
        qj.0.insert("channels".into(), "root:2".into());
        p.odoo_sections.insert("queue_job".into(), qj);
        p.logs.file_keep = 9;
        let main = &mut p.instances[0];
        main.http_port = Some(8069);
        main.gevent_port = Some(8072);
        main.db = Some("acme".into());
        main.extra_args = vec!["--dev=xml".into(), "--log-level=debug".into()];
        let mut test = Instance::new("test");
        test.workers = 2;
        test.reload_mode = ReloadMode::Restart;
        test.autofix_mode = AutofixMode::Ask;
        test.sql_log = true;
        test.env.0.insert("PGHOST".into(), "/run/pg-test".into());
        test.env.0.insert("STAGE_TOKEN".into(), "s3cret".into());
        p.instances.push(test);
        p
    }

    fn registry() -> GlobalConfig {
        let mut cfg = GlobalConfig::new("/w".into());
        for (id, edition, v) in [
            ("odoo-19.0", Edition::Community, "19.0"),
            ("odoo-20.0", Edition::Community, "20.0"),
            ("enterprise-19.0", Edition::Enterprise, "19.0"),
            ("enterprise-20.0", Edition::Enterprise, "20.0"),
        ] {
            cfg.sources.push(SourceEntry {
                id: id.into(),
                edition,
                odoo_version: v.parse().expect("version"),
                path: format!("/w/sources/{id}").into(),
            });
        }
        cfg
    }

    fn err(p: &ProjectConfig) -> ProjectError {
        p.validate().expect_err("must be invalid")
    }

    #[test]
    fn toml_round_trip() {
        let p = sample();
        let text = p.to_toml().expect("serialize");
        assert_eq!(
            ProjectConfig::from_toml(&text, Path::new("t")).expect("parse"),
            p
        );
        assert!(text.contains("[[instances]]"));
        assert!(text.contains("reload_mode = \"restart\""));
        assert!(text.contains("autofix_mode = \"ask\""));
        assert_eq!(text.matches("sql_log = true").count(), 1);
        assert!(!text.contains("sql_log = false"));
        assert!(text.contains("[odoo_options]"));
        assert!(text.contains("[logs]") && text.contains("file_keep = 9"));
        assert!(text.contains("[odoo_sections.queue_job]"));
        assert!(text.contains("[instances.env]"));
    }

    #[test]
    fn instance_env_overrides_project_env() {
        let p = sample();
        let main = p.instances[0].merged_env(&p.env);
        assert_eq!(main, p.env);
        let test = p.instances[1].merged_env(&p.env);
        assert_eq!(test.0.len(), 3);
        assert_eq!(test.0["PGHOST"], "/run/pg-test");
        assert_eq!(test.0["SECRET_TOKEN"], "hunter2");
        assert_eq!(test.0["STAGE_TOKEN"], "s3cret");
    }

    #[test]
    fn module_doc_example_parses() {
        let doc = include_str!("project.rs")
            .lines()
            .skip_while(|l| !l.starts_with("//! ```toml"))
            .skip(1)
            .take_while(|l| !l.starts_with("//! ```"))
            .map(|l| l.strip_prefix("//! ").unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
        let p = ProjectConfig::from_toml(&doc, Path::new("doc")).expect("doc example");
        assert_eq!(p.instances[0].http_port, Some(8069));
        assert_eq!(p.sources.enterprise.as_deref(), Some("enterprise-19.0"));
    }

    #[test]
    fn minimal_file_uses_defaults() {
        let p = ProjectConfig::from_toml(
            "version = 1\nname = \"acme\"\n[sources]\ncommunity = \"odoo-19.0\"\n[[instances]]\nid = \"main\"\n",
            Path::new("t"),
        )
        .expect("parse");
        let i = &p.instances[0];
        assert_eq!(i.reload_mode, ReloadMode::Auto);
        assert_eq!(i.autofix_mode, AutofixMode::Auto);
        assert_eq!(i.workers, 0);
        assert!(!i.sql_log);
        assert_eq!(i.http_port, None);
        assert_eq!(i.db_name("acme"), "acme_main");
        assert!(p.addons_paths.is_empty() && p.env.is_empty());
        assert!(p.logs.is_default());
    }

    #[test]
    fn default_log_settings_are_not_written() {
        let text = ProjectConfig::new("shop", "odoo-20.0")
            .to_toml()
            .expect("toml");
        assert!(!text.contains("[logs]"), "{text}");
    }

    #[test]
    fn rejects_bad_log_settings() {
        let mut p = ProjectConfig::new("shop", "odoo-20.0");
        p.logs.file_keep = 0;
        assert!(matches!(err(&p), ProjectError::LogSettings(m) if m.contains("file_keep")));
        p.logs.file_keep = 5;
        p.logs.buffer_entries = 10_000_000;
        assert!(matches!(err(&p), ProjectError::LogSettings(m) if m.contains("buffer_entries")));
        let text = "version = 1\nname = \"acme\"\n[sources]\ncommunity = \"odoo-19.0\"\n[logs]\nkeep = 3\n[[instances]]\nid = \"main\"\n";
        assert!(ProjectConfig::from_toml(text, Path::new("t")).is_err());
    }

    #[test]
    fn new_project_is_valid() {
        let p = ProjectConfig::new("shop", "odoo-20.0");
        p.validate().expect("valid");
        assert_eq!(p.instances[0].id, "main");
        assert_eq!(p.addons_paths, vec![PathBuf::from("addons")]);
    }

    #[test]
    fn file_round_trip_is_private() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(matches!(
            ProjectConfig::load(dir.path()),
            Err(ProjectError::NotFound(_))
        ));
        let p = sample();
        p.save(dir.path()).expect("save");
        let file = dir.path().join(PROJECT_FILE);
        let mode = fs::metadata(&file).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(ProjectConfig::load(dir.path()).expect("load"), p);
    }

    #[test]
    fn a_file_too_large_to_load_is_never_written() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut p = sample();
        p.instances[0]
            .env
            .0
            .insert("BIG".into(), "x".repeat(MAX_PROJECT_BYTES as usize));
        assert!(matches!(p.save(dir.path()), Err(ProjectError::TooLarge(_))));
        assert!(!dir.path().join(PROJECT_FILE).exists());
    }

    #[test]
    fn invalid_project_is_never_written() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut p = sample();
        p.name = "Bad Name".into();
        assert!(p.save(dir.path()).is_err());
        assert!(!dir.path().join(PROJECT_FILE).exists());
    }

    #[test]
    fn env_values_are_masked_in_debug() {
        let shown = format!("{:?}", sample());
        assert!(shown.contains("SECRET_TOKEN"));
        assert!(!shown.contains("hunter2"));
        assert!(shown.contains("STAGE_TOKEN"));
        assert!(!shown.contains("s3cret"));
        assert!(!shown.contains("/run/pg-test"));
        assert!(!shown.contains("localhost"));
        assert!(!shown.contains("master-pw"));
        assert!(shown.contains("debug"));
    }

    #[test]
    fn rejects_bad_names_and_versions() {
        let mut p = sample();
        p.version = 2;
        assert!(matches!(err(&p), ProjectError::UnsupportedVersion(2)));

        let mut p = sample();
        p.name = "../etc".into();
        assert!(matches!(err(&p), ProjectError::Name(_)));

        let mut p = sample();
        p.instances[1].id = "Test".into();
        assert!(matches!(err(&p), ProjectError::Name(_)));

        let mut p = sample();
        p.instances[0].db = Some("acme; DROP DATABASE x".into());
        assert!(matches!(
            err(&p),
            ProjectError::Name(NameError::Database(_))
        ));

        let mut p = sample();
        p.env.0.insert("BAD-KEY".into(), "x".into());
        assert!(matches!(err(&p), ProjectError::Name(NameError::EnvKey(_))));

        let mut p = sample();
        p.env.0.insert("OK".into(), "a\0b".into());
        assert!(matches!(err(&p), ProjectError::InvalidValue(_)));

        let mut p = sample();
        p.instances[1].env.0.insert("1BAD".into(), "x".into());
        assert!(matches!(err(&p), ProjectError::Name(NameError::EnvKey(_))));

        let mut p = sample();
        p.instances[1].env.0.insert("OK".into(), "a\0b".into());
        assert!(matches!(err(&p), ProjectError::InvalidValue(m) if m.contains("\"test\" env OK")));
    }

    #[test]
    fn rejects_bad_instances() {
        let mut p = sample();
        p.instances.clear();
        assert!(matches!(err(&p), ProjectError::NoInstances));

        let mut p = sample();
        p.instances[1].id = "main".into();
        assert!(matches!(err(&p), ProjectError::DuplicateInstance(_)));

        let mut p = sample();
        p.instances[1].http_port = Some(80);
        assert!(matches!(
            err(&p),
            ProjectError::PrivilegedPort { port: 80, .. }
        ));

        let mut p = sample();
        p.instances[1].http_port = Some(8072);
        assert!(matches!(err(&p), ProjectError::PortClash(8072)));

        let mut p = sample();
        p.instances[0].gevent_port = Some(8069);
        assert!(matches!(err(&p), ProjectError::PortClash(8069)));

        let mut p = sample();
        p.instances[1].db = Some("acme".into());
        assert!(matches!(err(&p), ProjectError::DatabaseClash(_)));

        let mut p = sample();
        p.instances[1].workers = MAX_WORKERS + 1;
        assert!(matches!(err(&p), ProjectError::TooManyWorkers { .. }));

        let mut p = ProjectConfig::new("a".repeat(40), "odoo-19.0");
        p.instances[0].id = "b".repeat(40);
        assert!(matches!(err(&p), ProjectError::DerivedDbTooLong(_)));
        p.instances[0].db = Some("short".into());
        p.validate().expect("explicit db fixes it");

        let mut p = sample();
        p.instances[0].extra_args.push(String::new());
        assert!(matches!(err(&p), ProjectError::InvalidValue(_)));
    }

    #[test]
    fn managed_options_cannot_be_overridden() {
        for arg in [
            "-c",
            "-c/etc/odoo.conf",
            "--config",
            "--config=/etc/odoo.conf",
            "-p8070",
            "--http-port=8070",
            "--gevent-port",
            "-dprod",
            "--database=prod",
            "--addons-path=/tmp",
            "--data-dir=/",
            "--logfile=/tmp/x",
            "--workers=4",
            "-s",
        ] {
            assert!(is_managed_option(arg), "{arg}");
            let mut p = sample();
            p.instances[0].extra_args = vec![arg.into()];
            assert!(
                matches!(err(&p), ProjectError::ManagedOption { .. }),
                "{arg}"
            );
        }
        for arg in [
            "--dev=xml",
            "--dev",
            "--log-level=debug",
            "--db-filter=^acme$",
            "--config-file-is-not-a-thing",
            "--without-demo=all",
            "-u",
            "-i",
            "base",
        ] {
            assert!(!is_managed_option(arg), "{arg}");
        }
    }

    #[test]
    fn addons_and_python_paths() {
        for bad in ["../other", "addons/../../x", ""] {
            let mut p = sample();
            p.addons_paths = vec![bad.into()];
            assert!(matches!(err(&p), ProjectError::BadAddonsPath(_)), "{bad:?}");
        }
        let mut p = sample();
        p.addons_paths = vec!["./addons".into(), "vendor/oca".into(), "/abs".into()];
        p.validate().expect("valid");
        assert_eq!(
            p.resolved_addons_paths(Path::new("/w/projects/acme")),
            vec![
                PathBuf::from("/w/projects/acme/./addons"),
                PathBuf::from("/w/projects/acme/vendor/oca"),
                PathBuf::from("/abs"),
            ]
        );

        let mut p = sample();
        p.python = Some("python3".into());
        assert!(matches!(err(&p), ProjectError::RelativePython(_)));
    }

    #[test]
    fn resolves_sources_from_registry() {
        let cfg = registry();
        let p = sample();
        let (c, e) = p.resolve_sources(&cfg).expect("resolve");
        assert_eq!(c.id, "odoo-19.0");
        assert_eq!(e.map(|e| e.id.as_str()), Some("enterprise-19.0"));

        let mut p = sample();
        p.sources.community = "odoo-18.0".into();
        assert!(matches!(
            p.resolve_sources(&cfg),
            Err(ProjectError::UnknownSource(_))
        ));

        let mut p = sample();
        p.sources.community = "enterprise-19.0".into();
        assert!(matches!(
            p.resolve_sources(&cfg),
            Err(ProjectError::WrongEdition {
                expected: Edition::Community,
                ..
            })
        ));

        let mut p = sample();
        p.sources.enterprise = Some("odoo-19.0".into());
        assert!(matches!(
            p.resolve_sources(&cfg),
            Err(ProjectError::WrongEdition {
                expected: Edition::Enterprise,
                ..
            })
        ));

        let mut p = sample();
        p.sources.enterprise = Some("enterprise-20.0".into());
        assert!(matches!(
            p.resolve_sources(&cfg),
            Err(ProjectError::VersionMismatch { .. })
        ));
    }

    #[test]
    fn invalid_odoo_options_are_rejected() {
        for (key, value) in [
            ("http_port", "8069"),
            ("addons_path", "/x"),
            ("Bad Key", "1"),
            ("ok", "a\nb"),
        ] {
            let mut p = sample();
            p.odoo_options.0.insert(key.into(), value.into());
            assert!(matches!(err(&p), ProjectError::Conf(_)), "{key}");
        }
        let mut p = sample();
        p.odoo_sections
            .insert("options".into(), OdooOptions::default());
        assert!(matches!(
            err(&p),
            ProjectError::Conf(ConfError::BadSection(_))
        ));
    }

    #[test]
    fn unknown_modes_are_rejected() {
        let text = "version = 1\nname = \"acme\"\n[sources]\ncommunity = \"odoo-19.0\"\n[[instances]]\nid = \"main\"\nreload_mode = \"sometimes\"\n";
        assert!(matches!(
            ProjectConfig::from_toml(text, Path::new("t")),
            Err(ProjectError::Parse { .. })
        ));
    }
}
