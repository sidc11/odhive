//! Watches custom addons folders and hands out debounced [`Batch`]es.
//!
//! - **Roots** are canonicalized when watching starts. Symlinks inside them
//!   are not followed (`notify` with `follow_symlinks = false`), and every
//!   changed path is checked with [`paths::ensure_within`] before it counts,
//!   so a link pointing out of the folder never makes OdHive look elsewhere.
//!   A module folder that is such a link is listed in
//!   [`Started::unwatched`]: add its real folder to `addons_paths` instead.
//! - **Debounce:** a batch is closed [`super::DEBOUNCE`] after its last
//!   change, or [`super::MAX_DELAY`] after its first one while changes keep
//!   coming (a `git checkout`, a formatter run).
//! - **Modules** are the folders directly inside a root that have a
//!   manifest. Each batch re-checks the modules it touched, so a manifest
//!   that appears (new or renamed module) or disappears is noticed; changes
//!   in folders without a manifest are dropped (Odoo does not load them).

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use notify::event::{AccessKind, AccessMode};
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::{Batch, DEBOUNCE, MANIFEST, MAX_DELAY, classify, is_module_name};
use crate::paths::{self, PathError};

/// Errors from starting to watch.
#[derive(Debug, Error)]
pub enum WatchError {
    /// Nothing to watch.
    #[error("no addons folders to watch")]
    NoRoots,
    /// An addons folder cannot be resolved.
    #[error("{path}: {source}")]
    Root {
        /// The folder.
        path: PathBuf,
        /// Why.
        #[source]
        source: io::Error,
    },
    /// The system refused to watch more folders.
    #[error(
        "{path}: too many folders to watch; raise the limit (Linux: sysctl fs.inotify.max_user_watches) or set reload_mode = \"off\""
    )]
    Limit {
        /// The folder.
        path: PathBuf,
    },
    /// The watcher failed.
    #[error("{path}: watching failed: {error}")]
    Notify {
        /// The folder.
        path: PathBuf,
        /// What `notify` reported.
        error: String,
    },
}

/// Debounce timing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchOptions {
    /// Quiet time that closes a batch.
    pub debounce: Duration,
    /// Longest a batch stays open.
    pub max_delay: Duration,
}

impl Default for WatchOptions {
    fn default() -> Self {
        Self {
            debounce: DEBOUNCE,
            max_delay: MAX_DELAY,
        }
    }
}

/// What watching started with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Started {
    /// Watched folders, canonical.
    pub roots: Vec<PathBuf>,
    /// Modules found in them.
    pub modules: usize,
    /// Module folders that are symlinks to outside their addons folder; their
    /// changes are not seen.
    pub unwatched: Vec<PathBuf>,
}

/// A raw event as the watcher thread hands it over.
type Raw = notify::Result<Event>;

/// Watches addons folders. Dropping it stops watching.
#[derive(Debug)]
pub struct Watch {
    rx: mpsc::UnboundedReceiver<Raw>,
    roots: Vec<PathBuf>,
    known: BTreeSet<String>,
    opts: WatchOptions,
    _watcher: Option<RecommendedWatcher>,
}

/// Changes of one batch before modules are re-checked.
#[derive(Debug, Default)]
struct Pending {
    /// Changed paths (absolute, unresolved) with their module.
    paths: BTreeMap<PathBuf, String>,
    /// Modules touched in any way (also by folder renames).
    touched: BTreeSet<String>,
    rescan: bool,
}

