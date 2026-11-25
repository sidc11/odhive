//! The state behind the API: OdHive's paths, the workspace and the instance
//! manager. The global config is read from disk on every request, so changes
//! made by the CLI are seen; changes made through the API are serialized by
//! a lock and written back at once.

use std::collections::HashMap;
use std::sync::Arc;

use odhive_core::config::{AppPaths, GlobalConfig};
use odhive_core::envcheck::CheckOptions;
use odhive_core::launch::{self, LoadedProject, Target};
use odhive_core::manager::{InstanceInfo, Manager, StopOutcome};
use odhive_core::pidfile::{self, PidRecord, RecordState, RunDir};
use odhive_core::ports::PortOwner;
use odhive_core::project::ProjectConfig;
use odhive_core::python::DiscoverOptions;
use odhive_core::sources::CloneOptions;
use odhive_core::supervisor::DEFAULT_STOP_TIMEOUT;
use odhive_core::usage::{Usage, UsageMonitor};
use odhive_core::workspace::Workspace;
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore, watch};

use crate::api::error::{ApiError, ApiResult};
use crate::api::ws::WsLimits;
use crate::auth::Auth;
use crate::jobs::{JobKind, Jobs};

/// Shared state of the API. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Service {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    paths: AppPaths,
    auth: Auth,
    workspace: Workspace,
    manager: Manager,
    jobs: Jobs,
    clone: CloneOptions,
    discover: DiscoverOptions,
    env: CheckOptions,
    /// Serializes read-modify-write cycles of the global config.
    config_lock: Mutex<()>,
    ws_limits: WsLimits,
    /// One permit per open WebSocket ([`WsLimits::max_connections`]).
    ws_slots: Arc<Semaphore>,
    /// Turns `true` when the server begins to shut down.
    shutdown: watch::Sender<bool>,
    /// Turns `true` when a client asked the service to quit.
    quit: watch::Sender<bool>,
    /// CPU and memory samples of running instances.
    usage: Arc<std::sync::Mutex<UsageMonitor>>,
}

/// Tunables of a [`Service`].
#[derive(Debug, Clone)]
pub struct ServiceOptions {
    /// Limits of each WebSocket connection.
    pub ws: WsLimits,
    /// git and the repository used to clone Odoo Community.
    pub clone: CloneOptions,
    /// Where project setups look for Python interpreters and uv.
    pub discover: DiscoverOptions,
    /// What the environment check looks at. Its `discover` and
    /// `requirements` are replaced per check (by [`Self::discover`] and the
    /// registered sources).
    pub env: CheckOptions,
}

impl Default for ServiceOptions {
    /// Default limits, the official Odoo repository, and Python and uv from
    /// the service's `PATH`.
    fn default() -> Self {
        Self {
            ws: WsLimits::default(),
            clone: CloneOptions::default(),
            discover: DiscoverOptions::from_env(),
            env: CheckOptions::from_env(DiscoverOptions::from_env()),
        }
    }
}

/// An instance this OdHive does not manage that runs according to its PID
/// file, e.g. one started by `odhive start` in a terminal.
#[derive(Debug, Clone)]
pub(crate) struct External {
    pub record: PidRecord,
    pub pid: u32,
    pub supervisor: Option<u32>,
}

