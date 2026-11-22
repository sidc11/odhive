//! Project virtual environment: `projects/<name>/.venv`.
//!
//! Created with `uv venv` when uv is available, else `python -I -m venv`; always
//! through an argument vector, never a shell. The tool's output is streamed as
//! [`VenvEvent::Output`] lines.
//!
//! The venv is built in place (venvs are not relocatable: `bin/activate` and
//! script shebangs hold the absolute path). A guard removes a partly created
//! `.venv` on any error, timeout, or when the future is dropped (cancel). An
//! existing `.venv` is never touched unless [`CreateOptions::replace`] is set,
//! and even then only if it looks like a venv (has `pyvenv.cfg`) and is a real
//! directory, not a symlink.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use thiserror::Error;
use tokio::process::Command;

use crate::proc::{self, RunError};
use crate::python::{self, PyVersion, PythonError};

/// Name of the venv directory inside a project.
pub const VENV_DIR: &str = ".venv";

/// Default time venv creation may take (stdlib `venv` runs `ensurepip`).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// Lines of tool output kept for error messages.
const OUTPUT_TAIL_LINES: usize = 20;

/// Errors returned while creating a venv.
#[derive(Debug, Error)]
pub enum VenvError {
    /// `.venv` already exists and `replace` was not set.
    #[error("{0} already exists; recreate it to replace it")]
    AlreadyExists(PathBuf),
    /// `.venv` exists but is not something OdHive may delete.
    #[error("{path} exists but {reason}; remove it yourself")]
    NotAVenv {
        /// Path involved.
        path: PathBuf,
        /// Why it is refused.
        reason: &'static str,
    },
    /// The project directory is missing or not a directory.
    #[error("project directory {0} does not exist")]
    NoProjectDir(PathBuf),
    /// Python's `venv` module cannot create environments here (Debian/Ubuntu
    /// without `python3-venv`).
    #[error(
        "{python}: the venv module is unavailable; install your distribution's python3-venv package (or install uv)"
    )]
    StdlibVenvMissing {
        /// Interpreter used.
        python: PathBuf,
    },
    /// The tool could not be started.
    #[error("{path}: cannot run: {source}")]
    Spawn {
        /// Executable path.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
    /// The tool exited with an error.
    #[error("{tool} failed with {status}:\n{output}")]
    Failed {
        /// Tool label.
        tool: VenvTool,
        /// Exit status.
        status: std::process::ExitStatus,
        /// Last lines of output.
        output: String,
    },
    /// Creation took longer than allowed.
    #[error("venv creation did not finish within {0:?}")]
    Timeout(Duration),
    /// The new venv's interpreter does not work as expected.
    #[error("created venv is unusable: {0}")]
    Verify(String),
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

/// Which tool creates the venv.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case", tag = "kind", content = "path")]
pub enum VenvTool {
    /// `uv venv`, with this uv executable.
    #[schema(value_type = String)]
    Uv(PathBuf),
    /// `python -m venv`.
    Stdlib,
}

impl VenvTool {
    /// uv if `uv` is given, else the stdlib module.
    #[must_use]
    pub fn prefer_uv(uv: Option<PathBuf>) -> Self {
        uv.map_or(Self::Stdlib, Self::Uv)
    }
}

impl std::fmt::Display for VenvTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Uv(_) => f.write_str("uv venv"),
            Self::Stdlib => f.write_str("python -m venv"),
        }
    }
}

/// Progress of [`create`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case", tag = "event")]
pub enum VenvEvent {
    /// An existing venv is being removed (`replace`).
    Removing {
        /// Venv directory.
        #[schema(value_type = String)]
        dir: PathBuf,
    },
    /// The tool was started.
    Started {
        /// Tool used.
        tool: VenvTool,
        /// Base interpreter.
        #[schema(value_type = String)]
        python: PathBuf,
        /// Venv directory.
        #[schema(value_type = String)]
        dir: PathBuf,
    },
    /// One line of tool output (stdout or stderr).
    Output {
        /// The line, without its terminator.
        line: String,
    },
    /// The venv exists and its interpreter answered.
    Finished {
        /// Venv interpreter (`.venv/bin/python`).
        #[schema(value_type = String)]
        python: PathBuf,
        /// Its version.
        #[schema(value_type = String)]
        version: PyVersion,
    },
}

