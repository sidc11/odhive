//! The state behind the API: OdHive's paths, the workspace and the instance
//! manager. The global config is read from disk on every request, so changes
//! made by the CLI are seen; changes made through the API are serialized by
//! a lock and written back at once.

use std::sync::Arc;

use odhive_core::config::{AppPaths, GlobalConfig};
use odhive_core::launch::{self, LoadedProject, Target};
use odhive_core::manager::{InstanceInfo, Manager, StopOutcome};
use odhive_core::pidfile::{self, PidRecord, RecordState, RunDir};
use odhive_core::ports::PortOwner;
use odhive_core::supervisor::DEFAULT_STOP_TIMEOUT;
use odhive_core::workspace::Workspace;
use tokio::sync::{Mutex, watch};

use crate::api::error::{ApiError, ApiResult};
use crate::api::ws::WsLimits;

/// Shared state of the API. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Service {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    paths: AppPaths,
    workspace: Workspace,
    manager: Manager,
    /// Serializes read-modify-write cycles of the global config.
    config_lock: Mutex<()>,
    ws_limits: WsLimits,
    /// Turns `true` when the server begins to shut down.
    shutdown: watch::Sender<bool>,
}

/// Tunables of a [`Service`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ServiceOptions {
    /// Limits of each WebSocket connection.
    pub ws: WsLimits,
}

/// An instance this OdHive does not manage that runs according to its PID
/// file, e.g. one started by `odhive start` in a terminal.
#[derive(Debug, Clone)]
pub(crate) struct External {
    pub record: PidRecord,
    pub pid: u32,
    pub supervisor: Option<u32>,
}

/// How a stop request ended.
#[derive(Debug, Clone)]
pub(crate) enum Stopped {
    /// Stopped by the manager; `was_running` = it had not ended before.
    Managed {
        was_running: bool,
        outcome: StopOutcome,
    },
    /// Stopped through its PID file.
    External(pidfile::AttachedStop),
    /// Nothing was running.
    NotRunning,
}

impl Service {
    /// State for `paths` and `workspace` with `manager` running the instances.
    /// The manager should use `paths.run_dir()` for its PID files.
    #[must_use]
    pub fn new(paths: AppPaths, workspace: Workspace, manager: Manager) -> Self {
        Self::with_options(paths, workspace, manager, ServiceOptions::default())
    }

