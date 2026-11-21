//! Instance manager: every `odoo-bin` instance OdHive runs, across projects.
//!
//! A [`Manager`] owns the [`PortRegistry`] and one entry per instance
//! (`project#instance`). Each instance gets its own ports, database and
//! arguments from an [`InstancePlan`], usually built from the project with
//! [`InstancePlan::for_instance`].
//!
//! - **Start:** ports are allocated, the process is spawned under a
//!   [`supervisor`], and a PID file is written when
//!   [`ManagerOptions::run_dir`] is set. Two running instances never share a
//!   database.
//! - **Port race:** probing a port and Odoo binding it are not atomic. When
//!   the first process of an instance with automatic ports crashes with
//!   "Address already in use", its ports are released and allocated again,
//!   once ([`Notice::PortRetry`]). The status stays `starting` meanwhile.
//! - **Recovery:** [`Manager::recover`] re-attaches instances left behind by
//!   an OdHive process that was killed hard (see [`crate::pidfile`]). They
//!   run without output; [`Manager::replace`] restarts one under supervision.
//!   An attached instance that exits by itself is noticed by polling.
//! - **Status:** each instance has one status stream that survives restarts
//!   and the port retry ([`Manager::wait_for`]); events of all instances go
//!   to one broadcast channel ([`Manager::subscribe`]).
//!
//! Dropping the last [`Manager`] clone kills supervised instances and leaves
//! attached ones running; [`Manager::shutdown`] stops all of them gracefully.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use serde::Serialize;
use thiserror::Error;
use tokio::sync::{broadcast, watch};
use tokio::task::JoinSet;

use crate::pidfile::{self, Attached, AttachedStop, PidFileError, PidRecord, Recovered, RunDir};
use crate::ports::{AllocError, AllocOptions, PortOwner, PortRegistry};
use crate::project::{ProjectConfig, ProjectError};
use crate::supervisor::{
    self, DEFAULT_STOP_TIMEOUT, Event, ExitInfo, LaunchSpec, Status, SuperviseOptions, Supervised,
    SupervisorError,
};

#[cfg(test)]
mod tests;

/// How often a re-attached instance is checked for having exited.
pub const DEFAULT_ATTACHED_POLL: Duration = Duration::from_secs(1);

/// Events buffered per subscriber before the oldest are dropped.
pub const DEFAULT_EVENT_CAPACITY: usize = 4096;

