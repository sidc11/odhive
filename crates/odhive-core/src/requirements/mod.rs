//! Installing a project's Python requirements into its venv.
//!
//! [`collect`] builds an [`InstallPlan`] from three trusted sources:
//! - Odoo's own `requirements.txt` (Community source root),
//! - `requirements.txt` at the root of each extra addons path (Enterprise,
//!   custom addons),
//! - `external_dependencies['python']` of every installable module directly
//!   inside those addons paths, read with the literal-only parser in
//!   [`manifest`] — manifests are never executed.
//!
//! Every line goes through [`spec::parse_requirement`]: only named PEP 508
//! requirements survive. pip options (`--index-url`, `-e`, `-r`, ...), URLs and
//! paths are reported in [`InstallPlan::skipped`] and never reach the
//! installer, so a requirements file cannot redirect where packages come from.
//!
//! [`install`] writes the surviving requirements to a private temporary file
//! and runs `uv pip install` (uv venvs have no pip) or `python -m pip install`
//! against the venv interpreter, through an argument vector. Output is
//! streamed; dropping the future (cancel, timeout) kills the installer's
//! whole process group. Packages already installed by then stay installed.

pub mod manifest;
pub mod spec;

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use thiserror::Error;
use tokio::process::Command;

use crate::proc::{self, RunError};
use crate::python;
use crate::syslib::{self, Hint};
use crate::venv::VenvTool;
pub use manifest::{MANIFEST_FILE, Manifest, ManifestError};
pub use spec::Requirement;

/// Requirements file name, in the Odoo source and in addons paths.
pub const REQUIREMENTS_FILE: &str = "requirements.txt";

/// Largest requirements file read.
pub const MAX_REQUIREMENTS_BYTES: u64 = 1024 * 1024;

/// Most module directories scanned per addons path.
pub const MAX_MODULES_PER_PATH: usize = 10_000;

/// Default time an install may take (some packages build from source).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Lines of installer output kept for error messages.
const OUTPUT_TAIL_LINES: usize = 30;

/// Errors from [`collect`] and [`install`].
#[derive(Debug, Error)]
pub enum RequirementsError {
    /// The Odoo source has no `requirements.txt`.
    #[error("{0} not found; is this an Odoo source checkout?")]
    NoOdooRequirements(PathBuf),
    /// A file is larger than [`MAX_REQUIREMENTS_BYTES`].
    #[error("{0}: larger than {MAX_REQUIREMENTS_BYTES} bytes")]
    TooLarge(PathBuf),
    /// The target interpreter is not inside a virtual environment.
    #[error("{0} is not a virtual environment interpreter; refusing to install into it")]
    NotAVenv(PathBuf),
    /// The target interpreter does not run.
    #[error("{path}: {message}")]
    Python {
        /// Interpreter path.
        path: PathBuf,
        /// What went wrong.
        message: String,
    },
    /// The installer could not be started.
    #[error("{path}: cannot run: {source}")]
    Spawn {
        /// Executable path.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
    /// The installer exited with an error.
    #[error("{tool} failed with {status}:\n{output}{}", hint.as_ref().map(|h| format!("\nhint: {h}")).unwrap_or_default())]
    Failed {
        /// Installer label.
        tool: String,
        /// Exit status.
        status: std::process::ExitStatus,
        /// Last lines of output.
        output: String,
        /// What is probably missing on the system, when recognised.
        hint: Option<Box<Hint>>,
    },
    /// The install took longer than allowed.
    #[error("requirements install did not finish within {0:?}")]
    Timeout(Duration),
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

/// Where a requirement came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Origin {
    /// A line of a requirements file.
    File {
        /// The file.
        #[schema(value_type = String)]
        path: PathBuf,
        /// 1-based line number.
        line: usize,
    },
    /// A module manifest's `external_dependencies['python']`.
    Manifest {
        /// The manifest file.
        #[schema(value_type = String)]
        path: PathBuf,
        /// Module (directory) name.
        module: String,
    },
    /// A Python module an instance reported missing ([`crate::autofix`]).
    Missing {
        /// The import name.
        module: String,
    },
}

/// A requirement and its first origin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct Entry {
    /// Normalised requirement.
    pub requirement: Requirement,
    /// Where it was first seen.
    pub origin: Origin,
}

/// Something found but not installed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct Skipped {
    /// Where it was found.
    pub origin: Origin,
    /// The text as written.
    pub text: String,
    /// Why it is not installed.
    pub reason: String,
}

/// A problem that did not stop collection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct Warning {
    /// Path involved.
    #[schema(value_type = String)]
    pub path: PathBuf,
    /// What is wrong.
    pub message: String,
}

