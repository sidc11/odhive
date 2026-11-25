//! From a registered project to runnable instances: find the project, write
//! its `odoo.conf`, build an [`InstancePlan`] per instance, and report what
//! runs according to the PID files.
//!
//! Instances are addressed as `<project>` (every instance) or
//! `<project>#<instance>` ([`Target`]).

use std::fmt;
use std::fs::{self, DirBuilder};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use thiserror::Error;

use crate::autofix::Trusted;
use crate::conf::{self, ConfContext, ConfError, DATA_DIR};
use crate::config::{AppPaths, GlobalConfig, ProjectEntry, SourceEntry};
use crate::fsutil::DIR_MODE;
use crate::logs::LOGS_DIR;
use crate::manager::{AutoFix, InstancePlan, ManagerError, Reload};
use crate::names::{self, NameError};
use crate::pidfile::{PidFileError, PidRecord, RecordState, RunDir};
use crate::ports::PortOwner;
use crate::project::{AutofixMode, Instance, ProjectConfig, ProjectError};
use crate::python::find_executable;
use crate::reload::watch::WatchOptions;
use crate::requirements::{self, InstallOptions};
use crate::schema::{self, SchemaError, SchemaOrigin};
use crate::venv::{VENV_DIR, VenvTool, venv_python};

/// Folder for cached option schemas, inside the app data dir.
pub const SCHEMA_CACHE_DIR: &str = "schema";

/// Interface Odoo listens on unless the project sets `http_interface`.
pub const DEFAULT_HTTP_INTERFACE: &str = "127.0.0.1";

/// Time allowed for option introspection.
const SCHEMA_TIMEOUT: Duration = Duration::from_secs(30);