impl Watch {
    /// Starts watching `roots` recursively. Blocking: watching walks every
    /// folder below the roots.
    ///
    /// # Errors
    /// See [`WatchError`].
    pub fn start(roots: &[PathBuf], opts: WatchOptions) -> Result<(Self, Started), WatchError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut watch = Self::with_channel(roots, rx, opts)?;
        let mut watcher = RecommendedWatcher::new(
            move |e: Raw| {
                // The receiver is gone only while the watch is being dropped.
                let _ = tx.send(e);
            },
            Config::default().with_follow_symlinks(false),
        )
        .map_err(|e| watch_error(&watch.roots[0], e))?;
        for root in &watch.roots {
            watcher
                .watch(root, RecursiveMode::Recursive)
                .map_err(|e| watch_error(root, e))?;
        }
        watch._watcher = Some(watcher);
        let started = watch.started();
        Ok((watch, started))
    }

    /// A watch fed from `rx` instead of the file system (for tests).
    fn with_channel(
        roots: &[PathBuf],
        rx: mpsc::UnboundedReceiver<Raw>,
        opts: WatchOptions,
    ) -> Result<Self, WatchError> {
        if roots.is_empty() {
            return Err(WatchError::NoRoots);
        }
        let mut canonical = Vec::with_capacity(roots.len());
        for r in roots {
            let c = std::fs::canonicalize(r).map_err(|source| WatchError::Root {
                path: r.clone(),
                source,
            })?;
            if !canonical.contains(&c) {
                canonical.push(c);
            }
        }
        let mut watch = Self {
            rx,
            roots: canonical,
            known: BTreeSet::new(),
            opts,
            _watcher: None,
        };
        watch.known = watch.scan().0;
        Ok(watch)
    }

    fn started(&self) -> Started {
        Started {
            roots: self.roots.clone(),
            modules: self.known.len(),
            unwatched: self.scan().1,
        }
    }

    /// Modules in the roots, and module folders that link out of their root.
    fn scan(&self) -> (BTreeSet<String>, Vec<PathBuf>) {
        let mut modules = BTreeSet::new();
        let mut unwatched = Vec::new();
        for root in &self.roots {
            let Ok(entries) = std::fs::read_dir(root) else {
                continue;
            };
            for e in entries.flatten() {
                let Some(name) = e.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                if !is_module_name(&name) {
                    continue;
                }
                let dir = e.path();
                match paths::ensure_within(root, &dir) {
                    Ok(resolved) if resolved.join(MANIFEST).is_file() => {
                        modules.insert(name);
                    }
                    Ok(_) => {}
                    Err(PathError::Escapes { .. }) if dir.join(MANIFEST).is_file() => {
                        unwatched.push(dir);
                    }
                    Err(_) => {}
                }
            }
        }
        unwatched.sort();
        (modules, unwatched)
    }

    /// Whether `module` is a module now: its folder, inside its root, has a
    /// manifest (in any root).
    fn is_module(&self, module: &str) -> bool {
        self.roots.iter().any(|root| {
            paths::ensure_within(root, &root.join(module))
                .is_ok_and(|dir| dir.join(MANIFEST).is_file())
        })
    }

    /// Modules currently known.
    #[must_use]
    pub fn modules(&self) -> &BTreeSet<String> {
        &self.known
    }

    /// The next non-empty batch. `None` once the watcher has stopped.
    pub async fn next(&mut self) -> Option<Batch> {
        loop {
            let first = self.rx.recv().await?;
            let mut pending = Pending::default();
            self.take(&mut pending, first);
            let opened = Instant::now();
            let mut last = opened;
            loop {
                let close = (last + self.opts.debounce).min(opened + self.opts.max_delay);
                tokio::select! {
                    raw = self.rx.recv() => match raw {
                        Some(raw) => {
                            self.take(&mut pending, raw);
                            last = Instant::now();
                        }
                        None => break,
                    },
                    () = tokio::time::sleep_until(close) => break,
                }
            }
            let batch = self.finish(pending);
            if !batch.is_empty() {
                return Some(batch);
            }
        }
    }

    /// Notes the paths of one raw event.
    fn take(&self, pending: &mut Pending, raw: Raw) {
        let event = match raw {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "file watcher error");
                return;
            }
        };
        if event.need_rescan() {
            pending.rescan = true;
        }
        match event.kind {
            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) | EventKind::Any => {
            }
            EventKind::Access(AccessKind::Close(AccessMode::Write)) => {}
            EventKind::Access(_) | EventKind::Other => return,
        }
        for path in event.paths {
            if let Some(module) = self.module_of(&path) {
                pending.touched.insert(module.clone());
                pending.paths.insert(path, module);
            }
        }
    }

    /// The module `path` belongs to, if it is inside a root (checked on
    /// disk: no symlink out of the root) and below a module-like folder.
    fn module_of(&self, path: &Path) -> Option<String> {
        let root = self
            .roots
            .iter()
            .filter(|r| path.starts_with(r))
            .max_by_key(|r| r.as_os_str().len())?;
        let rel = path.strip_prefix(root).ok()?;
        let Some(Component::Normal(first)) = rel.components().next() else {
            return None;
        };
        let module = first.to_str()?;
        if !is_module_name(module) {
            return None;
        }
        match paths::ensure_within(root, path) {
            Ok(_) => Some(module.to_owned()),
            Err(e) => {
                tracing::debug!(path = %path.display(), error = %e, "change ignored");
                None
            }
        }
    }

    /// Re-checks the touched modules and builds the batch.
    fn finish(&mut self, pending: Pending) -> Batch {
        let mut batch = Batch {
            rescan: pending.rescan,
            ..Batch::default()
        };
        for module in &pending.touched {
            match (self.known.contains(module), self.is_module(module)) {
                (false, true) => {
                    self.known.insert(module.clone());
                    batch.new_modules.insert(module.clone());
                }
                (true, false) => {
                    self.known.remove(module);
                    batch.removed_modules.insert(module.clone());
                }
                _ => {}
            }
        }
        for (path, module) in &pending.paths {
            if !self.known.contains(module) && !batch.removed_modules.contains(module) {
                continue;
            }
            let Some(root) = self.roots.iter().find(|r| path.starts_with(r)) else {
                continue;
            };
            let rel = path.strip_prefix(root).unwrap_or(path);
            if let Some(change) = classify(rel) {
                batch.add(path, change);
            }
        }
        if pending.rescan {
            // Lost events: start over from what is on disk.
            self.known = self.scan().0;
        }
        batch
    }
}

fn watch_error(path: &Path, e: notify::Error) -> WatchError {
    let path = path.to_path_buf();
    match e.kind {
        notify::ErrorKind::MaxFilesWatch => WatchError::Limit { path },
        // ENOSPC from inotify_add_watch: the watch limit.
        notify::ErrorKind::Io(ref io)
            if io.raw_os_error() == Some(rustix::io::Errno::NOSPC.raw_os_error()) =>
        {
            WatchError::Limit { path }
        }
        _ => WatchError::Notify {
            path,
            error: e.to_string(),
        },
    }
}

#[cfg(test)]
pub(super) mod testing {
    use super::{Raw, Watch, WatchError, WatchOptions, mpsc};
    use std::path::PathBuf;

    /// A watch fed by hand.
    pub fn channel(
        roots: &[PathBuf],
        opts: WatchOptions,
    ) -> Result<(Watch, mpsc::UnboundedSender<Raw>), WatchError> {
        let (tx, rx) = mpsc::unbounded_channel();
        Ok((Watch::with_channel(roots, rx, opts)?, tx))
    }
}