/// What [`install`] will install.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct InstallPlan {
    /// Requirements in collection order, exact duplicates removed.
    pub requirements: Vec<Entry>,
    /// Entries refused by validation.
    pub skipped: Vec<Skipped>,
    /// Unreadable manifests, missing addons paths and similar.
    pub warnings: Vec<Warning>,
    /// Requirements files read.
    pub files: Vec<PathBuf>,
    /// Installable modules whose manifests were read.
    pub modules: usize,
}

impl InstallPlan {
    /// True when there is nothing to install.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.requirements.is_empty()
    }

    fn add(&mut self, seen: &mut BTreeSet<Requirement>, text: &str, origin: Origin) {
        match spec::parse_requirement(text) {
            Ok(requirement) => {
                if seen.insert(requirement.clone()) {
                    self.requirements.push(Entry {
                        requirement,
                        origin,
                    });
                }
            }
            Err(reason) => self.skipped.push(Skipped {
                origin,
                text: text.to_owned(),
                reason,
            }),
        }
    }
}

/// Collects the requirements of an Odoo source plus extra addons paths
/// (Enterprise, custom). Odoo's bundled addons are not scanned: its
/// `requirements.txt` covers them.
///
/// # Errors
/// Only when Odoo's own `requirements.txt` is missing or unreadable; problems
/// in addons paths become [`InstallPlan::warnings`].
pub fn collect(
    odoo_source: &Path,
    addons_paths: &[PathBuf],
) -> Result<InstallPlan, RequirementsError> {
    let mut plan = InstallPlan::default();
    let mut seen = BTreeSet::new();

    let odoo_req = odoo_source.join(REQUIREMENTS_FILE);
    if !odoo_req.is_file() {
        return Err(RequirementsError::NoOdooRequirements(odoo_req));
    }
    read_requirements_file(&odoo_req, &mut plan, &mut seen)?;

    for root in addons_paths {
        if !root.is_dir() {
            plan.warnings.push(Warning {
                path: root.clone(),
                message: "addons path does not exist or is not a directory".into(),
            });
            continue;
        }
        let req = root.join(REQUIREMENTS_FILE);
        if req.is_file()
            && let Err(e) = read_requirements_file(&req, &mut plan, &mut seen)
        {
            plan.warnings.push(Warning {
                path: req,
                message: e.to_string(),
            });
        }
        scan_modules(root, &mut plan, &mut seen);
    }
    Ok(plan)
}

fn read_requirements_file(
    path: &Path,
    plan: &mut InstallPlan,
    seen: &mut BTreeSet<Requirement>,
) -> Result<(), RequirementsError> {
    let io_err = |source| RequirementsError::Io {
        path: path.to_path_buf(),
        source,
    };
    let file = fs::File::open(path).map_err(io_err)?;
    let mut bytes = Vec::new();
    file.take(MAX_REQUIREMENTS_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(io_err)?;
    if bytes.len() as u64 > MAX_REQUIREMENTS_BYTES {
        return Err(RequirementsError::TooLarge(path.to_path_buf()));
    }
    let text = String::from_utf8_lossy(&bytes);
    plan.files.push(path.to_path_buf());
    for (line, text) in spec::logical_lines(&text) {
        plan.add(
            seen,
            &text,
            Origin::File {
                path: path.to_path_buf(),
                line,
            },
        );
    }
    Ok(())
}

fn scan_modules(root: &Path, plan: &mut InstallPlan, seen: &mut BTreeSet<Requirement>) {
    let entries = match fs::read_dir(root) {
        Ok(e) => e,
        Err(e) => {
            plan.warnings.push(Warning {
                path: root.to_path_buf(),
                message: e.to_string(),
            });
            return;
        }
    };
    let mut modules: Vec<(String, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let manifest = e.path().join(MANIFEST_FILE);
            manifest.is_file().then_some((name, manifest))
        })
        .take(MAX_MODULES_PER_PATH + 1)
        .collect();
    if modules.len() > MAX_MODULES_PER_PATH {
        plan.warnings.push(Warning {
            path: root.to_path_buf(),
            message: format!("more than {MAX_MODULES_PER_PATH} modules; the rest were not scanned"),
        });
        modules.truncate(MAX_MODULES_PER_PATH);
    }
    modules.sort();
    for (module, path) in modules {
        match manifest::read_manifest(&path) {
            Ok(m) if !m.installable => {}
            Ok(m) => {
                plan.modules += 1;
                for dep in m.python_dependencies {
                    plan.add(
                        seen,
                        &dep,
                        Origin::Manifest {
                            path: path.clone(),
                            module: module.clone(),
                        },
                    );
                }
            }
            Err(e) => plan.warnings.push(Warning {
                path,
                message: e.to_string(),
            }),
        }
    }
}

/// Inputs to [`install`].
#[derive(Debug, Clone)]
pub struct InstallOptions {
    /// The venv's interpreter (`.venv/bin/python`).
    pub python: PathBuf,
    /// Installer: `uv pip` for [`VenvTool::Uv`], `python -m pip` for
    /// [`VenvTool::Stdlib`] (the tool that created the venv).
    pub tool: VenvTool,
    /// Time allowed.
    pub timeout: Duration,
}

