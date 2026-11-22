//! Odoo option schema: every `odoo-bin server` option with its config key, type,
//! default, help and group.
//!
//! The schema comes from the installed Odoo itself: [`introspect`] runs a fixed
//! script (`introspect.py`) with the project's Python, which reads Odoo's option
//! parser and prints JSON. This is used instead of parsing `--help` text because
//! the config builder needs what `--help` does not show: the config-file key
//! (`--db-filter` → `dbfilter`, `--no-http` → `http_enable`), defaults, and
//! file-only options such as `admin_passwd`.
//!
//! Results are cached per source. Until a venv exists, [`bundled`] provides the
//! schema captured from Odoo 19.0 and 20.0.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::process::Command;

use crate::config::{Edition, SourceEntry};
use crate::fsutil;
use crate::proc::read_capped;
use crate::version::{OdooVersion, RELEASE_PY};

/// The introspection script passed to `python -c`.
pub const INTROSPECT_PY: &str = include_str!("introspect.py");

const BUNDLED_19: &str = include_str!("odoo-19.json");
const BUNDLED_20: &str = include_str!("odoo-20.json");

/// Output format version printed by [`INTROSPECT_PY`].
pub const RAW_FORMAT: u32 = 1;
/// Cache file format version.
pub const CACHE_FORMAT: u32 = 1;
/// Default time limit for introspection (first imports of Odoo can be slow).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
/// Introspection stdout larger than this is rejected.
pub const MAX_OUTPUT_BYTES: u64 = 4 * 1024 * 1024;
/// More options than this is not a real Odoo.
pub const MAX_OPTIONS: usize = 2000;
/// Config keys whose values are secrets (masked in UI, API, logs).
pub const SECRET_KEYS: &[&str] = &[
    "admin_passwd",
    "db_password",
    "smtp_password",
    "proxy_access_token",
];

const STDERR_TAIL_BYTES: usize = 4096;
const CONFIG_PY: &str = "odoo/tools/config.py";

/// Errors returned while building an option schema.
#[derive(Debug, Error)]
pub enum SchemaError {
    /// The Python executable could not be started.
    #[error("could not start {path}: {source}")]
    Spawn {
        /// Python path.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// Python exited with an error.
    #[error("option introspection failed ({status}): {stderr}")]
    Failed {
        /// Exit status.
        status: std::process::ExitStatus,
        /// Last part of stderr.
        stderr: String,
    },
    /// Python did not finish in time.
    #[error("option introspection timed out after {0:?}")]
    Timeout(Duration),
    /// Output exceeded [`MAX_OUTPUT_BYTES`].
    #[error("option introspection printed more than {MAX_OUTPUT_BYTES} bytes")]
    OutputTooLarge,
    /// Reading the process output failed.
    #[error("reading introspection output: {0}")]
    Io(#[from] std::io::Error),
    /// Output was not the expected JSON.
    #[error("invalid introspection output: {0}")]
    Json(#[from] serde_json::Error),
    /// Output was JSON but not a usable schema.
    #[error("invalid option schema: {0}")]
    Invalid(String),
    /// The tree reports a different version than the registry.
    #[error("source reports Odoo {found}, registry says {expected}")]
    VersionMismatch {
        /// Version in the registry.
        expected: String,
        /// Version reported by Odoo.
        found: String,
    },
    /// Option schemas only exist for Community sources (they hold `odoo-bin`).
    #[error("source {0:?} is not a Community source")]
    NotCommunity(String),
}

/// Value type of an option.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum OptionKind {
    /// `true` / `false`.
    Bool,
    /// Integer.
    Int,
    /// Floating point number.
    Float,
    /// Free text.
    String,
    /// File system path.
    Path,
    /// Comma-separated list.
    List,
    /// One of a fixed set of values.
    Choice {
        /// Allowed values, in Odoo's order.
        choices: Vec<String>,
    },
}

/// One option, merged across all flags that share a config key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct OdooOption {
    /// Config-file key (`dest`), e.g. `http_port`.
    pub key: String,
    /// Command-line flags, e.g. `["-p", "--http-port"]`. Empty for file-only options.
    pub flags: Vec<String>,
    /// Value type.
    #[serde(flatten)]
    pub kind: OptionKind,
    /// Default value, if Odoo declares one.
    pub default: Option<Value>,
    /// Help text, if shown by `--help`.
    pub help: Option<String>,
    /// `--help` section, e.g. `HTTP Service Configuration`; `None` for file-only options.
    pub group: Option<String>,
    /// Metavar shown by `--help`, e.g. `PORT`.
    pub metavar: Option<String>,
    /// Can be read from `odoo.conf`.
    pub file_loadable: bool,
    /// Is written to `odoo.conf` by `--save`.
    pub file_exportable: bool,
    /// Value must be masked everywhere.
    pub secret: bool,
}

/// All options of one Odoo series.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct OptionSchema {
    /// Series reported by Odoo, e.g. `19.0`.
    pub series: String,
    /// Options in Odoo's order (file-only first, then by `--help` group).
    pub options: Vec<OdooOption>,
}