    /// Like [`Service::new`] with non-default `opts`.
    #[must_use]
    pub fn with_options(
        paths: AppPaths,
        workspace: Workspace,
        manager: Manager,
        opts: ServiceOptions,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                paths,
                workspace,
                manager,
                config_lock: Mutex::new(()),
                ws_limits: opts.ws,
                shutdown: watch::Sender::new(false),
            }),
        }
    }

    /// Tells long-lived connections (WebSockets) to close: the server is
    /// shutting down. [`crate::serve`] calls it when its shutdown future
    /// resolves.
    pub fn begin_shutdown(&self) {
        self.inner.shutdown.send_replace(true);
    }

    /// Turns `true` once [`Service::begin_shutdown`] was called.
    pub(crate) fn shutdown_signal(&self) -> watch::Receiver<bool> {
        self.inner.shutdown.subscribe()
    }

    pub(crate) fn ws_limits(&self) -> WsLimits {
        self.inner.ws_limits
    }

    /// The instance manager.
    #[must_use]
    pub fn manager(&self) -> &Manager {
        &self.inner.manager
    }

    pub(crate) fn paths(&self) -> &AppPaths {
        &self.inner.paths
    }

    pub(crate) fn workspace(&self) -> &Workspace {
        &self.inner.workspace
    }

    pub(crate) fn run_dir(&self) -> RunDir {
        RunDir::new(self.inner.paths.run_dir())
    }

    /// The global config as it is on disk now.
    pub(crate) fn config(&self) -> ApiResult<GlobalConfig> {
        Ok(GlobalConfig::load_or_default(&self.inner.paths)?)
    }

    /// Loads the config, lets `edit` change it and saves it when `edit`
    /// succeeds. Concurrent edits through the API are serialized.
    pub(crate) async fn edit_config<T>(
        &self,
        edit: impl FnOnce(&mut GlobalConfig) -> ApiResult<T>,
    ) -> ApiResult<T> {
        let _guard = self.inner.config_lock.lock().await;
        let mut cfg = self.config()?;
        let out = edit(&mut cfg)?;
        cfg.save(&self.inner.paths.config_file())?;
        Ok(out)
    }

    /// The project `name`.
    pub(crate) fn project(&self, name: &str) -> ApiResult<(GlobalConfig, LoadedProject)> {
        let cfg = self.config()?;
        let project = launch::load_project(&cfg, name)?;
        Ok((cfg, project))
    }

    /// The running process of `owner` if another OdHive (or none) runs it.
    pub(crate) fn external(&self, owner: &PortOwner) -> ApiResult<Option<External>> {
        if self.inner.manager.get(owner).is_some() {
            return Ok(None);
        }
        let record = match self.run_dir().read(owner) {
            Ok(r) => r,
            Err(pidfile::PidFileError::Invalid(_)) => None,
            Err(e) => return Err(e.into()),
        };
        Ok(record.and_then(|record| match record.state() {
            RecordState::Running { pid, supervisor } => Some(External {
                record,
                pid,
                supervisor,
            }),
            RecordState::Stale { .. } => None,
        }))
    }

    /// Starts instance `id` of project `name`, or, with `replace`, restarts
    /// it with the current project settings (starting it if it is stopped).
    pub(crate) async fn start(
        &self,
        name: &str,
        id: &str,
        replace: bool,
    ) -> ApiResult<InstanceInfo> {
        let (cfg, project) = self.project(name)?;
        let target = Target {
            project: name.to_owned(),
            instance: Some(id.to_owned()),
        };
        let selected = project.select(&target)?;
        let owner = PortOwner::new(name, id);
        if let Some(ext) = self.external(&owner)? {
            return Err(ApiError::conflict(
                "running_elsewhere",
                format!(
                    "{owner} runs outside this OdHive (pid {}); stop it first",
                    ext.pid
                ),
            ));
        }
        let manager = &self.inner.manager;
        // Instances run elsewhere keep their database and their ports.
        let mut reservations = Vec::new();
        let others = launch::running_records(&self.run_dir())?;
        let prepared = launch::prepare(&cfg, &self.inner.paths, &project, &selected).await?;
        let Some(plan) = prepared.plans.into_iter().next() else {
            return Err(ApiError::internal(format!("no plan for {owner}")));
        };
        for rec in others {
            let other = rec.owner();
            if other == owner || manager.get(&other).is_some() {
                continue;
            }
            if rec.db.is_some() && rec.db == plan.spec.db {
                return Err(ApiError::conflict(
                    "database_in_use",
                    format!(
                        "database {:?} is already used by {other}",
                        rec.db.unwrap_or_default()
                    ),
                ));
            }
            if let Ok(r) = manager
                .registry()
                .reserve(other, rec.http_port, rec.gevent_port)
            {
                reservations.push(r);
            }
        }
        let info = if replace {
            manager.replace(plan).await?
        } else {
            manager.start(plan).await?
        };
        drop(reservations);
        Ok(info)
    }

    /// Stops instance `owner`: through the manager, or through its PID file
    /// when another OdHive runs it.
    pub(crate) async fn stop(&self, owner: &PortOwner) -> ApiResult<Stopped> {
        if let Some(info) = self.inner.manager.get(owner) {
            let outcome = self.inner.manager.stop(owner).await?;
            return Ok(Stopped::Managed {
                was_running: !info.status.is_terminal(),
                outcome,
            });
        }
        match self.external(owner)? {
            Some(ext) => {
                let stop =
                    pidfile::stop_recorded(&self.run_dir(), &ext.record, DEFAULT_STOP_TIMEOUT)
                        .await?;
                Ok(Stopped::External(stop))
            }
            None => Ok(Stopped::NotRunning),
        }
    }
}