impl InstallOptions {
    /// Default options.
    #[must_use]
    pub fn new(python: PathBuf, tool: VenvTool) -> Self {
        Self {
            python,
            tool,
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

/// Progress of [`install`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case", tag = "event")]
pub enum InstallEvent {
    /// The installer was started.
    Started {
        /// Installer label (`uv pip install` / `pip install`).
        tool: String,
        /// Target interpreter.
        #[schema(value_type = String)]
        python: PathBuf,
        /// Number of requirements passed.
        count: usize,
    },
    /// One line of installer output.
    Output {
        /// The line.
        line: String,
    },
    /// The installer succeeded.
    Finished {
        /// Number of requirements passed.
        count: usize,
    },
}

/// Installs `plan` into the venv of `opts.python`.
///
/// # Errors
/// See [`RequirementsError`].
pub async fn install(
    plan: &InstallPlan,
    opts: &InstallOptions,
    mut on_event: impl FnMut(InstallEvent),
) -> Result<(), RequirementsError> {
    let count = plan.requirements.len();
    if count == 0 {
        on_event(InstallEvent::Finished { count });
        return Ok(());
    }
    let probed = python::probe(&opts.python, python::DEFAULT_PROBE_TIMEOUT)
        .await
        .map_err(|e| RequirementsError::Python {
            path: opts.python.clone(),
            message: e.to_string(),
        })?;
    if !probed.venv {
        return Err(RequirementsError::NotAVenv(opts.python.clone()));
    }

    // Private working dir (0700) holding the validated requirements; also the
    // installer's cwd, so no project file there can configure it.
    let work = tempfile::Builder::new()
        .prefix("odhive-install-")
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
        .map_err(|source| RequirementsError::Io {
            path: std::env::temp_dir(),
            source,
        })?;
    let req_file = work.path().join(REQUIREMENTS_FILE);
    write_requirements(&req_file, plan).map_err(|source| RequirementsError::Io {
        path: req_file.clone(),
        source,
    })?;

    let (exe, label, mut cmd) = installer_command(opts, &req_file);
    for var in proc::PYTHON_ENV_OVERRIDES {
        cmd.env_remove(var);
    }
    cmd.current_dir(work.path())
        .env("NO_COLOR", "1")
        .env("PIP_NO_INPUT", "1")
        .env("PIP_DISABLE_PIP_VERSION_CHECK", "1");

    on_event(InstallEvent::Started {
        tool: label.clone(),
        python: opts.python.clone(),
        count,
    });
    let mut hint = None;
    let run = proc::run_streaming(cmd, OUTPUT_TAIL_LINES, |line| {
        if hint.is_none() {
            hint = syslib::detect_build_failure(&line).map(|l| Box::new(Hint::for_host(l)));
        }
        on_event(InstallEvent::Output { line });
    });
    let done = tokio::time::timeout(opts.timeout, run)
        .await
        .map_err(|_| RequirementsError::Timeout(opts.timeout))?
        .map_err(|e| match e {
            RunError::Spawn(source) => RequirementsError::Spawn {
                path: exe.clone(),
                source,
            },
            RunError::Io(source) => RequirementsError::Io {
                path: exe.clone(),
                source,
            },
        })?;
    if !done.status.success() {
        return Err(RequirementsError::Failed {
            tool: label,
            status: done.status,
            output: done.tail,
            hint,
        });
    }
    on_event(InstallEvent::Finished { count });
    Ok(())
}

fn write_requirements(path: &Path, plan: &InstallPlan) -> io::Result<()> {
    let mut f = fs::File::create_new(path)?;
    for e in &plan.requirements {
        // Validated: one line each, no options, no newlines.
        writeln!(f, "{}", e.requirement)?;
    }
    f.sync_all()
}

/// `(executable, label, command)` for the chosen installer.
fn installer_command(opts: &InstallOptions, req_file: &Path) -> (PathBuf, String, Command) {
    match &opts.tool {
        VenvTool::Uv(uv) => {
            let mut c = Command::new(uv);
            c.args(["pip", "install", "--no-config", "--python"])
                .arg(&opts.python)
                .arg("-r")
                .arg(req_file);
            (uv.clone(), "uv pip install".into(), c)
        }
        VenvTool::Stdlib => {
            let mut c = Command::new(&opts.python);
            c.args([
                "-I",
                "-m",
                "pip",
                "install",
                "--require-virtualenv",
                "--no-input",
                "--disable-pip-version-check",
                "--progress-bar",
                "off",
                "-r",
            ])
            .arg(req_file);
            (opts.python.clone(), "pip install".into(), c)
        }
    }
}

#[cfg(test)]
mod tests;
