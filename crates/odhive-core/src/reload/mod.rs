//! Smart Reload: what a change in a custom addons folder means for a running
//! instance.
//!
//! Paths are classified relative to the addons folder they are in
//! ([`classify`]): the first component is the module, the rest says what
//! kind of file changed.
//!
//! | Path (inside `<module>/`)          | Kind                  | Auto mode             |
//! |------------------------------------|-----------------------|-----------------------|
//! | `static/**`                        | [`FileKind::Static`]  | refresh the browser   |
//! | `__manifest__.py`                  | [`FileKind::Manifest`]| restart `-u <module>` |
//! | `security/**`, `*.xml`, `*.csv`, `*.po` | [`FileKind::Data`] | restart `-u <module>` |
//! | `*.py`                             | [`FileKind::Python`]  | restart               |
//! | anything else                      | ignored               |                       |
//!
//! Ignored everywhere: hidden files and folders (`.git`, `.idea`, `.#x`),
//! `__pycache__`, `node_modules`, compiled Python and editor temporary or
//! backup files (`x~`, `.x.swp`, `#x#`, `4913`, `x___jb_tmp___`, …).
//!
//! Changes within the debounce window are collected into a [`Batch`] and
//! turned into one [`Action`] by [`decide`]. A manifest appearing for a
//! module that was not there before is reported separately (it is not
//! installed; installing is the user's call).
//!
//! The file watching itself lives in [`watch`].

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde::Serialize;

pub use crate::project::ReloadMode;

pub mod watch;

#[cfg(test)]
mod tests;

/// Quiet time after the last change before a batch is acted on.
pub const DEBOUNCE: Duration = Duration::from_millis(500);
/// A batch is acted on at the latest this long after its first change, even
/// while changes keep coming.
pub const MAX_DELAY: Duration = Duration::from_secs(5);
/// Changed files listed in a [`Batch`] (the count is always complete).
pub const MAX_LISTED_FILES: usize = 20;
/// Longest accepted module (folder) name.
pub const MAX_MODULE_NAME: usize = 128;

/// The module manifest file name.
pub const MANIFEST: &str = "__manifest__.py";

/// What kind of module file changed.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    /// Python code: the process must be restarted.
    Python,
    /// Data loaded on module update: XML, CSV, translations, `security/`.
    Data,
    /// The module manifest.
    Manifest,
    /// Web assets under `static/`: served from disk, no restart needed.
    Static,
}

/// One classified change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// Module (folder name directly inside the addons folder).
    pub module: String,
    /// What changed.
    pub kind: FileKind,
}

/// What to do about a batch of changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize, utoipa::ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    /// Restart the process.
    Restart,
    /// Restart the process with `-u <modules>`, so their data is reloaded.
    Update {
        /// Modules to update, sorted.
        modules: Vec<String>,
    },
    /// Only static files changed: reload the page in the browser.
    RefreshBrowser {
        /// Modules whose static files changed, sorted.
        modules: Vec<String>,
    },
}

impl Action {
    /// Arguments for the restarted process (`-u m1,m2` for an update).
    #[must_use]
    pub fn args(&self) -> Vec<OsString> {
        match self {
            Self::Update { modules } => vec!["-u".into(), modules.join(",").into()],
            Self::Restart | Self::RefreshBrowser { .. } => Vec::new(),
        }
    }

    /// The process is restarted.
    #[must_use]
    pub fn restarts(&self) -> bool {
        !matches!(self, Self::RefreshBrowser { .. })
    }
}

/// Whether `name` can be a module: a non-empty ASCII identifier
/// (`[A-Za-z0-9_]`) of at most [`MAX_MODULE_NAME`] bytes. Keeps `-u` values
/// to plain names (no `,`, no option-like text).
#[must_use]
pub fn is_module_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_MODULE_NAME
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// A folder or file name that is never looked at, wherever it is.
fn ignored_component(name: &str) -> bool {
    name.starts_with('.') || matches!(name, "__pycache__" | "node_modules")
}

