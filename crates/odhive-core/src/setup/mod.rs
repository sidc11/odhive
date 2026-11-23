//! Creating projects and their Python environment.
//!
//! [`create_project`] makes `<workspace>/projects/<name>/` with its folders and
//! `odhive.toml` and registers it. [`setup_env`] picks a Python interpreter,
//! creates `.venv` and installs the requirements of the project's sources and
//! addons. Both report progress as events; the caller saves the global config.

use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use serde::Serialize;
use thiserror::Error;

use crate::conf::DATA_DIR;
use crate::config::{Edition, GlobalConfig, ProjectEntry, SourceEntry};
use crate::fsutil::DIR_MODE;
use crate::launch::{LoadedProject, has_modules};
use crate::logs::LOGS_DIR;
use crate::names::{self, NameError};
use crate::project::{DEFAULT_ADDONS_DIR, Instance, ProjectConfig, ProjectError};
use crate::python::{
    self, Compat, DiscoverOptions, Interpreter, PickError, PySupport, PythonError,
};
use crate::requirements::{
    self, InstallEvent, InstallOptions, RequirementsError, Skipped, Warning,
};
use crate::sources::{self, SourceError};
use crate::venv::{self, CreateOptions, VENV_DIR, VenvError, VenvEvent, VenvTool};
use crate::version::OdooVersion;
use crate::workspace::Workspace;

/// Backups folder inside a project.
pub const BACKUPS_DIR: &str = "backups";