impl OptionSchema {
    /// Looks up an option by config key.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&OdooOption> {
        self.options.iter().find(|o| o.key == key)
    }

    /// Looks up an option by command-line flag (`--http-port`, `-p`).
    #[must_use]
    pub fn by_flag(&self, flag: &str) -> Option<&OdooOption> {
        self.options
            .iter()
            .find(|o| o.flags.iter().any(|f| f == flag))
    }

    /// Group titles in display order (`None` = file-only options).
    #[must_use]
    pub fn groups(&self) -> Vec<Option<&str>> {
        let mut out: Vec<Option<&str>> = Vec::new();
        for o in &self.options {
            let g = o.group.as_deref();
            if !out.contains(&g) {
                out.push(g);
            }
        }
        out
    }

    /// Parses the JSON printed by [`INTROSPECT_PY`].
    ///
    /// # Errors
    /// Returns [`SchemaError`] for malformed JSON or an unusable schema.
    pub fn from_raw_json(text: &str) -> Result<Self, SchemaError> {
        let raw: RawSchema = serde_json::from_str(text)?;
        if raw.format != RAW_FORMAT {
            return Err(SchemaError::Invalid(format!(
                "unsupported format {}",
                raw.format
            )));
        }
        if raw.options.is_empty() || raw.options.len() > MAX_OPTIONS {
            return Err(SchemaError::Invalid(format!(
                "{} options",
                raw.options.len()
            )));
        }
        let mut options: Vec<OdooOption> = Vec::new();
        for r in raw.options {
            if r.dest.is_empty()
                || !r
                    .dest
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_')
            {
                return Err(SchemaError::Invalid(format!("bad option key {:?}", r.dest)));
            }
            match options.iter_mut().find(|o| o.key == r.dest) {
                Some(existing) => merge(existing, r),
                None => options.push(convert(r)),
            }
        }
        Ok(Self {
            series: raw.series,
            options,
        })
    }
}

#[derive(Deserialize)]
struct RawSchema {
    format: u32,
    series: String,
    options: Vec<RawOption>,
}

#[derive(Deserialize)]
struct RawOption {
    dest: String,
    flags: Vec<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    action: String,
    default: Value,
    help: Option<String>,
    metavar: Option<String>,
    choices: Option<Vec<String>>,
    group: Option<String>,
    file_loadable: bool,
    file_exportable: bool,
}

fn kind_of(r: &RawOption) -> Option<OptionKind> {
    let kind = match r.kind.as_deref()? {
        "int" => OptionKind::Int,
        "float" => OptionKind::Float,
        "bool" | "without_demo" => OptionKind::Bool,
        "path" => OptionKind::Path,
        "comma" | "addons_path" | "upgrade_path" | "pre_upgrade_scripts" => OptionKind::List,
        "choice" => OptionKind::Choice {
            choices: r.choices.clone().unwrap_or_default(),
        },
        // `string` and types added by future Odoo versions.
        _ => OptionKind::String,
    };
    Some(kind)
}