/// Inputs to [`create`].
#[derive(Debug, Clone)]
pub struct CreateOptions {
    /// Base interpreter (absolute path, e.g. from [`python::pick`]).
    pub python: PathBuf,
    /// Tool to use.
    pub tool: VenvTool,
    /// Shell prompt prefix (usually the project name).
    pub prompt: Option<String>,
    /// Remove an existing `.venv` first.
    pub replace: bool,
    /// Time allowed for the tool plus verification.
    pub timeout: Duration,
}

impl CreateOptions {
    /// Default options for `python` and `tool`.
    #[must_use]
    pub fn new(python: PathBuf, tool: VenvTool) -> Self {
        Self {
            python,
            tool,
            prompt: None,
            replace: false,
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

/// A created venv.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Venv {
    /// Venv directory.
    pub dir: PathBuf,
    /// Its interpreter (`bin/python`).
    pub python: PathBuf,
    /// Interpreter version.
    pub version: PyVersion,
    /// Tool that created it.
    pub tool: VenvTool,
}

/// Interpreter path inside a venv directory.
#[must_use]
pub fn venv_python(dir: &Path) -> PathBuf {
    dir.join("bin").join("python")
}

/// Creates `<project_dir>/.venv`.
///
/// # Errors
/// See [`VenvError`]. On error, a partly created `.venv` has been removed.
pub async fn create(
    project_dir: &Path,
    opts: &CreateOptions,
    mut on_event: impl FnMut(VenvEvent),
) -> Result<Venv, VenvError> {
    if !project_dir.is_dir() {
        return Err(VenvError::NoProjectDir(project_dir.to_path_buf()));
    }
    let dir = project_dir.join(VENV_DIR);
    prepare_target(&dir, opts.replace, &mut on_event)?;

    let base = python::probe(&opts.python, python::DEFAULT_PROBE_TIMEOUT)
        .await
        .map_err(|e| VenvError::Verify(format!("base interpreter: {e}")))?;
    if base.venv {
        return Err(VenvError::Verify(format!(
            "base interpreter {} is itself a virtual environment",
            opts.python.display()
        )));
    }

    // From here on `.venv` is ours: removed unless creation succeeds.
    let mut guard = Cleanup::new(dir.clone());
    let work = async {
        on_event(VenvEvent::Started {
            tool: opts.tool.clone(),
            python: opts.python.clone(),
            dir: dir.clone(),
        });
        run_tool(&dir, opts, &mut on_event).await?;
        verify(&dir, base.interpreter.version).await
    };
    let venv = tokio::time::timeout(opts.timeout, work)
        .await
        .map_err(|_| VenvError::Timeout(opts.timeout))??;
    guard.disarm();
    on_event(VenvEvent::Finished {
        python: venv_python(&dir),
        version: venv,
    });
    Ok(Venv {
        python: venv_python(&dir),
        dir,
        version: venv,
        tool: opts.tool.clone(),
    })
}

/// Checks that `dir` is free, or removes an existing venv there when `replace`.
fn prepare_target(
    dir: &Path,
    replace: bool,
    on_event: &mut impl FnMut(VenvEvent),
) -> Result<(), VenvError> {
    let meta = match fs::symlink_metadata(dir) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(VenvError::Io {
                path: dir.to_path_buf(),
                source,
            });
        }
    };
    if !replace {
        return Err(VenvError::AlreadyExists(dir.to_path_buf()));
    }
    let refuse = |reason| VenvError::NotAVenv {
        path: dir.to_path_buf(),
        reason,
    };
    if meta.file_type().is_symlink() {
        return Err(refuse("is a symlink"));
    }
    if !meta.is_dir() {
        return Err(refuse("is not a directory"));
    }
    if !dir.join("pyvenv.cfg").is_file() {
        return Err(refuse("has no pyvenv.cfg (not a virtual environment)"));
    }
    on_event(VenvEvent::Removing {
        dir: dir.to_path_buf(),
    });
    fs::remove_dir_all(dir).map_err(|source| VenvError::Io {
        path: dir.to_path_buf(),
        source,
    })
}

