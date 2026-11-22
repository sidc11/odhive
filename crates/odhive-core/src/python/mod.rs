//! Python interpreter discovery.
//!
//! Candidates come from two places: `python3` / `python3.N` executables on `PATH`,
//! and `uv python list --only-installed` when uv is available. What uv or the file
//! name claim is never trusted: every candidate is probed by running a fixed
//! script (`probe.py`) as `python -I -B -c <script>`, without a shell, and the
//! interpreter's own answer decides its version, real path and flavour.
//!
//! [`PySupport`] holds the Python range an Odoo source accepts, read from
//! `MIN_PY_VERSION` / `MAX_PY_VERSION` in `odoo/release.py` (text only, never
//! executed). [`pick`] chooses the newest supported, standard CPython build.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::process::Command;

use crate::proc::read_capped;
use crate::version::{self, OdooVersion};

/// The probe script passed to `python -c`.
pub const PROBE_PY: &str = include_str!("probe.py");

/// Default time one interpreter (or `uv python list`) may take to answer.
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Output larger than this from a probe or from uv is rejected.
pub const MAX_PROBE_OUTPUT_BYTES: u64 = 64 * 1024;

/// Output larger than this from `uv python list` is rejected.
pub const MAX_UV_OUTPUT_BYTES: u64 = 1024 * 1024;

/// At most this many candidates are probed.
pub const MAX_CANDIDATES: usize = 64;

const PROBE_FORMAT: u32 = 1;
const STDERR_TAIL_BYTES: usize = 2048;

/// Errors returned while probing an interpreter.
#[derive(Debug, Error)]
pub enum PythonError {
    /// The executable could not be started.
    #[error("{path}: cannot run: {source}")]
    Spawn {
        /// Executable path.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// Waiting for or reading from the child failed.
    #[error("{path}: {source}")]
    Io {
        /// Executable path.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The child did not finish in time and was killed.
    #[error("{path}: no answer within {timeout:?}")]
    Timeout {
        /// Executable path.
        path: PathBuf,
        /// Time allowed.
        timeout: Duration,
    },
    /// The child exited with an error (not Python 3, or broken).
    #[error("{path}: exited with {status}: {stderr}")]
    Failed {
        /// Executable path.
        path: PathBuf,
        /// Exit status.
        status: std::process::ExitStatus,
        /// Tail of stderr.
        stderr: String,
    },
    /// The child printed something that is not a probe result.
    #[error("{path}: unexpected output: {reason}")]
    BadOutput {
        /// Executable path.
        path: PathBuf,
        /// What was wrong.
        reason: String,
    },
}

/// A Python version `major.minor.patch`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PyVersion {
    /// Major version (always 3 for accepted interpreters).
    pub major: u8,
    /// Minor version.
    pub minor: u8,
    /// Micro / patch version.
    pub patch: u16,
}

impl PyVersion {
    /// Builds a version.
    #[must_use]
    pub const fn new(major: u8, minor: u8, patch: u16) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// `(major, minor)`, the part Odoo compares.
    #[must_use]
    pub const fn minor_version(&self) -> (u8, u8) {
        (self.major, self.minor)
    }
}

impl fmt::Display for PyVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl Serialize for PyVersion {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for PyVersion {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        let mut parts = text.split('.');
        let parsed = (|| {
            let v = Self::new(
                parts.next()?.parse().ok()?,
                parts.next()?.parse().ok()?,
                parts.next()?.parse().ok()?,
            );
            parts.next().is_none().then_some(v)
        })();
        parsed.ok_or_else(|| serde::de::Error::custom(format!("invalid Python version {text:?}")))
    }
}

/// How a Python version relates to what an Odoo source supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Compat {
    /// Within `MIN_PY_VERSION ..= MAX_PY_VERSION`.
    Supported,
    /// Older than `MIN_PY_VERSION`: Odoo refuses to start.
    TooOld,
    /// Newer than `MAX_PY_VERSION`: Odoo starts with a warning, dependencies may not build.
    TooNew,
}