/// Errors returned by the manager.
#[derive(Debug, Error)]
pub enum ManagerError {
    /// The instance is not known to the manager.
    #[error("{0} is not running")]
    NotRunning(PortOwner),
    /// The instance already runs.
    #[error("{0} is already running")]
    AlreadyRunning(PortOwner),
    /// The instance is being stopped.
    #[error("{0} is being stopped")]
    Busy(PortOwner),
    /// The project has no such instance.
    #[error("project {project:?} has no instance {instance:?}")]
    UnknownInstance {
        /// Project name.
        project: String,
        /// Requested instance id.
        instance: String,
    },
    /// Another running instance uses the database.
    #[error("database {db:?} is already used by {by}")]
    DatabaseInUse {
        /// Database name.
        db: String,
        /// Instance using it.
        by: PortOwner,
    },
    /// The instance was started by an earlier OdHive; it can only be
    /// restarted from its project settings ([`Manager::replace`]).
    #[error("{0} was started by an earlier OdHive; restart it from its project settings")]
    NoPlan(PortOwner),
    /// The project settings are invalid.
    #[error(transparent)]
    Project(#[from] ProjectError),
    /// Ports could not be allocated.
    #[error(transparent)]
    Ports(#[from] AllocError),
    /// The supervisor failed.
    #[error(transparent)]
    Supervisor(#[from] SupervisorError),
    /// PID files could not be read or removed.
    #[error(transparent)]
    PidFile(#[from] PidFileError),
}

/// Everything needed to start one instance.
#[derive(Debug, Clone)]
pub struct InstancePlan {
    /// The instance.
    pub owner: PortOwner,
    /// Pinned HTTP port; `None` = allocated automatically.
    pub http_port: Option<u16>,
    /// Pinned gevent port; `None` = allocated automatically.
    pub gevent_port: Option<u16>,
    /// What to run.
    pub spec: LaunchSpec,
}

impl InstancePlan {
    /// The plan for instance `instance_id` of `project`, run from
    /// `project_dir` by `python odoo_bin -c conf`. Ports, database, workers
    /// and extra arguments come from the instance, env from the project.
    ///
    /// # Errors
    /// [`ManagerError::Project`] if the project is invalid, or
    /// [`ManagerError::UnknownInstance`].
    pub fn for_instance(
        project: &ProjectConfig,
        project_dir: &Path,
        instance_id: &str,
        python: PathBuf,
        odoo_bin: PathBuf,
        conf: Option<PathBuf>,
    ) -> Result<Self, ManagerError> {
        project.validate()?;
        let instance = project
            .instances
            .iter()
            .find(|i| i.id == instance_id)
            .ok_or_else(|| ManagerError::UnknownInstance {
                project: project.name.clone(),
                instance: instance_id.to_owned(),
            })?;
        let mut spec = LaunchSpec::new(python, odoo_bin, project_dir.to_path_buf());
        spec.conf = conf;
        spec.db = Some(instance.db_name(&project.name));
        spec.workers = instance.workers;
        spec.extra_args.clone_from(&instance.extra_args);
        spec.env = project.env.clone();
        Ok(Self {
            owner: PortOwner::new(&project.name, &instance.id),
            http_port: instance.http_port,
            gevent_port: instance.gevent_port,
            spec,
        })
    }

    /// Both ports are allocated automatically, so a port race can be retried
    /// on other ports.
    #[must_use]
    pub fn ports_automatic(&self) -> bool {
        self.http_port.is_none() && self.gevent_port.is_none()
    }
}

/// Manager settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagerOptions {
    /// Where automatic ports are searched.
    pub alloc: AllocOptions,
    /// Time between SIGINT and SIGKILL when stopping.
    pub stop_timeout: Duration,
    /// Where PID files are kept; `None` writes none and recovers nothing.
    pub run_dir: Option<RunDir>,
    /// Retry once on new ports after an "Address already in use" crash.
    pub port_retry: bool,
    /// How often re-attached instances are checked.
    pub attached_poll: Duration,
    /// Events buffered per subscriber.
    pub event_capacity: usize,
}

impl Default for ManagerOptions {
    fn default() -> Self {
        Self {
            alloc: AllocOptions::default(),
            stop_timeout: DEFAULT_STOP_TIMEOUT,
            run_dir: None,
            port_retry: true,
            attached_poll: DEFAULT_ATTACHED_POLL,
            event_capacity: DEFAULT_EVENT_CAPACITY,
        }
    }
}

/// Something the manager itself reports, next to the supervisor's events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Notice {
    /// The first process lost the race for its port; the instance is started
    /// again on newly allocated ports. A [`Event::Spawned`] follows.
    PortRetry {
        /// HTTP port that failed.
        http_port: u16,
        /// Gevent port that failed.
        gevent_port: u16,
        /// How the process ended.
        exit: ExitInfo,
    },
    /// An instance of an earlier OdHive was re-attached. It runs without
    /// output until it is restarted under supervision.
    Attached {
        /// Process id.
        pid: u32,
        /// HTTP port.
        http_port: u16,
        /// Gevent port.
        gevent_port: u16,
    },
}

/// One event of one instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum InstanceEvent {
    /// From the instance's supervisor (attached instances report their
    /// status changes this way too).
    Supervisor(Event),
    /// From the manager.
    Manager(Notice),
}

/// An event, with the instance it belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ManagerEvent {
    /// The instance.
    #[serde(flatten)]
    pub owner: PortOwner,
    /// What happened.
    #[serde(flatten)]
    pub event: InstanceEvent,
}

/// What is known about one instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstanceInfo {
    /// The instance.
    #[serde(flatten)]
    pub owner: PortOwner,
    /// HTTP port, while ports are held.
    pub http_port: Option<u16>,
    /// Gevent port, while ports are held.
    pub gevent_port: Option<u16>,
    /// Database.
    pub db: Option<String>,
    /// Started by this OdHive (output is captured). `false` = re-attached.
    pub supervised: bool,
    /// The ports were allocated again after a port race.
    pub port_retried: bool,
    /// Current status.
    pub status: Status,
}

/// How [`Manager::stop`] ended an instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StopOutcome {
    /// A supervised instance; its final status.
    Supervised {
        /// Final status.
        status: Status,
    },
    /// A re-attached instance (no exit code is known).
    Attached {
        /// How it went.
        stop: AttachedStop,
    },
}