fn convert(r: RawOption) -> OdooOption {
    // Flags without a value type are booleans (`store_true`, `store_false`), except
    // optparse's `count` action.
    let kind = kind_of(&r).unwrap_or(if r.action == "count" {
        OptionKind::Int
    } else {
        OptionKind::Bool
    });
    // optparse fills a metavar even for flags that take no value.
    let metavar = r.kind.as_ref().and(r.metavar);
    OdooOption {
        secret: SECRET_KEYS.contains(&r.dest.as_str()),
        key: r.dest,
        flags: r.flags,
        kind,
        default: (!r.default.is_null()).then_some(r.default),
        help: r.help,
        group: r.group,
        metavar,
        file_loadable: r.file_loadable,
        file_exportable: r.file_exportable,
    }
}

/// Folds a second flag for the same key (`--without-demo` after `--with-demo`,
/// `--log-web` after `--log-handler`) into the existing option.
fn merge(o: &mut OdooOption, r: RawOption) {
    for f in &r.flags {
        if !o.flags.contains(f) {
            o.flags.push(f.clone());
        }
    }
    if o.default.is_none() && !r.default.is_null() {
        o.default = Some(r.default.clone());
    }
    if o.help.is_none() {
        o.help.clone_from(&r.help);
    }
    if o.group.is_none() {
        o.group.clone_from(&r.group);
    }
    o.file_loadable |= r.file_loadable;
    o.file_exportable |= r.file_exportable;
}

/// The schema captured from Odoo 19.0 / 20.0, used until a venv can be introspected.
/// saas series use the bundle of their major version.
///
/// # Panics
/// Never for a valid [`OdooVersion`]: both bundles are verified by tests.
#[must_use]
pub fn bundled(version: OdooVersion) -> OptionSchema {
    let text = if version.major == 19 {
        BUNDLED_19
    } else {
        BUNDLED_20
    };
    OptionSchema::from_raw_json(text).expect("bundled schema is valid")
}

/// Runs [`INTROSPECT_PY`] with `python` against the Community tree at `source`.
///
/// Runs `python -I -B -c <script> <source>` without a shell (isolated mode: no
/// `PYTHON*` env vars, no user site, no cwd on `sys.path`; no `.pyc` written into
/// the source tree), with `ODOO_RC=/dev/null` so the user's `~/.odoorc` is ignored.
///
/// # Errors
/// Fails if Python cannot start, exits with an error, times out, or prints an
/// unusable schema.
pub async fn introspect(
    python: &Path,
    source: &Path,
    timeout: Duration,
) -> Result<OptionSchema, SchemaError> {
    let mut child = Command::new(python)
        .args(["-I", "-B", "-c", INTROSPECT_PY])
        .arg(source)
        .current_dir(source)
        .env("ODOO_RC", "/dev/null")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|source| SchemaError::Spawn {
            path: python.to_path_buf(),
            source,
        })?;
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");

    let run = async {
        let (out, err) = tokio::join!(
            read_capped(stdout, MAX_OUTPUT_BYTES),
            read_capped(stderr, MAX_OUTPUT_BYTES)
        );
        let status = child.wait().await?;
        Ok::<_, std::io::Error>((out?, err?, status))
    };
    let (out, err, status) = tokio::time::timeout(timeout, run)
        .await
        .map_err(|_| SchemaError::Timeout(timeout))??;

    if !status.success() {
        let tail = &err[err.len().saturating_sub(STDERR_TAIL_BYTES)..];
        return Err(SchemaError::Failed {
            status,
            stderr: String::from_utf8_lossy(tail).trim().to_owned(),
        });
    }
    if out.len() as u64 > MAX_OUTPUT_BYTES {
        return Err(SchemaError::OutputTooLarge);
    }
    OptionSchema::from_raw_json(&String::from_utf8_lossy(&out))
}

/// Where a schema came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaOrigin {
    /// Read from the cache; the source tree is unchanged since it was built.
    Cached,
    /// Freshly introspected (and cached).
    Introspected,
    /// Bundled fallback; `reason` says why introspection was not used.
    Bundled {
        /// Human-readable reason.
        reason: String,
    },
}