/// The Python range (inclusive, by `major.minor`) an Odoo source supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct PySupport {
    /// Oldest supported `(major, minor)`.
    #[schema(value_type = Vec<u8>)]
    pub min: (u8, u8),
    /// Newest supported `(major, minor)`.
    #[schema(value_type = Vec<u8>)]
    pub max: (u8, u8),
}

impl PySupport {
    /// Range shipped by Odoo 19.0 / 20.0 (checked 2026-11-21), used when a source
    /// tree's `release.py` does not state it.
    #[must_use]
    pub fn for_odoo(version: OdooVersion) -> Self {
        match version.major {
            19 => Self {
                min: (3, 10),
                max: (3, 14),
            },
            _ => Self {
                min: (3, 12),
                max: (3, 14),
            },
        }
    }

    /// Reads `MIN_PY_VERSION = (3, 10)` / `MAX_PY_VERSION = (3, 14)` from
    /// `release.py` text. Returns `None` unless both are present and sane.
    #[must_use]
    pub fn from_release_py(text: &str) -> Option<Self> {
        let min = text
            .lines()
            .find_map(|l| version_tuple(l, "MIN_PY_VERSION"))?;
        let max = text
            .lines()
            .find_map(|l| version_tuple(l, "MAX_PY_VERSION"))?;
        (min.0 == 3 && max.0 == 3 && min <= max).then_some(Self { min, max })
    }

    /// Range for a source tree: from its `release.py`, else [`Self::for_odoo`].
    #[must_use]
    pub fn for_source(source: &Path, version: OdooVersion) -> Self {
        version::read_release_py(source)
            .ok()
            .and_then(|t| Self::from_release_py(&t))
            .unwrap_or_else(|| Self::for_odoo(version))
    }

    /// Classifies `v` against this range.
    #[must_use]
    pub fn check(&self, v: PyVersion) -> Compat {
        let mv = v.minor_version();
        if mv < self.min {
            Compat::TooOld
        } else if mv > self.max {
            Compat::TooNew
        } else {
            Compat::Supported
        }
    }
}

impl fmt::Display for PySupport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Python {}.{}–{}.{}",
            self.min.0, self.min.1, self.max.0, self.max.1
        )
    }
}

/// Where a candidate was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// An explicitly given path.
    Explicit,
    /// A `python3*` executable on `PATH`.
    Path,
    /// `uv python list --only-installed`.
    Uv,
}

/// A probed, working Python interpreter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct Interpreter {
    /// Real interpreter executable (`sys.executable`, symlinks resolved).
    pub path: PathBuf,
    /// Version reported by the interpreter itself.
    pub version: PyVersion,
    /// `sys.implementation.name`, e.g. `cpython`.
    pub implementation: String,
    /// Built without the GIL (`python3.Nt`).
    pub free_threaded: bool,
    /// Every place this interpreter was found, in discovery order.
    pub origins: Vec<Origin>,
}

impl Interpreter {
    /// A standard CPython build (what Odoo's dependencies are built for).
    #[must_use]
    pub fn is_standard_cpython(&self) -> bool {
        self.implementation == "cpython" && !self.free_threaded
    }
}

/// A candidate that was not accepted, with the reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct Skipped {
    /// Candidate path as found.
    pub path: PathBuf,
    /// Human-readable reason.
    pub reason: String,
}

/// Result of [`discover`].
#[derive(Debug, Clone, Default, Serialize, serde::Deserialize)]
pub struct Discovery {
    /// Working interpreters, deduplicated by real path, in discovery order.
    pub interpreters: Vec<Interpreter>,
    /// Candidates that failed to probe or were rejected.
    pub skipped: Vec<Skipped>,
    /// Why uv was not used, if it was configured but failed.
    pub uv_error: Option<String>,
}

/// Inputs to [`discover`]; [`Self::from_env`] gives the normal setup.
#[derive(Debug, Clone)]
pub struct DiscoverOptions {
    /// `PATH`-style directory list to scan for `python3*`; `None` scans nothing.
    pub path_var: Option<OsString>,
    /// uv executable; `None` skips uv.
    pub uv: Option<PathBuf>,
    /// Extra interpreters to probe first (e.g. one the user typed in).
    pub explicit: Vec<PathBuf>,
    /// Time each probe may take.
    pub timeout: Duration,
}