/// Errors while creating a project or its environment.
#[derive(Debug, Error)]
pub enum SetupError {
    /// The project name is invalid.
    #[error(transparent)]
    Name(#[from] NameError),
    /// A project with this name is registered already.
    #[error("a project named {0:?} already exists")]
    AlreadyRegistered(String),
    /// The project folder exists already.
    #[error("{0} already exists; remove it or pick another name")]
    Exists(PathBuf),
    /// No source matches the id, and it is not a path either.
    #[error("no {edition:?} source {arg:?}; give a registered id or the path of an Odoo tree")]
    UnknownSource {
        /// What was given.
        arg: String,
        /// Edition looked for.
        edition: Edition,
    },
    /// An addons folder is not an existing directory.
    #[error("addons path {0} is not a directory")]
    BadAddonsPath(PathBuf),
    /// Registering a source failed.
    #[error(transparent)]
    Source(#[from] SourceError),
    /// The project settings are invalid.
    #[error(transparent)]
    Project(#[from] ProjectError),
    /// The chosen interpreter does not work.
    #[error(transparent)]
    Python(#[from] PythonError),
    /// The chosen interpreter is older than the source supports.
    #[error("{path} is Python {version}, but this Odoo needs {support}")]
    PythonTooOld {
        /// Interpreter.
        path: PathBuf,
        /// Its version.
        version: python::PyVersion,
        /// What the source supports.
        support: PySupport,
    },
    /// No suitable interpreter was found.
    #[error(transparent)]
    Pick(#[from] PickError),
    /// The venv could not be created.
    #[error(transparent)]
    Venv(#[from] VenvError),
    /// The existing venv has no interpreter.
    #[error("{0} is not a working venv; recreate it")]
    BrokenVenv(PathBuf),
    /// Requirements could not be collected or installed.
    #[error(transparent)]
    Requirements(#[from] RequirementsError),
    /// Filesystem error.
    #[error("{path}: {source}")]
    Io {
        /// Path involved.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> SetupError + '_ {
    move |source| SetupError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Finds a registered Community source by id, or registers the Odoo tree at
/// `id_or_path`.
///
/// # Errors
/// [`SetupError::UnknownSource`] or registration errors.
pub fn community_source(
    cfg: &mut GlobalConfig,
    id_or_path: &str,
) -> Result<SourceEntry, SetupError> {
    if let Some(s) = find_source(cfg, id_or_path, Edition::Community) {
        return Ok(s);
    }
    let path = as_path(id_or_path, Edition::Community)?;
    Ok(sources::register_community(cfg, &path)?)
}

/// Finds a registered Enterprise source by id, or registers the Enterprise
/// tree at `id_or_path` for `version`.
///
/// # Errors
/// [`SetupError::UnknownSource`] or registration errors.
pub fn enterprise_source(
    cfg: &mut GlobalConfig,
    id_or_path: &str,
    version: OdooVersion,
) -> Result<SourceEntry, SetupError> {
    if let Some(s) = find_source(cfg, id_or_path, Edition::Enterprise) {
        return Ok(s);
    }
    let path = as_path(id_or_path, Edition::Enterprise)?;
    Ok(sources::register_enterprise(cfg, &path, version)?)
}

fn find_source(cfg: &GlobalConfig, id: &str, edition: Edition) -> Option<SourceEntry> {
    cfg.sources
        .iter()
        .find(|s| s.id == id && s.edition == edition)
        .cloned()
}

fn as_path(arg: &str, edition: Edition) -> Result<PathBuf, SetupError> {
    let path = PathBuf::from(arg);
    if arg.contains('/') || path.exists() {
        Ok(path)
    } else {
        Err(SetupError::UnknownSource {
            arg: arg.to_owned(),
            edition,
        })
    }
}

/// What a new project is made of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewProject {
    /// Project name.
    pub name: String,
    /// Registered Community source id.
    pub community: String,
    /// Registered Enterprise source id.
    pub enterprise: Option<String>,
    /// Extra addons folders (existing directories), after `addons/`.
    pub addons_paths: Vec<PathBuf>,
    /// Interpreter to build the venv with; `None` = detect.
    pub python: Option<PathBuf>,
    /// The project's first instance.
    pub instance: Instance,
}

/// Creates `<workspace>/projects/<name>/` with `addons/`, `data/`, `logs/`,
/// `backups/` and `odhive.toml`, and adds it to `cfg` (not saved). On error
/// nothing is left behind.
///
/// # Errors
/// See [`SetupError`].
pub fn create_project(
    cfg: &mut GlobalConfig,
    ws: &Workspace,
    new: &NewProject,
) -> Result<LoadedProject, SetupError> {
    names::validate_project_name(&new.name)?;
    if cfg.projects.iter().any(|p| p.name == new.name) {
        return Err(SetupError::AlreadyRegistered(new.name.clone()));
    }
    let mut config = ProjectConfig::new(&new.name, &new.community);
    config.sources.enterprise.clone_from(&new.enterprise);
    config.python.clone_from(&new.python);
    config.instances = vec![new.instance.clone()];
    for p in &new.addons_paths {
        let real = fs::canonicalize(p)
            .ok()
            .filter(|r| r.is_dir())
            .ok_or_else(|| SetupError::BadAddonsPath(p.clone()))?;
        if !config.addons_paths.contains(&real) {
            config.addons_paths.push(real);
        }
    }
    config.validate()?;
    config.resolve_sources(cfg)?;

    let dir = ws.projects_dir().join(&new.name);
    DirBuilder::new().mode(DIR_MODE).create(&dir).map_err(|e| {
        if e.kind() == io::ErrorKind::AlreadyExists {
            SetupError::Exists(dir.clone())
        } else {
            io_err(&dir)(e)
        }
    })?;
    let guard = RemoveOnError(Some(dir.clone()));
    for sub in [DEFAULT_ADDONS_DIR, DATA_DIR, LOGS_DIR, BACKUPS_DIR] {
        let p = dir.join(sub);
        DirBuilder::new()
            .mode(DIR_MODE)
            .create(&p)
            .map_err(io_err(&p))?;
    }
    config.save(&dir)?;
    guard.keep();

    let entry = ProjectEntry {
        name: new.name.clone(),
        path: dir,
    };
    cfg.projects.push(entry.clone());
    Ok(LoadedProject { entry, config })
}

/// Removes `name` from the registry and returns its entry, or `None` if no
/// project has this name. Its files are never deleted.
#[must_use]
pub fn unregister_project(cfg: &mut GlobalConfig, name: &str) -> Option<ProjectEntry> {
    let idx = cfg.projects.iter().position(|p| p.name == name)?;
    Some(cfg.projects.remove(idx))
}

struct RemoveOnError(Option<PathBuf>);

impl RemoveOnError {
    fn keep(mut self) {
        self.0 = None;
    }
}

impl Drop for RemoveOnError {
    fn drop(&mut self) {
        if let Some(dir) = self.0.take()
            && let Err(e) = fs::remove_dir_all(&dir)
        {
            tracing::warn!(path = %dir.display(), error = %e, "could not remove partial project");
        }
    }
}

/// How [`setup_env`] runs.
#[derive(Debug, Clone)]
pub struct EnvOptions {
    /// Use this interpreter (stored in `odhive.toml`); `None` = the project's
    /// setting, else the newest suitable one found.
    pub python: Option<PathBuf>,
    /// Where to look for interpreters and uv.
    pub discover: DiscoverOptions,
    /// Replace an existing `.venv`.
    pub recreate: bool,
    /// Install requirements.
    pub install: bool,
}

impl EnvOptions {
    /// Detect Python and uv from the environment, keep an existing venv,
    /// install requirements.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            python: None,
            discover: DiscoverOptions::from_env(),
            recreate: false,
            install: true,
        }
    }
}

/// Progress of [`setup_env`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case", tag = "step")]
pub enum SetupEvent {
    /// The interpreter for the venv.
    Python {
        /// Interpreter.
        #[schema(value_type = String)]
        path: PathBuf,
        /// Its version.
        #[schema(value_type = String)]
        version: python::PyVersion,
        /// How it relates to what Odoo supports.
        compat: Compat,
        /// What Odoo supports.
        support: PySupport,
    },
    /// Venv creation progress.
    Venv(VenvEvent),
    /// An existing venv is kept.
    VenvKept {
        /// Venv folder.
        #[schema(value_type = String)]
        dir: PathBuf,
    },
    /// Requirements were collected.
    Collected {
        /// Requirements to install.
        count: usize,
        /// Modules whose manifests were read.
        modules: usize,
        /// Entries that are not installed.
        skipped: Vec<Skipped>,
        /// Problems while collecting.
        warnings: Vec<Warning>,
    },
    /// Requirements install progress.
    Install(InstallEvent),
}

/// What [`setup_env`] did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct EnvReport {
    /// Base interpreter (`None` when an existing venv was kept).
    #[schema(value_type = Option<String>)]
    pub base: Option<PathBuf>,
    /// The venv's interpreter.
    #[schema(value_type = String)]
    pub python: PathBuf,
    /// Requirements passed to the installer.
    pub installed: usize,
}

/// Creates the project's `.venv` and installs its requirements. An explicit
/// interpreter is stored in `odhive.toml`.
///
/// # Errors
/// See [`SetupError`].
pub async fn setup_env(
    cfg: &GlobalConfig,
    project: &mut LoadedProject,
    opts: &EnvOptions,
    mut on_event: impl FnMut(SetupEvent),
) -> Result<EnvReport, SetupError> {
    let dir = project.entry.path.clone();
    let (community, enterprise) = project.config.resolve_sources(cfg)?;
    let (community, enterprise) = (community.clone(), enterprise.cloned());
    let uv = opts.discover.uv.clone();
    let venv_dir = dir.join(VENV_DIR);

    let venv_exists = fs::symlink_metadata(&venv_dir).is_ok();
    let (base, python) = if venv_exists && !opts.recreate {
        let python = venv::venv_python(&venv_dir);
        if !python.is_file() {
            return Err(SetupError::BrokenVenv(venv_dir));
        }
        on_event(SetupEvent::VenvKept { dir: venv_dir });
        (None, python)
    } else {
        let support = PySupport::for_source(&community.path, community.odoo_version);
        let base = choose_python(project, opts, support, &mut on_event).await?;
        let mut create = CreateOptions::new(base.clone(), VenvTool::prefer_uv(uv.clone()));
        create.prompt = Some(project.config.name.clone());
        create.replace = opts.recreate;
        let created = venv::create(&dir, &create, |e| on_event(SetupEvent::Venv(e))).await?;
        if let Some(explicit) = &opts.python
            && project.config.python.as_ref() != Some(explicit)
        {
            project.config.python = Some(explicit.clone());
            project.config.save(&dir)?;
        }
        (Some(base), created.python)
    };

    let mut installed = 0;
    if opts.install {
        let mut extra: Vec<PathBuf> = enterprise.iter().map(|e| e.path.clone()).collect();
        extra.extend(
            project
                .config
                .resolved_addons_paths(&dir)
                .into_iter()
                .filter(|p| has_modules(p)),
        );
        let plan = requirements::collect(&community.path, &extra)?;
        on_event(SetupEvent::Collected {
            count: plan.requirements.len(),
            modules: plan.modules,
            skipped: plan.skipped.clone(),
            warnings: plan.warnings.clone(),
        });
        // uv installs into any venv; pip only into one that has pip.
        let install = InstallOptions::new(python.clone(), VenvTool::prefer_uv(uv));
        requirements::install(&plan, &install, |e| on_event(SetupEvent::Install(e))).await?;
        installed = plan.requirements.len();
    }
    Ok(EnvReport {
        base,
        python,
        installed,
    })
}

async fn choose_python(
    project: &LoadedProject,
    opts: &EnvOptions,
    support: PySupport,
    on_event: &mut impl FnMut(SetupEvent),
) -> Result<PathBuf, SetupError> {
    let explicit = opts.python.as_ref().or(project.config.python.as_ref());
    let interpreter: Interpreter = match explicit {
        Some(path) => {
            let probed = python::probe(path, opts.discover.timeout).await?;
            if support.check(probed.interpreter.version) == Compat::TooOld {
                return Err(SetupError::PythonTooOld {
                    path: path.clone(),
                    version: probed.interpreter.version,
                    support,
                });
            }
            // Keep the path as given: a shim (mise, pyenv) dispatches on it.
            Interpreter {
                path: path.clone(),
                ..probed.interpreter
            }
        }
        None => {
            let found = python::discover(&opts.discover).await;
            python::pick(&found.interpreters, support)?.clone()
        }
    };
    on_event(SetupEvent::Python {
        path: interpreter.path.clone(),
        version: interpreter.version,
        compat: support.check(interpreter.version),
        support,
    });
    Ok(interpreter.path)
}

#[cfg(test)]
mod tests;