/// A schema and where it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedSchema {
    /// The schema.
    pub schema: OptionSchema,
    /// Its origin.
    pub origin: SchemaOrigin,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FileStamp {
    len: u64,
    modified_ns: u128,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Fingerprint {
    release_py: FileStamp,
    config_py: FileStamp,
}

#[derive(Serialize, Deserialize)]
struct CacheFile {
    format: u32,
    fingerprint: Fingerprint,
    schema: OptionSchema,
}

/// Returns the schema for a Community source: from the cache when the tree is
/// unchanged, else by introspection with `python` (then cached), else bundled.
///
/// Introspection failures are not errors: they fall back to the bundled schema and
/// the reason is reported in [`SchemaOrigin::Bundled`].
///
/// # Errors
/// Only fails for a non-Community source.
pub async fn load_schema(
    source: &SourceEntry,
    python: Option<&Path>,
    cache_dir: &Path,
    timeout: Duration,
) -> Result<LoadedSchema, SchemaError> {
    if source.edition != Edition::Community {
        return Err(SchemaError::NotCommunity(source.id.clone()));
    }
    let expected = source.odoo_version.to_string();
    let cache_file = cache_path(cache_dir, &source.id);
    let fingerprint = fingerprint(&source.path);

    if let (Some(file), Some(fp)) = (&cache_file, &fingerprint)
        && let Some(schema) = read_cache(file, fp, &expected)
    {
        return Ok(LoadedSchema {
            schema,
            origin: SchemaOrigin::Cached,
        });
    }

    let bundled_because = |reason: String| LoadedSchema {
        schema: bundled(source.odoo_version),
        origin: SchemaOrigin::Bundled { reason },
    };
    let Some(python) = python else {
        return Ok(bundled_because("no Python environment yet".into()));
    };
    let schema = match introspect(python, &source.path, timeout).await {
        Ok(s) if s.series == expected => s,
        Ok(s) => {
            let e = SchemaError::VersionMismatch {
                expected,
                found: s.series,
            };
            return Ok(bundled_because(e.to_string()));
        }
        Err(e) => return Ok(bundled_because(e.to_string())),
    };
    if let (Some(file), Some(fp)) = (cache_file, fingerprint) {
        write_cache(&file, fp, &schema);
    }
    Ok(LoadedSchema {
        schema,
        origin: SchemaOrigin::Introspected,
    })
}

/// Cache file for a source id, or `None` if the id is not a safe file name.
fn cache_path(cache_dir: &Path, id: &str) -> Option<PathBuf> {
    let safe = !id.is_empty()
        && !id.starts_with('.')
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    safe.then(|| cache_dir.join(format!("{id}.json")))
}

fn stamp(path: &Path) -> Option<FileStamp> {
    let meta = fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    let modified_ns = modified
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some(FileStamp {
        len: meta.len(),
        modified_ns,
    })
}

fn fingerprint(source: &Path) -> Option<Fingerprint> {
    Some(Fingerprint {
        release_py: stamp(&source.join(RELEASE_PY))?,
        config_py: stamp(&source.join(CONFIG_PY))?,
    })
}

fn read_cache(file: &Path, fp: &Fingerprint, expected: &str) -> Option<OptionSchema> {
    let text = fsutil::read_private(file, MAX_OUTPUT_BYTES).ok()??;
    let cache: CacheFile = serde_json::from_str(&text).ok()?;
    (cache.format == CACHE_FORMAT && &cache.fingerprint == fp && cache.schema.series == expected)
        .then_some(cache.schema)
}

/// Best effort: a cache that cannot be written only costs a re-introspection.
fn write_cache(file: &Path, fingerprint: Fingerprint, schema: &OptionSchema) {
    let cache = CacheFile {
        format: CACHE_FORMAT,
        fingerprint,
        schema: schema.clone(),
    };
    if let Ok(text) = serde_json::to_string(&cache)
        && let Err(e) = fsutil::write_private_atomic(file, text.as_bytes())
    {
        tracing::warn!(path = %e.path.display(), error = %e.source, "option schema cache not written");
    }
}

#[cfg(test)]
mod tests;