impl DiscoverOptions {
    /// Scans the process `PATH`, and uses `uv` if it is on that `PATH`.
    #[must_use]
    pub fn from_env() -> Self {
        let path_var = std::env::var_os("PATH");
        let uv = path_var.as_deref().and_then(|p| find_executable(p, "uv"));
        Self {
            path_var,
            uv,
            explicit: Vec::new(),
            timeout: DEFAULT_PROBE_TIMEOUT,
        }
    }
}

/// Finds and probes every Python 3 interpreter. Never fails: problems are
/// reported in [`Discovery::skipped`] / [`Discovery::uv_error`].
pub async fn discover(opts: &DiscoverOptions) -> Discovery {
    let mut out = Discovery::default();
    let mut candidates: Vec<(PathBuf, Origin)> = opts
        .explicit
        .iter()
        .map(|p| (p.clone(), Origin::Explicit))
        .collect();
    if let Some(path_var) = &opts.path_var {
        candidates.extend(
            path_candidates(path_var)
                .into_iter()
                .map(|p| (p, Origin::Path)),
        );
    }
    if let Some(uv) = &opts.uv {
        match uv_installed(uv, opts.timeout).await {
            Ok(paths) => candidates.extend(paths.into_iter().map(|p| (p, Origin::Uv))),
            Err(e) => out.uv_error = Some(e),
        }
    }

    // Candidate path → index of the interpreter it probed to (`None`: skipped).
    let mut seen: HashMap<PathBuf, Option<usize>> = HashMap::new();
    for (path, origin) in candidates {
        if out.interpreters.len() + out.skipped.len() >= MAX_CANDIDATES {
            break;
        }
        // A path reached twice (e.g. PATH and uv) is probed once. Paths are not
        // canonicalized first: version-manager shims (mise, pyenv) are links to
        // one dispatcher binary that picks the Python from argv[0]. Different
        // paths to one interpreter are merged after probing, by `sys.executable`.
        if let Some(idx) = seen.get(&path) {
            if let Some(i) = idx.map(|idx| &mut out.interpreters[idx])
                && !i.origins.contains(&origin)
            {
                i.origins.push(origin);
            }
            continue;
        }
        seen.insert(path.clone(), None);
        if let Some(reason) = excluded(&path) {
            out.skipped.push(Skipped { path, reason });
            continue;
        }
        match probe(&path, opts.timeout).await {
            Ok(found) if found.venv => out.skipped.push(Skipped {
                path,
                reason: "is a virtual environment".into(),
            }),
            Ok(found) => {
                let idx = merge(&mut out.interpreters, found.interpreter, origin);
                seen.insert(path, Some(idx));
            }
            Err(e) => out.skipped.push(Skipped {
                path,
                reason: e.to_string(),
            }),
        }
    }
    out
}

/// Interpreter facts from one probe run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probed {
    /// The interpreter (with empty `origins`).
    pub interpreter: Interpreter,
    /// `sys.prefix != sys.base_prefix`.
    pub venv: bool,
}

/// Runs the probe script with `python` and parses its answer.
///
/// # Errors
/// Fails if the executable cannot start, errors, times out, or prints anything
/// other than a valid Python 3 probe result.
pub async fn probe(python: &Path, timeout: Duration) -> Result<Probed, PythonError> {
    let (out, _) = run_capped(
        Command::new(python).args(["-I", "-B", "-c", PROBE_PY]),
        python,
        timeout,
        MAX_PROBE_OUTPUT_BYTES,
    )
    .await?;
    parse_probe(python, &out)
}

#[derive(Deserialize)]
struct RawProbe {
    format: u32,
    version: Vec<u32>,
    executable: PathBuf,
    implementation: String,
    free_threaded: bool,
    venv: bool,
}