/// An instance running on a database ([`Service::instances_on`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OnDatabase {
    /// Instance id.
    pub instance: String,
    /// Run by this service (else from a terminal).
    pub managed: bool,
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
    /// State for `paths` and `workspace` with `manager` running the instances
    /// and `auth` guarding the API. The manager should use `paths.run_dir()`
    /// for its PID files.
    #[must_use]
    pub fn new(paths: AppPaths, auth: Auth, workspace: Workspace, manager: Manager) -> Self {
        Self::with_options(paths, auth, workspace, manager, ServiceOptions::default())
    }

    /// Like [`Service::new`] with non-default `opts`.
    #[must_use]
    pub fn with_options(
        paths: AppPaths,
        auth: Auth,
        workspace: Workspace,
        manager: Manager,
        opts: ServiceOptions,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                paths,
                auth,
                workspace,
                manager,
                jobs: Jobs::new(),
                clone: opts.clone,
                discover: opts.discover,
                env: opts.env,
                config_lock: Mutex::new(()),
                ws_slots: Arc::new(Semaphore::new(opts.ws.max_connections)),
                ws_limits: opts.ws,
                shutdown: watch::Sender::new(false),
                quit: watch::Sender::new(false),
                usage: Arc::default(),
            }),
        }
    }

    /// Tells long-lived connections (WebSockets) to close: the server is
    /// shutting down. [`crate::serve`] calls it when its shutdown future
    /// resolves.
    pub fn begin_shutdown(&self) {
        self.inner.shutdown.send_replace(true);
    }

    /// Asks whoever awaits [`Service::quit_requested`] to end the service.
    pub fn request_quit(&self) {
        self.inner.quit.send_replace(true);
    }

    /// Resolves once [`Service::request_quit`] was called (also when it was
    /// called before).
    pub async fn quit_requested(&self) {
        let mut rx = self.inner.quit.subscribe();
        // The sender lives in `self`, so `wait_for` only ends on `true`.
        let _ = rx.wait_for(|quit| *quit).await;
    }

    /// Cancels the running jobs and waits up to `wait` for them to clean up.
    pub async fn cancel_jobs(&self, wait: std::time::Duration) {
        self.inner.jobs.cancel_all(wait).await;
    }

    /// Background jobs.
    #[must_use]
    pub fn jobs(&self) -> &Jobs {
        &self.inner.jobs
    }

    pub(crate) fn clone_options(&self) -> &CloneOptions {
        &self.inner.clone
    }

    pub(crate) fn discover_options(&self) -> &DiscoverOptions {
        &self.inner.discover
    }

    pub(crate) fn env_options(&self) -> &CheckOptions {
        &self.inner.env
    }

    /// Instances of project `name` that run here or elsewhere.
    pub(crate) fn running_instances(&self, name: &str) -> ApiResult<Vec<String>> {
        let mut running: Vec<String> = self
            .inner
            .manager
            .list_project(name)
            .into_iter()
            .filter(|i| !i.is_done())
            .map(|i| i.owner.to_string())
            .collect();
        running.extend(
            launch::running_records(&self.run_dir())?
                .iter()
                .filter(|r| r.project == name)
                .map(|r| r.owner().to_string()),
        );
        running.sort();
        running.dedup();
        Ok(running)
    }

    /// Instances of project `config` running on database `db`, here
    /// (`managed`) or elsewhere, sorted by id. An instance without a known
    /// database uses the one its config names.
    pub(crate) fn instances_on(
        &self,
        config: &ProjectConfig,
        db: &str,
    ) -> ApiResult<Vec<OnDatabase>> {
        let configured = |id: &str| {
            config
                .instances
                .iter()
                .find(|i| i.id == id)
                .map(|i| i.db_name(&config.name))
        };
        let mut found: Vec<OnDatabase> = self
            .inner
            .manager
            .list_project(&config.name)
            .into_iter()
            .filter(|i| {
                let on = i.db.clone().or_else(|| configured(&i.owner.instance));
                !i.is_done() && on.as_deref() == Some(db)
            })
            .map(|i| OnDatabase {
                instance: i.owner.instance,
                managed: true,
            })
            .collect();
        for rec in launch::running_records(&self.run_dir())? {
            let owner = rec.owner();
            if rec.project != config.name || self.inner.manager.get(&owner).is_some() {
                continue;
            }
            let on = rec.db.clone().or_else(|| configured(&owner.instance));
            if on.as_deref() == Some(db) {
                found.push(OnDatabase {
                    instance: owner.instance,
                    managed: false,
                });
            }
        }
        found.sort_by(|a, b| a.instance.cmp(&b.instance));
        found.dedup_by(|a, b| a.instance == b.instance);
        Ok(found)
    }

    /// `409 setup_running` while project `name` is being set up.
    pub(crate) fn ensure_not_setting_up(&self, name: &str) -> ApiResult<()> {
        let kind = JobKind::Setup {
            project: name.to_owned(),
        };
        match self.inner.jobs.running(&kind) {
            Some(job) => Err(ApiError::conflict(
                "setup_running",
                format!("project {name} is being set up (job {})", job.id),
            )),
            None => Ok(()),
        }
    }

    /// `409 database_job_running` while a database of project `name` is
    /// being created, restored or deleted.
    pub(crate) fn ensure_no_database_job(&self, name: &str) -> ApiResult<()> {
        let busy = self.inner.jobs.list().into_iter().find(|j| {
            j.state == crate::jobs::JobState::Running && j.kind.database_project() == Some(name)
        });
        match busy {
            Some(job) => Err(ApiError::conflict(
                "database_job_running",
                format!(
                    "a database of project {name} is being created, restored or deleted (job {})",
                    job.id
                ),
            )),
            None => Ok(()),
        }
    }

    /// `409 database_job_running` while database `database` of project
    /// `name` is being created, restored or deleted.
    pub(crate) fn ensure_database_idle(&self, name: &str, database: &str) -> ApiResult<()> {
        let busy =
            self.inner.jobs.list().into_iter().find(|j| {
                j.state == crate::jobs::JobState::Running && j.kind.touches(name, database)
            });
        match busy {
            Some(job) => Err(ApiError::conflict(
                "database_job_running",
                format!(
                    "database {database} of project {name} is being created, restored, deleted or copied (job {})",
                    job.id
                ),
            )),
            None => Ok(()),
        }
    }

    /// Turns `true` once [`Service::begin_shutdown`] was called.
    pub(crate) fn shutdown_signal(&self) -> watch::Receiver<bool> {
        self.inner.shutdown.subscribe()
    }

    pub(crate) fn ws_limits(&self) -> WsLimits {
        self.inner.ws_limits
    }

    /// A slot for one more WebSocket, held while it is open; `None` when
    /// [`WsLimits::max_connections`] are open.
    pub(crate) fn ws_slot(&self) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.inner.ws_slots).try_acquire_owned().ok()
    }

    /// The instance manager.
    #[must_use]
    pub fn manager(&self) -> &Manager {
        &self.inner.manager
    }

    /// Who may use the API.
    #[must_use]
    pub fn auth(&self) -> &Auth {
        &self.inner.auth
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

    /// Loads project `name`, lets `edit` change its settings and saves them
    /// when `edit` succeeds (the save validates them first). Serialized with
    /// config edits; refused while the project is being set up, since setup
    /// writes the same file.
    pub(crate) async fn edit_project<T>(
        &self,
        name: &str,
        edit: impl FnOnce(&mut ProjectConfig) -> ApiResult<T>,
    ) -> ApiResult<T> {
        let _guard = self.inner.config_lock.lock().await;
        self.ensure_not_setting_up(name)?;
        let (_, mut project) = self.project(name)?;
        let out = edit(&mut project.config)?;
        project.config.save(project.dir())?;
        Ok(out)
    }

    /// The project `name`.
    pub(crate) fn project(&self, name: &str) -> ApiResult<(GlobalConfig, LoadedProject)> {
        let cfg = self.config()?;
        let project = launch::load_project(&cfg, name)?;
        Ok((cfg, project))
    }

    /// The root process of every running instance this service knows of:
    /// its own and, through their PID files, those run elsewhere.
    pub(crate) fn running_pids(&self) -> ApiResult<Vec<(PortOwner, u32)>> {
        let mut out: Vec<(PortOwner, u32)> = self
            .inner
            .manager
            .list()
            .into_iter()
            .filter_map(|i| i.status.pid().filter(|&p| p != 0).map(|p| (i.owner, p)))
            .collect();
        for listed in self.run_dir().list()? {
            let Ok(record) = listed.record else { continue };
            let owner = record.owner();
            if out.iter().any(|(o, _)| *o == owner) || self.inner.manager.get(&owner).is_some() {
                continue;
            }
            if let RecordState::Running { pid, .. } = record.state() {
                out.push((owner, pid));
            }
        }
        Ok(out)
    }

    /// Samples the CPU and memory use of `pids`' process trees (blocking:
    /// reads the whole process table).
    pub(crate) fn sample_usage(&self, pids: &[u32]) -> HashMap<u32, Usage> {
        let mut monitor = self
            .inner
            .usage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        monitor.sample(pids)
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
        self.ensure_not_setting_up(name)?;
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