async fn run_tool(
    dir: &Path,
    opts: &CreateOptions,
    on_event: &mut impl FnMut(VenvEvent),
) -> Result<(), VenvError> {
    let (exe, mut cmd) = match &opts.tool {
        VenvTool::Uv(uv) => {
            let mut c = Command::new(uv);
            c.args([
                "venv",
                "--no-config",
                "--no-project",
                "--no-python-downloads",
                "--python",
            ])
            .arg(&opts.python);
            if let Some(p) = &opts.prompt {
                c.arg("--prompt").arg(p);
            }
            c.arg("--").arg(dir);
            c.env("NO_COLOR", "1");
            (uv.clone(), c)
        }
        VenvTool::Stdlib => {
            let mut c = Command::new(&opts.python);
            c.args(["-I", "-m", "venv"]);
            if let Some(p) = &opts.prompt {
                c.arg("--prompt").arg(p);
            }
            c.arg("--").arg(dir);
            (opts.python.clone(), c)
        }
    };
    // An activated venv or a Python override in OdHive's own environment must
    // not leak into the new one.
    for var in proc::PYTHON_ENV_OVERRIDES {
        cmd.env_remove(var);
    }
    cmd.env("LC_ALL", "C");
    let done = proc::run_streaming(cmd, OUTPUT_TAIL_LINES, |line| {
        on_event(VenvEvent::Output { line });
    })
    .await
    .map_err(|e| match e {
        RunError::Spawn(source) => VenvError::Spawn { path: exe, source },
        RunError::Io(source) => VenvError::Io { path: exe, source },
    })?;
    if done.status.success() {
        return Ok(());
    }
    // Debian/Ubuntu wrap this message: "...because ensurepip is not\navailable."
    if opts.tool == VenvTool::Stdlib && done.tail.contains("ensurepip is not") {
        return Err(VenvError::StdlibVenvMissing {
            python: opts.python.clone(),
        });
    }
    Err(VenvError::Failed {
        tool: opts.tool.clone(),
        status: done.status,
        output: done.tail,
    })
}

/// Probes the venv's interpreter: it must report being a venv of `expected`.
async fn verify(dir: &Path, expected: PyVersion) -> Result<PyVersion, VenvError> {
    let python = venv_python(dir);
    let probed = python::probe(&python, python::DEFAULT_PROBE_TIMEOUT)
        .await
        .map_err(|e: PythonError| VenvError::Verify(e.to_string()))?;
    if !probed.venv {
        return Err(VenvError::Verify(format!(
            "{} does not run as a virtual environment",
            python.display()
        )));
    }
    let got = probed.interpreter.version;
    if got.minor_version() != expected.minor_version() {
        return Err(VenvError::Verify(format!(
            "venv runs Python {got}, expected {expected}"
        )));
    }
    Ok(got)
}

/// Removes a directory on drop unless disarmed.
struct Cleanup {
    dir: Option<PathBuf>,
}

impl Cleanup {
    fn new(dir: PathBuf) -> Self {
        Self { dir: Some(dir) }
    }

    fn disarm(&mut self) {
        self.dir = None;
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(dir) = self.dir.take() {
            // Only a real directory we were creating; never follow a symlink.
            if fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir())
                && let Err(e) = fs::remove_dir_all(&dir)
            {
                tracing::warn!(dir = %dir.display(), error = %e, "could not remove partial venv");
            }
        }
    }
}

#[cfg(test)]
mod tests;