/// What [`Manager::recover`] found in one PID file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Recovery {
    /// Still running; attached again.
    Attached {
        /// The instance.
        #[serde(flatten)]
        owner: PortOwner,
        /// Process id.
        pid: u32,
        /// HTTP port.
        http_port: u16,
        /// Gevent port.
        gevent_port: u16,
        /// Its ports could not be reserved again.
        ports_error: Option<String>,
    },
    /// Supervised by another OdHive process that is still alive; left alone.
    SupervisedElsewhere {
        /// The instance.
        #[serde(flatten)]
        owner: PortOwner,
        /// Process id.
        pid: u32,
        /// The other OdHive process.
        by: u32,
    },
    /// The process was gone; the record was removed.
    Stale {
        /// The instance.
        #[serde(flatten)]
        owner: PortOwner,
        /// Process id it had.
        pid: u32,
        /// Leftover group members were killed.
        killed_leftovers: bool,
    },
    /// The file was unusable and was removed.
    Invalid {
        /// The file.
        path: PathBuf,
        /// Why.
        reason: String,
    },
}

/// Runs and tracks instances. Cheap to clone; clones share state. Needs a
/// Tokio runtime.
#[derive(Debug, Clone)]
pub struct Manager {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    registry: PortRegistry,
    opts: ManagerOptions,
    events: broadcast::Sender<ManagerEvent>,
    entries: Mutex<BTreeMap<PortOwner, Entry>>,
    next_gen: AtomicU64,
}

#[derive(Debug)]
struct Entry {
    /// Changes whenever the slot gets a new handle; tasks working on an
    /// older handle check it and back off.
    generation: u64,
    /// `None` for attached instances.
    plan: Option<InstancePlan>,
    status: Arc<watch::Sender<Status>>,
    http_port: Option<u16>,
    gevent_port: Option<u16>,
    db: Option<String>,
    port_retried: bool,
    slot: Slot,
}

#[derive(Debug)]
enum Slot {
    Supervised(Arc<Supervised>),
    /// `None` while being stopped.
    Attached(Option<Box<Attached>>),
    /// The port retry could not start a process.
    Failed,
}

impl Entry {
    fn info(&self, owner: &PortOwner) -> InstanceInfo {
        InstanceInfo {
            owner: owner.clone(),
            http_port: self.http_port,
            gevent_port: self.gevent_port,
            db: self.db.clone(),
            supervised: !matches!(self.slot, Slot::Attached(_)),
            port_retried: self.port_retried,
            status: self.status.borrow().clone(),
        }
    }

    /// No process is running and none is being stopped.
    fn is_done(&self) -> bool {
        match &self.slot {
            Slot::Supervised(_) | Slot::Failed => self.status.borrow().is_terminal(),
            Slot::Attached(_) => false,
        }
    }
}

impl Manager {
    /// A manager with its own port registry.
    #[must_use]
    pub fn new(opts: ManagerOptions) -> Self {
        Self::with_registry(PortRegistry::new(), opts)
    }

    /// A manager that allocates from `registry`.
    #[must_use]
    pub fn with_registry(registry: PortRegistry, opts: ManagerOptions) -> Self {
        let (events, _) = broadcast::channel(opts.event_capacity.max(1));
        Self {
            inner: Arc::new(Inner {
                registry,
                opts,
                events,
                entries: Mutex::new(BTreeMap::new()),
                next_gen: AtomicU64::new(1),
            }),
        }
    }

    /// The port registry.
    #[must_use]
    pub fn registry(&self) -> &PortRegistry {
        &self.inner.registry
    }

