//! Smart Reload for one managed instance (see [`crate::reload`]).
//!
//! A task per instance watches its custom addons folders and acts on each
//! debounced batch: restart, restart with `-u <modules>`, or only tell the
//! browser to refresh. The task lives as long as the instance's entry (it is
//! kept across restarts and the port retry) and is aborted with it.
//!
//! Modules to update stay pending until a process started with them reaches
//! `running`: a change that arrives while that process is still loading (or
//! after it crashed) restarts with them again, so an update is not lost.
//! While a missing dependency is being installed, acting waits.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use serde::Serialize;
use tokio::task::JoinHandle;

use super::{Inner, Notice, Slot};
use crate::logs::store::Mark;
use crate::ports::PortOwner;
use crate::project::ReloadMode;
use crate::reload::watch::{Watch, WatchOptions};
use crate::reload::{self, Action, Batch, DEBOUNCE};
use crate::supervisor::Status;

/// Smart Reload settings of one instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reload {
    /// What changes lead to. [`ReloadMode::Off`] watches nothing.
    pub mode: ReloadMode,
    /// Custom addons folders to watch.
    pub roots: Vec<PathBuf>,
    /// Debounce timing.
    pub watch: WatchOptions,
}

/// Smart Reload state shown with an instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct ReloadInfo {
    /// Mode.
    pub mode: ReloadMode,
    /// The addons folders are being watched.
    pub watching: bool,
    /// Modules to update that no running process has loaded yet.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_update: Vec<String>,
}

#[derive(Debug, Default)]
struct State {
    watching: bool,
    pending: BTreeSet<String>,
}

/// The Smart Reload of one instance entry.
#[derive(Debug)]
pub(super) struct ReloadState {
    mode: ReloadMode,
    state: Mutex<State>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for ReloadState {
    fn drop(&mut self) {
        if let Some(task) = self
            .task
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            // Drops the watch, which stops watching.
            task.abort();
        }
    }
}

impl ReloadState {
    /// Starts watching for `owner`. `None` when the mode is off or there is
    /// nothing to watch.
    pub fn start(config: Reload, inner: Weak<Inner>, owner: PortOwner) -> Option<Arc<Self>> {
        if config.mode == ReloadMode::Off || config.roots.is_empty() {
            return None;
        }
        let me = Arc::new(Self {
            mode: config.mode,
            state: Mutex::new(State::default()),
            task: Mutex::new(None),
        });
        let task = tokio::spawn(run(inner, owner, Arc::downgrade(&me), config));
        *me.task.lock().unwrap_or_else(PoisonError::into_inner) = Some(task);
        Some(me)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // Plain values only, so valid after a panic.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn info(&self) -> ReloadInfo {
        let s = self.lock();
        ReloadInfo {
            mode: self.mode,
            watching: s.watching,
            pending_update: s.pending.iter().cloned().collect(),
        }
    }
}

fn notify(inner: &Weak<Inner>, owner: &PortOwner, notice: Notice) {
    if let Some(me) = inner.upgrade() {
        me.notify(owner, notice);
    }
}

async fn run(inner: Weak<Inner>, owner: PortOwner, me: Weak<ReloadState>, config: Reload) {
    let (watched, opts) = (config.roots.clone(), config.watch);
    let started = tokio::task::spawn_blocking(move || Watch::start(&watched, opts)).await;
    let (mut watch, started) = match started {
        Ok(Ok(w)) => w,
        Ok(Err(e)) => {
            notify(
                &inner,
                &owner,
                Notice::ReloadFailed {
                    error: e.to_string(),
                },
            );
            return;
        }
        // Cancelled with the runtime.
        Err(_) => return,
    };
    match me.upgrade() {
        Some(state) => state.lock().watching = true,
        None => return,
    }
    // Canonical, like the paths of changed files.
    let roots = started.roots.clone();
    notify(
        &inner,
        &owner,
        Notice::ReloadWatching {
            roots: started.roots,
            modules: started.modules,
            unwatched: started.unwatched,
        },
    );
    while let Some(batch) = watch.next().await {
        let Some(state) = me.upgrade() else {
            return;
        };
        for module in &batch.new_modules {
            notify(
                &inner,
                &owner,
                Notice::ModuleAdded {
                    module: module.clone(),
                },
            );
        }
        if let Some(action) = reload::decide(config.mode, &batch) {
            apply(&inner, &owner, &state, action, &batch, &roots).await;
        }
    }
    if let Some(state) = me.upgrade() {
        state.lock().watching = false;
    }
}