/// Editor temporary, swap, backup and compiled files.
fn ignored_file(name: &str) -> bool {
    const SUFFIXES: &[&str] = &[
        "~",
        ".swp",
        ".swo",
        ".swx",
        ".swpx",
        ".kate-swp",
        ".tmp",
        ".bak",
        ".orig",
        ".rej",
        ".pyc",
        ".pyo",
        "___jb_tmp___",
        "___jb_old___",
    ];
    // `4913` is the file vim creates to test whether a folder is writable.
    name == "4913"
        || (name.starts_with('#') && name.ends_with('#'))
        || SUFFIXES.iter().any(|s| name.ends_with(s))
}

/// Classifies a path relative to an addons folder. `None` = not a change
/// that matters (outside a module, ignored, or a file kind Odoo does not
/// load).
#[must_use]
pub fn classify(rel: &Path) -> Option<Change> {
    let mut parts = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(p) => parts.push(p.to_str()?),
            _ => return None,
        }
    }
    let (&module, rest) = parts.split_first()?;
    let (&file, dirs) = rest.split_last()?;
    if !is_module_name(module) || parts.iter().any(|p| ignored_component(p)) || ignored_file(file) {
        return None;
    }
    let kind = if dirs.first() == Some(&"static") {
        FileKind::Static
    } else if dirs.is_empty() && file == MANIFEST {
        FileKind::Manifest
    } else if dirs.first() == Some(&"security") {
        FileKind::Data
    } else {
        match Path::new(file).extension()?.to_str()? {
            "py" => FileKind::Python,
            "xml" | "csv" | "po" => FileKind::Data,
            _ => return None,
        }
    };
    Some(Change {
        module: module.to_owned(),
        kind,
    })
}

/// Changes collected during one debounce window.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Batch {
    /// Kinds of change per module.
    pub modules: BTreeMap<String, BTreeSet<FileKind>>,
    /// Modules whose manifest appeared (not there when watching began).
    pub new_modules: BTreeSet<String>,
    /// Modules whose manifest is gone.
    pub removed_modules: BTreeSet<String>,
    /// The watcher lost events (queue overflow): anything may have changed.
    pub rescan: bool,
    /// Changed files that matter, up to [`MAX_LISTED_FILES`].
    pub files: Vec<PathBuf>,
    /// Number of changed files that matter.
    pub file_count: usize,
}

impl Batch {
    /// Adds a classified change of `path`.
    pub fn add(&mut self, path: &Path, change: Change) {
        self.modules
            .entry(change.module)
            .or_default()
            .insert(change.kind);
        if !self.files.iter().any(|f| f == path) {
            self.file_count += 1;
            if self.files.len() < MAX_LISTED_FILES {
                self.files.push(path.to_path_buf());
            }
        }
    }

    /// Nothing that matters changed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.modules.is_empty()
            && self.new_modules.is_empty()
            && self.removed_modules.is_empty()
            && !self.rescan
    }
}

/// The action for `batch` in `mode`. `None` = nothing to do (mode off, or
/// only a new module appeared).
///
/// - Auto: data or manifest changes → [`Action::Update`] of those modules
///   (which restarts, so Python changes are covered); else Python changes →
///   [`Action::Restart`]; else static changes → [`Action::RefreshBrowser`].
/// - Restart: like auto, but an update is a plain restart.
/// - A removed module or lost events → [`Action::Restart`] (no module can be
///   named for `-u`).
#[must_use]
pub fn decide(mode: ReloadMode, batch: &Batch) -> Option<Action> {
    if mode == ReloadMode::Off {
        return None;
    }
    let with = |k: &[FileKind]| -> Vec<String> {
        batch
            .modules
            .iter()
            .filter(|(m, kinds)| {
                !batch.new_modules.contains(*m)
                    && !batch.removed_modules.contains(*m)
                    && kinds.iter().any(|x| k.contains(x))
            })
            .map(|(m, _)| m.clone())
            .collect()
    };
    let update = with(&[FileKind::Data, FileKind::Manifest]);
    let python = with(&[FileKind::Python]);
    let fixed = batch.rescan || !batch.removed_modules.is_empty();
    if !update.is_empty() && mode == ReloadMode::Auto && !fixed {
        return Some(Action::Update { modules: update });
    }
    if fixed || !update.is_empty() || !python.is_empty() {
        return Some(Action::Restart);
    }
    let refresh = with(&[FileKind::Static]);
    (!refresh.is_empty()).then_some(Action::RefreshBrowser { modules: refresh })
}