fn parse_probe(python: &Path, out: &[u8]) -> Result<Probed, PythonError> {
    let bad = |reason: String| PythonError::BadOutput {
        path: python.to_path_buf(),
        reason,
    };
    let text = std::str::from_utf8(out).map_err(|_| bad("not UTF-8".into()))?;
    let raw: RawProbe =
        serde_json::from_str(text.trim()).map_err(|e| bad(format!("not a probe result: {e}")))?;
    if raw.format != PROBE_FORMAT {
        return Err(bad(format!("probe format {}", raw.format)));
    }
    let [major, minor, patch] = raw.version[..] else {
        return Err(bad(format!("version {:?}", raw.version)));
    };
    let version = match (
        u8::try_from(major),
        u8::try_from(minor),
        u16::try_from(patch),
    ) {
        (Ok(3), Ok(minor), Ok(patch)) => PyVersion::new(3, minor, patch),
        _ => return Err(bad(format!("not Python 3: {major}.{minor}.{patch}"))),
    };
    if raw.implementation.is_empty()
        || !raw
            .implementation
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err(bad(format!("implementation {:?}", raw.implementation)));
    }
    // `sys.executable` can be empty or relative in odd embeddings; fall back to
    // the path we ran, resolved.
    let exe = if raw.executable.is_absolute() {
        raw.executable
    } else {
        python.to_path_buf()
    };
    let path = fs::canonicalize(&exe).map_err(|e| bad(format!("{}: {e}", exe.display())))?;
    Ok(Probed {
        interpreter: Interpreter {
            path,
            version,
            implementation: raw.implementation,
            free_threaded: raw.free_threaded,
            origins: Vec::new(),
        },
        venv: raw.venv,
    })
}

/// Why [`pick`] found nothing.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PickError {
    /// No Python 3 interpreter was found at all.
    #[error("no Python 3 interpreter found; install {0} (for example with `uv python install`)")]
    NoneFound(PySupport),
    /// Interpreters were found but none fits.
    #[error("no suitable interpreter for {support}; found: {found}")]
    NoneSupported {
        /// Required range.
        support: PySupport,
        /// Short list of what was found.
        found: String,
    },
}

/// Picks the newest standard CPython within `support`. Ties keep discovery order.
///
/// # Errors
/// Returns [`PickError`] describing what was found when nothing fits.
pub fn pick(interpreters: &[Interpreter], support: PySupport) -> Result<&Interpreter, PickError> {
    let best = interpreters
        .iter()
        .filter(|i| i.is_standard_cpython() && support.check(i.version) == Compat::Supported)
        .fold(None::<&Interpreter>, |best, i| match best {
            Some(b) if b.version >= i.version => Some(b),
            _ => Some(i),
        });
    if let Some(best) = best {
        return Ok(best);
    }
    if interpreters.is_empty() {
        return Err(PickError::NoneFound(support));
    }
    let found = interpreters
        .iter()
        .map(|i| {
            let note = if i.implementation != "cpython" {
                format!(" ({})", i.implementation)
            } else if i.free_threaded {
                " (free-threaded)".to_owned()
            } else {
                String::new()
            };
            format!("{}{note}", i.version)
        })
        .collect::<Vec<_>>()
        .join(", ");
    Err(PickError::NoneSupported { support, found })
}

/// Adds `found` or merges it into an entry with the same real path; returns its index.
fn merge(list: &mut Vec<Interpreter>, mut found: Interpreter, origin: Origin) -> usize {
    if let Some(idx) = list.iter().position(|i| i.path == found.path) {
        if !list[idx].origins.contains(&origin) {
            list[idx].origins.push(origin);
        }
        idx
    } else {
        found.origins = vec![origin];
        list.push(found);
        list.len() - 1
    }
}

/// Candidates that must not be probed at all.
fn excluded(path: &Path) -> Option<String> {
    // macOS `/usr/bin/python3` is a shim that may open the "install developer
    // tools" dialog, and is Python 3.9 at best (too old for Odoo 19/20).
    if cfg!(target_os = "macos") && path.starts_with("/usr/bin") {
        return Some("macOS system Python stub (too old for Odoo)".into());
    }
    None
}

/// `python3` and `python3.N` (`N` digits, optional `t` for free-threaded) in
/// every `PATH` directory, in `PATH` order, executable regular files only.
fn path_candidates(path_var: &OsStr) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in std::env::split_paths(path_var) {
        // Relative PATH entries depend on the cwd; never use them.
        if !dir.is_absolute() {
            continue;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        let mut names: Vec<OsString> = entries
            .filter_map(Result::ok)
            .map(|e| e.file_name())
            .filter(|n| n.to_str().is_some_and(is_python_name))
            .collect();
        names.sort();
        out.extend(
            names
                .into_iter()
                .map(|n| dir.join(n))
                .filter(|p| is_executable_file(p)),
        );
    }
    out
}