/// Carries out `action` on the instance, if it is still the one `state`
/// belongs to and runs under supervision.
async fn apply(
    inner: &Weak<Inner>,
    owner: &PortOwner,
    state: &Arc<ReloadState>,
    action: Action,
    batch: &Batch,
    roots: &[PathBuf],
) {
    let (handle, log, status) = loop {
        let Some(me) = inner.upgrade() else {
            return;
        };
        let found = {
            let entries = me.lock();
            let Some(entry) = entries
                .get(owner)
                .filter(|e| e.reload.as_ref().is_some_and(|r| Arc::ptr_eq(r, state)))
            else {
                return;
            };
            let Slot::Supervised(handle) = &entry.slot else {
                return;
            };
            (!entry.is_fixing()).then(|| {
                (
                    handle.clone(),
                    entry.log.clone(),
                    entry.status.borrow().clone(),
                )
            })
        };
        drop(me);
        match found {
            Some(f) => break f,
            // The install restarts the instance itself; act after it.
            None => tokio::time::sleep(DEBOUNCE).await,
        }
    };
    let action = {
        let mut s = state.lock();
        if matches!(status, Status::Running { .. }) {
            // The process that had them loaded them.
            s.pending.clear();
        }
        match action {
            Action::Update { modules } => {
                s.pending.extend(modules);
                Action::Update {
                    modules: s.pending.iter().cloned().collect(),
                }
            }
            Action::Restart if !s.pending.is_empty() && state.mode == ReloadMode::Auto => {
                Action::Update {
                    modules: s.pending.iter().cloned().collect(),
                }
            }
            other => other,
        }
    };
    if action.restarts()
        && let Some(log) = &log
    {
        let update = match &action {
            Action::Update { modules } => modules.clone(),
            _ => Vec::new(),
        };
        let what = if update.is_empty() {
            "restarting".to_owned()
        } else {
            format!("restarting with -u {}", update.join(","))
        };
        let files: Vec<String> = batch.files.iter().map(|f| shown_path(roots, f)).collect();
        let changed = match (batch.file_count, files.first()) {
            (1, Some(file)) => file.clone(),
            (0, _) => "files".to_owned(),
            (n, _) => format!("{n} files"),
        };
        // A file error was reported when the log was opened or first written.
        let _ = log.marker(
            &format!("odhive: {changed} changed; {what}"),
            Mark::Reload {
                files,
                file_count: batch.file_count,
                update,
            },
        );
    }
    notify(
        inner,
        owner,
        Notice::Reload {
            action: action.clone(),
            files: batch.files.clone(),
            file_count: batch.file_count,
        },
    );
    if action.restarts() {
        // A failed spawn is reported by the supervisor as `failed_to_start`.
        if let Err(e) = handle.restart_with(action.args()).await {
            tracing::warn!(instance = %owner, error = %e, "reload restart failed");
        }
    }
}

/// `path` relative to the addons folder it is in (`sale_x/models/order.py`),
/// or as it is when no root contains it.
fn shown_path(roots: &[PathBuf], path: &Path) -> String {
    roots
        .iter()
        .filter_map(|root| path.strip_prefix(root).ok())
        .min_by_key(|rel| rel.components().count())
        .unwrap_or(path)
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shown_paths_are_relative_to_the_innermost_root() {
        let roots = [PathBuf::from("/p/addons"), PathBuf::from("/p/addons/extra")];
        let shown = |p: &str| shown_path(&roots, Path::new(p));
        assert_eq!(
            shown("/p/addons/sale_x/models/order.py"),
            "sale_x/models/order.py"
        );
        assert_eq!(shown("/p/addons/extra/m/__init__.py"), "m/__init__.py");
        assert_eq!(shown("/elsewhere/m/a.py"), "/elsewhere/m/a.py");
        // Component-wise: `/p/addons2` is not inside `/p/addons`.
        assert_eq!(shown("/p/addons2/m/a.py"), "/p/addons2/m/a.py");
    }
}