/// Errors from finding or preparing a project.
#[derive(Debug, Error)]
pub enum LaunchError {
    /// `<project>[#<instance>]` is malformed.
    #[error("invalid target {0:?}: use <project> or <project>#<instance>")]
    BadTarget(String),
    /// A name in the target is invalid.
    #[error(transparent)]
    Name(#[from] NameError),
    /// No project with this name is registered.
    #[error("no project named {0:?}; see `odhive project list`")]
    UnknownProject(String),
    /// The project has no such instance.
    #[error("project {project:?} has no instance {instance:?} (instances: {known})")]
    UnknownInstance {
        /// Project name.
        project: String,
        /// Requested instance.
        instance: String,
        /// Instances it has.
        known: String,
    },
    /// The project file names another project.
    #[error("{path} belongs to project {found:?}, not {expected:?}")]
    NameMismatch {
        /// Project folder.
        path: PathBuf,
        /// Registered name.
        expected: String,
        /// Name in the file.
        found: String,
    },
    /// The project has no venv yet.
    #[error("{0} has no Python environment; run `odhive project setup {1}`")]
    NoVenv(PathBuf, String),
    /// `odoo-bin` is missing from the Community source.
    #[error("{0} not found; is the Odoo source still there?")]
    NoOdooBin(PathBuf),
    /// The project file is invalid or its sources are unknown.
    #[error(transparent)]
    Project(#[from] ProjectError),
    /// `odoo.conf` could not be written.
    #[error(transparent)]
    Conf(#[from] ConfError),
    /// The option schema could not be loaded.
    #[error(transparent)]
    Schema(#[from] SchemaError),
    /// An instance plan could not be built.
    #[error(transparent)]
    Manager(#[from] ManagerError),
    /// PID files could not be read.
    #[error(transparent)]
    PidFile(#[from] PidFileError),
    /// A project folder could not be created.
    #[error("{path}: {source}")]
    Io {
        /// Path involved.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
}

/// `<project>` or `<project>#<instance>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Target {
    /// Project name.
    pub project: String,
    /// One instance, or `None` for all of them.
    pub instance: Option<String>,
}

impl Target {
    /// Every instance of `project`.
    #[must_use]
    pub fn project(project: impl Into<String>) -> Self {
        Self {
            project: project.into(),
            instance: None,
        }
    }

    /// Whether `owner` is addressed by this target.
    #[must_use]
    pub fn matches(&self, owner: &PortOwner) -> bool {
        owner.project == self.project && self.instance.as_ref().is_none_or(|i| *i == owner.instance)
    }
}

impl FromStr for Target {
    type Err = LaunchError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (project, instance) = match s.split_once('#') {
            Some((p, i)) if !i.contains('#') => (p, Some(i)),
            Some(_) => return Err(LaunchError::BadTarget(s.to_owned())),
            None => (s, None),
        };
        names::validate_project_name(project)?;
        if let Some(i) = instance {
            names::validate_instance_id(i)?;
        }
        Ok(Self {
            project: project.to_owned(),
            instance: instance.map(str::to_owned),
        })
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.instance {
            Some(i) => write!(f, "{}#{i}", self.project),
            None => f.write_str(&self.project),
        }
    }
}

/// A registered project and its settings.
#[derive(Debug, Clone)]
pub struct LoadedProject {
    /// Registry entry (name and folder).
    pub entry: ProjectEntry,
    /// `odhive.toml`.
    pub config: ProjectConfig,
}

impl LoadedProject {
    /// Project folder.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.entry.path
    }

    /// The project's log folder.
    #[must_use]
    pub fn logs_dir(&self) -> PathBuf {
        self.entry.path.join(LOGS_DIR)
    }

    /// The instances `target` selects, in project order.
    ///
    /// # Errors
    /// [`LaunchError::UnknownInstance`].
    pub fn select(&self, target: &Target) -> Result<Vec<&Instance>, LaunchError> {
        let all = &self.config.instances;
        match &target.instance {
            None => Ok(all.iter().collect()),
            Some(id) => all
                .iter()
                .find(|i| i.id == *id)
                .map(|i| vec![i])
                .ok_or_else(|| LaunchError::UnknownInstance {
                    project: self.config.name.clone(),
                    instance: id.clone(),
                    known: all
                        .iter()
                        .map(|i| i.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                }),
        }
    }
}

/// Finds `name` in the registry and loads its `odhive.toml`.
///
/// # Errors
/// [`LaunchError::UnknownProject`], [`LaunchError::NameMismatch`] or
/// [`LaunchError::Project`].
pub fn load_project(cfg: &GlobalConfig, name: &str) -> Result<LoadedProject, LaunchError> {
    let entry = cfg
        .projects
        .iter()
        .find(|p| p.name == name)
        .ok_or_else(|| LaunchError::UnknownProject(name.to_owned()))?
        .clone();
    let config = ProjectConfig::load(&entry.path)?;
    if config.name != entry.name {
        return Err(LaunchError::NameMismatch {
            path: entry.path,
            expected: entry.name,
            found: config.name,
        });
    }
    Ok(LoadedProject { entry, config })
}

/// What [`prepare`] produced.
#[derive(Debug, Clone)]
pub struct Prepared {
    /// One plan per selected instance.
    pub plans: Vec<InstancePlan>,
    /// The written `odoo.conf`.
    pub conf: PathBuf,
    /// Where the option schema came from.
    pub schema_origin: SchemaOrigin,
    /// Custom addons folders left out of `addons_path`: missing, or without
    /// any module (Odoo refuses an addons folder with no module in it).
    pub skipped_addons: Vec<PathBuf>,
    /// Why the project's requirements could not be read for the dependency
    /// auto-fix (only the built-in table is trusted then).
    pub trusted_error: Option<String>,
}

/// Writes the project's `odoo.conf` and builds the plans of `instances`.
///
/// `addons_path` lists Community `addons`, Enterprise, then the project's own
/// folders that contain at least one module; those folders are also what
/// Smart Reload watches (Odoo's own sources are not). `http_interface` defaults to
/// [`DEFAULT_HTTP_INTERFACE`], so a new instance is reachable from this
/// machine only.
///
/// # Errors
/// See [`LaunchError`].
pub async fn prepare(
    cfg: &GlobalConfig,
    paths: &AppPaths,
    project: &LoadedProject,
    instances: &[&Instance],
) -> Result<Prepared, LaunchError> {
    let dir = project.dir();
    let (community, enterprise) = project.config.resolve_sources(cfg)?;
    let python = venv_python(&dir.join(VENV_DIR));
    if !python.is_file() {
        return Err(LaunchError::NoVenv(
            dir.join(VENV_DIR),
            project.config.name.clone(),
        ));
    }
    let odoo_bin = community.path.join("odoo-bin");
    if !odoo_bin.is_file() {
        return Err(LaunchError::NoOdooBin(odoo_bin));
    }
    for sub in [DATA_DIR, LOGS_DIR] {
        let p = dir.join(sub);
        DirBuilder::new()
            .recursive(true)
            .mode(DIR_MODE)
            .create(&p)
            .map_err(|source| LaunchError::Io { path: p, source })?;
    }

    let loaded = schema::load_schema(
        community,
        Some(&python),
        &paths.data_dir.join(SCHEMA_CACHE_DIR),
        SCHEMA_TIMEOUT,
    )
    .await?;

    let StartConf {
        ctx,
        config,
        skipped_addons,
    } = start_conf(&project.config, dir, community, enterprise);
    // Smart Reload watches the custom folders Odoo loads modules from.
    let watched: Vec<PathBuf> = project
        .config
        .resolved_addons_paths(dir)
        .into_iter()
        .filter(|p| !skipped_addons.contains(p))
        .collect();

    let conf = conf::write_conf(&config, dir, &ctx, Some(&loaded.schema))?;

    let mut trusted_error = None;
    let trusted = if instances.iter().all(|i| i.autofix_mode == AutofixMode::Off) {
        Arc::new(Trusted::builtin())
    } else {
        // Same sources as the requirements install of `project setup`.
        let mut extra: Vec<PathBuf> = enterprise.iter().map(|e| e.path.clone()).collect();
        extra.extend(
            project
                .config
                .resolved_addons_paths(dir)
                .into_iter()
                .filter(|p| has_modules(p)),
        );
        match requirements::collect(&community.path, &extra) {
            Ok(plan) => Arc::new(Trusted::from_plan(&plan)),
            Err(e) => {
                trusted_error = Some(e.to_string());
                Arc::new(Trusted::builtin())
            }
        }
    };
    // uv venvs have no pip; uv installs into any venv.
    let uv = std::env::var_os("PATH").and_then(|p| find_executable(&p, "uv"));
    let install = InstallOptions::new(python.clone(), VenvTool::prefer_uv(uv));

    let plans = instances
        .iter()
        .map(|i| {
            let mut plan = InstancePlan::for_instance(
                &project.config,
                dir,
                &i.id,
                python.clone(),
                odoo_bin.clone(),
                Some(conf.clone()),
            )?;
            plan.autofix = Some(AutoFix {
                mode: i.autofix_mode,
                install: install.clone(),
                trusted: trusted.clone(),
            });
            plan.reload = Some(Reload {
                mode: i.reload_mode,
                roots: watched.clone(),
                watch: WatchOptions::default(),
            });
            Ok(plan)
        })
        .collect::<Result<Vec<_>, ManagerError>>()?;
    Ok(Prepared {
        plans,
        conf,
        schema_origin: loaded.origin,
        skipped_addons,
        trusted_error,
    })
}

/// What a start writes into `odoo.conf`: the project's options with
/// [`DEFAULT_HTTP_INTERFACE`] unless set, and the addons path without the
/// project's folders that hold no module yet (Odoo refuses those).
#[derive(Debug, Clone)]
pub struct StartConf {
    /// Generated paths.
    pub ctx: ConfContext,
    /// The project with the start defaults applied.
    pub config: ProjectConfig,
    /// Custom addons folders left out of `addons_path`.
    pub skipped_addons: Vec<PathBuf>,
}

/// See [`StartConf`].
#[must_use]
pub fn start_conf(
    project: &ProjectConfig,
    dir: &Path,
    community: &SourceEntry,
    enterprise: Option<&SourceEntry>,
) -> StartConf {
    let mut ctx = ConfContext::for_project(project, dir, community, enterprise);
    let skipped_addons: Vec<PathBuf> = project
        .resolved_addons_paths(dir)
        .into_iter()
        .filter(|p| !has_modules(p))
        .collect();
    ctx.addons_paths.retain(|p| !skipped_addons.contains(p));
    let mut config = project.clone();
    config
        .odoo_options
        .0
        .entry("http_interface".into())
        .or_insert_with(|| DEFAULT_HTTP_INTERFACE.into());
    StartConf {
        ctx,
        config,
        skipped_addons,
    }
}

/// Whether `dir` holds at least one module (a subfolder with a manifest).
#[must_use]
pub fn has_modules(dir: &Path) -> bool {
    fs::read_dir(dir).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|e| e.path().join("__manifest__.py").is_file())
    })
}

/// Where one instance stands, from the PID files. Read-only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct InstanceState {
    /// The instance.
    #[serde(flatten)]
    pub owner: PortOwner,
    /// Database it uses (or would use).
    pub db: String,
    /// Its PID record, if there is one.
    #[serde(skip)]
    pub record: Option<PidRecord>,
    /// What the record says, if there is one; `None` = stopped.
    pub run: Option<RecordState>,
}

impl InstanceState {
    /// The process runs.
    #[must_use]
    pub fn is_running(&self) -> bool {
        matches!(self.run, Some(RecordState::Running { .. }))
    }
}

/// The state of every instance of `project` that `target` selects.
///
/// # Errors
/// [`LaunchError::UnknownInstance`] or I/O errors reading the run dir.
pub fn states(
    run: &RunDir,
    project: &LoadedProject,
    target: &Target,
) -> Result<Vec<InstanceState>, LaunchError> {
    let name = &project.config.name;
    project
        .select(target)?
        .into_iter()
        .map(|i| {
            let owner = PortOwner::new(name, &i.id);
            let record = match run.read(&owner) {
                Ok(r) => r,
                // Unusable files are cleaned up by the next recovery.
                Err(PidFileError::Invalid(_)) => None,
                Err(e) => return Err(e.into()),
            };
            Ok(InstanceState {
                db: record
                    .as_ref()
                    .and_then(|r| r.db.clone())
                    .unwrap_or_else(|| i.db_name(name)),
                run: record.as_ref().map(PidRecord::state),
                record,
                owner,
            })
        })
        .collect()
}

/// Every valid record whose process runs, across all projects.
///
/// # Errors
/// I/O errors reading the run dir.
pub fn running_records(run: &RunDir) -> Result<Vec<PidRecord>, LaunchError> {
    Ok(run
        .list()?
        .into_iter()
        .filter_map(|l| l.record.ok())
        .filter(|r| matches!(r.state(), RecordState::Running { .. }))
        .collect())
}

#[cfg(test)]
mod tests;