    /// Events of all instances from now on.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<ManagerEvent> {
        self.inner.events.subscribe()
    }

    /// Checks the PID files and attaches instances that still run. Call it
    /// once at startup, before starting instances. Without a run dir it does
    /// nothing.
    ///
    /// # Errors
    /// [`ManagerError::PidFile`] when the run dir cannot be read.
    pub fn recover(&self) -> Result<Vec<Recovery>, ManagerError> {
        let Some(run) = &self.inner.opts.run_dir else {
            return Ok(Vec::new());
        };
        let found = pidfile::recover(run, &self.inner.registry)?;
        let mut out = Vec::with_capacity(found.len());
        for r in found {
            out.push(match r {
                Recovered::Running {
                    instance,
                    ports_error,
                } => self.attach(instance, ports_error.map(|e| e.to_string())),
                Recovered::Supervised { record, by } => Recovery::SupervisedElsewhere {
                    owner: record.owner(),
                    pid: record.process.pid,
                    by: by.pid,
                },
                Recovered::Stale {
                    record,
                    killed_leftovers,
                } => Recovery::Stale {
                    owner: record.owner(),
                    pid: record.process.pid,
                    killed_leftovers,
                },
                Recovered::Invalid { path, reason } => Recovery::Invalid { path, reason },
            });
        }
        Ok(out)
    }

    fn attach(&self, instance: Attached, ports_error: Option<String>) -> Recovery {
        let record = instance.record().clone();
        let owner = record.owner();
        let pid = record.process.pid;
        let generation = self.inner.next_generation();
        {
            let mut entries = self.inner.lock();
            if entries.contains_key(&owner) {
                // Only possible when recover runs after starts; the process
                // keeps running and the next recovery finds it again.
                tracing::warn!(instance = %owner, "already known; not attached");
            } else {
                entries.insert(
                    owner.clone(),
                    Entry {
                        generation,
                        plan: None,
                        status: Arc::new(watch::channel(Status::Running { pid }).0),
                        http_port: Some(record.http_port),
                        gevent_port: Some(record.gevent_port),
                        db: record.db.clone(),
                        port_retried: false,
                        slot: Slot::Attached(Some(Box::new(instance))),
                    },
                );
                self.inner.notify(
                    &owner,
                    Notice::Attached {
                        pid,
                        http_port: record.http_port,
                        gevent_port: record.gevent_port,
                    },
                );
                tokio::spawn(watch_attached(
                    Arc::downgrade(&self.inner),
                    owner.clone(),
                    generation,
                    record.clone(),
                ));
            }
        }
        Recovery::Attached {
            owner,
            pid,
            http_port: record.http_port,
            gevent_port: record.gevent_port,
            ports_error,
        }
    }

    /// Starts an instance. An earlier run of it that has ended is replaced.
    ///
    /// # Errors
    /// [`ManagerError::AlreadyRunning`], [`ManagerError::DatabaseInUse`],
    /// [`ManagerError::Ports`] or [`ManagerError::Supervisor`].
    pub async fn start(&self, plan: InstancePlan) -> Result<InstanceInfo, ManagerError> {
        let owner = plan.owner.clone();
        let old = {
            let mut entries = self.inner.lock();
            match entries.get(&owner) {
                Some(e) if !e.is_done() => return Err(ManagerError::AlreadyRunning(owner)),
                Some(_) => entries.remove(&owner),
                None => None,
            }
        };
        let status = match old {
            Some(old) => {
                release(old.slot).await;
                Some(old.status)
            }
            None => None,
        };
        let mut entries = self.inner.lock();
        if entries.contains_key(&owner) {
            return Err(ManagerError::AlreadyRunning(owner));
        }
        self.inner.launch(&mut entries, plan, status, false)
    }

    /// Stops an instance gracefully and forgets it. Its ports are released.
    ///
    /// # Errors
    /// [`ManagerError::NotRunning`], [`ManagerError::Busy`] when a stop is
    /// already under way, or [`ManagerError::PidFile`].
    pub async fn stop(&self, owner: &PortOwner) -> Result<StopOutcome, ManagerError> {
        enum Job {
            Supervised(Arc<Supervised>),
            Attached(Box<Attached>, u32),
            Done(Status),
        }
        let (generation, status, job) = {
            let mut entries = self.inner.lock();
            let entry = entries
                .get_mut(owner)
                .ok_or_else(|| ManagerError::NotRunning(owner.clone()))?;
            let job = match &mut entry.slot {
                Slot::Supervised(h) => Job::Supervised(h.clone()),
                Slot::Attached(a) => {
                    let a = a.take().ok_or_else(|| ManagerError::Busy(owner.clone()))?;
                    let pid = a.record().process.pid;
                    Job::Attached(a, pid)
                }
                Slot::Failed => Job::Done(entry.status.borrow().clone()),
            };
            (entry.generation, entry.status.clone(), job)
        };
        let outcome = match job {
            Job::Supervised(handle) => {
                let stopped = handle.stop().await;
                drop(handle);
                match stopped {
                    Ok(status) => StopOutcome::Supervised { status },
                    Err(e) => {
                        // The supervisor is gone: nothing is left to stop.
                        if let Some(entry) = self.inner.remove_if(owner, generation) {
                            release(entry.slot).await;
                        }
                        return Err(e.into());
                    }
                }
            }
            Job::Attached(a, pid) => {
                self.inner
                    .set_status(owner, &status, Status::Stopping { pid });
                let stop = a.stop(self.inner.opts.stop_timeout).await;
                self.inner.set_status(owner, &status, attached_stopped());
                match stop {
                    Ok(stop) => StopOutcome::Attached { stop },
                    Err(e) => {
                        self.inner.remove_if(owner, generation);
                        return Err(e.into());
                    }
                }
            }
            Job::Done(status) => StopOutcome::Supervised { status },
        };
        if let Some(entry) = self.inner.remove_if(owner, generation) {
            release(entry.slot).await;
        }
        Ok(outcome)
    }

    /// Restarts a supervised instance on the same ports with the same
    /// settings (use [`Manager::replace`] for new settings).
    ///
    /// # Errors
    /// [`ManagerError::NotRunning`], [`ManagerError::NoPlan`] for a
    /// re-attached instance, or [`ManagerError::Supervisor`] when the new
    /// process cannot be started.
    pub async fn restart(&self, owner: &PortOwner) -> Result<InstanceInfo, ManagerError> {
        let (handle, plan) = {
            let entries = self.inner.lock();
            let entry = entries
                .get(owner)
                .ok_or_else(|| ManagerError::NotRunning(owner.clone()))?;
            match &entry.slot {
                Slot::Supervised(h) => (Some(h.clone()), None),
                Slot::Attached(_) => return Err(ManagerError::NoPlan(owner.clone())),
                Slot::Failed => (None, entry.plan.clone()),
            }
        };
        match (handle, plan) {
            (Some(handle), _) => {
                handle.restart().await?;
                drop(handle);
                self.get(owner)
                    .ok_or_else(|| ManagerError::NotRunning(owner.clone()))
            }
            (None, Some(plan)) => self.start(plan).await,
            (None, None) => Err(ManagerError::NoPlan(owner.clone())),
        }
    }

    /// Stops the instance if it runs (supervised or attached), then starts it
    /// from `plan`: new settings, or a re-attached instance taken back under
    /// supervision.
    ///
    /// # Errors
    /// See [`Manager::stop`] and [`Manager::start`].
    pub async fn replace(&self, plan: InstancePlan) -> Result<InstanceInfo, ManagerError> {
        match self.stop(&plan.owner).await {
            Ok(_) | Err(ManagerError::NotRunning(_)) => {}
            Err(e) => return Err(e),
        }
        self.start(plan).await
    }

    /// One instance.
    #[must_use]
    pub fn get(&self, owner: &PortOwner) -> Option<InstanceInfo> {
        self.inner.lock().get(owner).map(|e| e.info(owner))
    }

    /// All instances, sorted by project and instance.
    #[must_use]
    pub fn list(&self) -> Vec<InstanceInfo> {
        self.inner
            .lock()
            .iter()
            .map(|(owner, e)| e.info(owner))
            .collect()
    }

    /// The instances of one project.
    #[must_use]
    pub fn list_project(&self, project: &str) -> Vec<InstanceInfo> {
        self.inner
            .lock()
            .iter()
            .filter(|(owner, _)| owner.project == project)
            .map(|(owner, e)| e.info(owner))
            .collect()
    }

    /// A receiver for the instance's status. It follows restarts and the
    /// port retry, and closes when the instance is stopped.
    #[must_use]
    pub fn watch(&self, owner: &PortOwner) -> Option<watch::Receiver<Status>> {
        self.inner.lock().get(owner).map(|e| e.status.subscribe())
    }

    /// Waits until the instance's status matches `f` and returns it. When the
    /// instance is stopped first, returns its last status.
    ///
    /// # Errors
    /// [`ManagerError::NotRunning`] if the instance is unknown.
    pub async fn wait_for(
        &self,
        owner: &PortOwner,
        mut f: impl FnMut(&Status) -> bool,
    ) -> Result<Status, ManagerError> {
        let mut rx = self
            .watch(owner)
            .ok_or_else(|| ManagerError::NotRunning(owner.clone()))?;
        let found = rx.wait_for(|s| f(s)).await.map(|s| s.clone());
        Ok(found.unwrap_or_else(|_| rx.borrow().clone()))
    }

    /// Stops every instance, in parallel.
    pub async fn shutdown(&self) -> Vec<(PortOwner, Result<StopOutcome, ManagerError>)> {
        let owners: Vec<PortOwner> = self.inner.lock().keys().cloned().collect();
        let mut set = JoinSet::new();
        for owner in owners {
            let me = self.clone();
            set.spawn(async move {
                let r = me.stop(&owner).await;
                (owner, r)
            });
        }
        let mut out = Vec::new();
        while let Some(r) = set.join_next().await {
            if let Ok(r) = r {
                out.push(r);
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, BTreeMap<PortOwner, Entry>> {
        // Entries are inserted and removed whole, so the map is valid after a panic.
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn next_generation(&self) -> u64 {
        self.next_gen.fetch_add(1, Ordering::Relaxed)
    }

    fn send(&self, owner: &PortOwner, event: InstanceEvent) {
        // No subscribers is fine.
        let _ = self.events.send(ManagerEvent {
            owner: owner.clone(),
            event,
        });
    }

    fn notify(&self, owner: &PortOwner, notice: Notice) {
        self.send(owner, InstanceEvent::Manager(notice));
    }

    /// Sets and reports a status the manager decides (attached instances,
    /// failed retries).
    fn set_status(&self, owner: &PortOwner, tx: &watch::Sender<Status>, status: Status) {
        tx.send_replace(status.clone());
        self.send(owner, InstanceEvent::Supervisor(Event::Status { status }));
    }

    fn remove_if(&self, owner: &PortOwner, generation: u64) -> Option<Entry> {
        let mut entries = self.lock();
        if entries
            .get(owner)
            .is_some_and(|e| e.generation == generation)
        {
            entries.remove(owner)
        } else {
            None
        }
    }

    /// Allocates, spawns and inserts. `status` carries the status stream of
    /// an earlier run, so waiters follow the instance across runs.
    fn launch(
        self: &Arc<Self>,
        entries: &mut BTreeMap<PortOwner, Entry>,
        plan: InstancePlan,
        status: Option<Arc<watch::Sender<Status>>>,
        port_retried: bool,
    ) -> Result<InstanceInfo, ManagerError> {
        let owner = plan.owner.clone();
        if let Some(db) = &plan.spec.db
            && let Some((by, _)) = entries
                .iter()
                .find(|(o, e)| **o != owner && !e.is_done() && e.db.as_ref() == Some(db))
        {
            return Err(ManagerError::DatabaseInUse {
                db: db.clone(),
                by: by.clone(),
            });
        }
        let ports = self.registry.allocate(
            owner.clone(),
            plan.http_port,
            plan.gevent_port,
            &self.opts.alloc,
        )?;
        let generation = self.next_generation();
        let status =
            status.unwrap_or_else(|| Arc::new(watch::channel(Status::Starting { pid: 0 }).0));
        let retry = self.opts.port_retry && !port_retried && plan.ports_automatic();
        let on_event = forward(
            Arc::downgrade(self),
            owner.clone(),
            generation,
            status.clone(),
            retry,
        );
        let opts = SuperviseOptions {
            stop_timeout: self.opts.stop_timeout,
            run_dir: self.opts.run_dir.clone(),
        };
        let handle = supervisor::start(plan.spec.clone(), ports, opts, on_event)?;
        let entry = Entry {
            generation,
            http_port: Some(handle.http_port()),
            gevent_port: Some(handle.gevent_port()),
            db: plan.spec.db.clone(),
            plan: Some(plan),
            status,
            port_retried,
            slot: Slot::Supervised(Arc::new(handle)),
        };
        let info = entry.info(&owner);
        entries.insert(owner, entry);
        Ok(info)
    }
}

/// The status attached instances end with: no exit code is known.
fn attached_stopped() -> Status {
    Status::Stopped {
        exit: ExitInfo {
            code: None,
            signal: None,
            port_in_use: false,
            tail: String::new(),
        },
    }
}

/// Ends what a removed entry still holds: a supervised handle is shut down,
/// so its ports are free once this returns.
async fn release(slot: Slot) {
    if let Slot::Supervised(handle) = slot
        && let Some(handle) = Arc::into_inner(handle)
    {
        // Gone = the task already ended; nothing is left to release.
        let _ = handle.shutdown().await;
    }
}

/// The supervisor's event callback: keeps the instance's status stream and
/// forwards events, and turns the first port-race crash into a retry.
fn forward(
    inner: Weak<Inner>,
    owner: PortOwner,
    generation: u64,
    status: Arc<watch::Sender<Status>>,
    retry: bool,
) -> impl FnMut(Event) + Send + 'static {
    let mut spawns = 0_u32;
    move |event| {
        let Some(me) = inner.upgrade() else {
            return;
        };
        match &event {
            Event::Spawned { .. } => spawns += 1,
            Event::Status {
                status: Status::Crashed { exit },
            } if retry && spawns == 1 && exit.port_in_use => {
                tokio::spawn(retry_ports(me, owner.clone(), generation, exit.clone()));
                return;
            }
            Event::Status { status: s } => {
                status.send_replace(s.clone());
            }
            Event::Output { .. } => {}
        }
        me.send(&owner, InstanceEvent::Supervisor(event));
    }
}

/// Starts the instance again on newly allocated ports after its first
/// process crashed with "Address already in use".
async fn retry_ports(inner: Arc<Inner>, owner: PortOwner, generation: u64, exit: ExitInfo) {
    let crashed = Status::Crashed { exit: exit.clone() };
    let taken = {
        let mut entries = inner.lock();
        let ours = entries.get(&owner).is_some_and(|e| {
            e.generation == generation
                && matches!(&e.slot, Slot::Supervised(h) if Arc::strong_count(h) == 1)
        });
        if ours {
            entries.remove(&owner)
        } else {
            if let Some(e) = entries.get(&owner).filter(|e| e.generation == generation) {
                // Someone else is acting on it right now: report the crash.
                inner.set_status(&owner, &e.status, crashed);
            }
            None
        }
    };
    let Some(entry) = taken else {
        return;
    };
    let (http_port, gevent_port) = (
        entry.http_port.unwrap_or_default(),
        entry.gevent_port.unwrap_or_default(),
    );
    let (plan, status) = (entry.plan.clone(), entry.status.clone());
    release(entry.slot).await;
    let Some(plan) = plan else {
        return;
    };
    inner.notify(
        &owner,
        Notice::PortRetry {
            http_port,
            gevent_port,
            exit,
        },
    );
    let mut entries = inner.lock();
    if entries.contains_key(&owner) {
        // Started again by someone else in the meantime.
        return;
    }
    if let Err(e) = inner.launch(&mut entries, plan.clone(), Some(status.clone()), true) {
        inner.set_status(
            &owner,
            &status,
            Status::FailedToStart {
                error: e.to_string(),
            },
        );
        entries.insert(
            owner,
            Entry {
                generation: inner.next_generation(),
                db: plan.spec.db.clone(),
                plan: Some(plan),
                status,
                http_port: None,
                gevent_port: None,
                port_retried: true,
                slot: Slot::Failed,
            },
        );
    }
}

/// Notices when a re-attached instance exits by itself, and forgets it.
async fn watch_attached(inner: Weak<Inner>, owner: PortOwner, generation: u64, record: PidRecord) {
    let Some(poll) = inner.upgrade().map(|i| i.opts.attached_poll) else {
        return;
    };
    let mut tick = tokio::time::interval(poll);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if record.process.is_alive() {
            continue;
        }
        let Some(me) = inner.upgrade() else {
            return;
        };
        let (attached, status) = {
            let mut entries = me.lock();
            let Some(entry) = entries
                .get_mut(&owner)
                .filter(|e| e.generation == generation)
            else {
                return;
            };
            let Slot::Attached(slot) = &mut entry.slot else {
                return;
            };
            if slot.is_none() {
                // Being stopped.
                continue;
            }
            let attached = slot.take();
            let status = entry.status.clone();
            entries.remove(&owner);
            (attached, status)
        };
        if let Some(a) = attached {
            // Already gone: removes the record, kills leftovers, frees ports.
            if let Err(e) = a.stop(Duration::ZERO).await {
                tracing::warn!(instance = %owner, error = %e, "could not clean up");
            }
        }
        me.set_status(&owner, &status, attached_stopped());
        return;
    }
}