fn is_python_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("python3") else {
        return false;
    };
    if rest.is_empty() {
        return true;
    }
    let Some(minor) = rest.strip_prefix('.') else {
        return false;
    };
    let minor = minor.strip_suffix('t').unwrap_or(minor);
    !minor.is_empty() && minor.len() <= 3 && minor.bytes().all(|b| b.is_ascii_digit())
}

fn is_executable_file(p: &Path) -> bool {
    fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// First executable `name` in a `PATH`-style list (absolute entries only).
#[must_use]
pub fn find_executable(path_var: &OsStr, name: &str) -> Option<PathBuf> {
    std::env::split_paths(path_var)
        .filter(|d| d.is_absolute())
        .map(|d| d.join(name))
        .find(|p| is_executable_file(p))
}

#[derive(Deserialize)]
struct UvEntry {
    path: Option<PathBuf>,
}

/// Paths of interpreters uv has installed or found.
async fn uv_installed(uv: &Path, timeout: Duration) -> Result<Vec<PathBuf>, String> {
    let (out, _) = run_capped(
        Command::new(uv)
            .args([
                "python",
                "list",
                "--only-installed",
                "--output-format",
                "json",
                "--no-config",
                "--offline",
            ])
            .env("NO_COLOR", "1")
            .env("UV_NO_PROGRESS", "1"),
        uv,
        timeout,
        MAX_UV_OUTPUT_BYTES,
    )
    .await
    .map_err(|e| e.to_string())?;
    parse_uv_list(&out)
}

fn parse_uv_list(out: &[u8]) -> Result<Vec<PathBuf>, String> {
    let entries: Vec<UvEntry> = serde_json::from_slice(out)
        .map_err(|e| format!("uv python list: unexpected output: {e}"))?;
    Ok(entries
        .into_iter()
        .filter_map(|e| e.path)
        .filter(|p| p.is_absolute())
        .collect())
}

/// Runs `cmd` (no stdin, piped stdout/stderr, killed on timeout) and returns its
/// stdout and stderr if it exits successfully.
async fn run_capped(
    cmd: &mut Command,
    exe: &Path,
    timeout: Duration,
    cap: u64,
) -> Result<(Vec<u8>, Vec<u8>), PythonError> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|source| PythonError::Spawn {
            path: exe.to_path_buf(),
            source,
        })?;
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    let run = async {
        let (out, err) = tokio::join!(read_capped(stdout, cap), read_capped(stderr, cap));
        let status = child.wait().await?;
        Ok::<_, std::io::Error>((out?, err?, status))
    };
    let (out, err, status) = tokio::time::timeout(timeout, run)
        .await
        .map_err(|_| PythonError::Timeout {
            path: exe.to_path_buf(),
            timeout,
        })?
        .map_err(|source| PythonError::Io {
            path: exe.to_path_buf(),
            source,
        })?;
    if !status.success() {
        let tail = &err[err.len().saturating_sub(STDERR_TAIL_BYTES)..];
        return Err(PythonError::Failed {
            path: exe.to_path_buf(),
            status,
            stderr: String::from_utf8_lossy(tail).trim().to_owned(),
        });
    }
    if out.len() as u64 > cap {
        return Err(PythonError::BadOutput {
            path: exe.to_path_buf(),
            reason: "output too large".into(),
        });
    }
    Ok((out, err))
}

/// Parses a top-level `NAME = (3, 10)` line.
fn version_tuple(line: &str, name: &str) -> Option<(u8, u8)> {
    let rest = line.strip_prefix(name)?.trim_start().strip_prefix('=')?;
    if rest.starts_with('=') {
        return None;
    }
    let inner = rest.trim().strip_prefix('(')?;
    let (inner, _) = inner.split_once(')')?;
    let mut parts = inner.split(',').map(str::trim);
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    match parts.next() {
        None | Some("") => Some((major, minor)),
        Some(_) => None,
    }
}

#[cfg(test)]
mod tests;
