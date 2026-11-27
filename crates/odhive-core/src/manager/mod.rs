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
//!   to the bus's instance topic ([`Manager::subscribe`]).
//! - **Logs:** output lines of a supervised instance go straight from its
//!   supervisor into the instance's log (parser, in-memory buffer and
//!   `<logs_dir>/<instance>.log`), so none is lost to a slow subscriber.
//!   Completed entries go to the bus's log topic ([`Manager::subscribe_logs`])
//!   and stay readable by sequence number ([`Manager::read_log`]). The log
//!   survives restarts and the port retry, and goes when the instance is
//!   stopped. Re-attached instances have no output, so no log.
//! - **Dependency auto-fix:** with [`InstancePlan::autofix`] set, an entry
//!   reporting a missing Python module is handled per [`crate::autofix`]:
//!   the package is installed into the venv and the instance restarted on
//!   its ports ([`Notice::DependencyInstalling`] …), or the user is asked
//!   ([`Manager::confirm_fix`]). Meanwhile a crashed instance is not done.
//! - **Smart Reload:** with [`InstancePlan::reload`] set, the instance's
//!   custom addons folders are watched per [`crate::reload`]: a change
//!   restarts it, restarts it with `-u <modules>`, or asks for a browser
//!   refresh ([`Notice::Reload`]). A crashed instance that is watched is not
//!   done: saving the fix starts it again.
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
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::autofix::{Candidate, GiveUp, Trust};
use crate::events::{BusOptions, EventBus, LogEvent, Subscription};
use crate::logs::store::{Page, Record};
use crate::logs::{LOGS_DIR, LogSettings};
use crate::pidfile::{self, Attached, AttachedStop, PidFileError, PidRecord, Recovered, RunDir};
use crate::ports::{AllocError, AllocOptions, PortChange, PortOwner, PortRegistry};
use crate::project::{ProjectConfig, ProjectError};
use crate::reload::Action;
use crate::requirements::Requirement;
use crate::supervisor::{
    self, DEFAULT_STOP_TIMEOUT, Event, ExitInfo, LaunchSpec, Status, SuperviseOptions, Supervised,
    SupervisorError,
};
use crate::syslib::Hint;

mod fix;
mod log;
mod reload;
#[cfg(test)]
mod tests;

use fix::FixState;
pub use fix::{AutoFix, FixAction, FixInfo};
pub use log::{IDLE_FLUSH, LogHandle};
use log::{LogSlot, LogTarget};
use reload::ReloadState;
pub use reload::{Reload, ReloadInfo};

/// How often a re-attached instance is checked for having exited.
pub const DEFAULT_ATTACHED_POLL: Duration = Duration::from_secs(1);

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
    /// No missing dependency awaits confirmation.
    #[error("{0} has no missing dependency waiting for confirmation")]
    NoPendingFix(PortOwner),
    /// The pending dependency cannot be installed any more.
    #[error("{owner}: {reason}")]
    FixRefused {
        /// The instance.
        owner: PortOwner,
        /// Why.
        reason: GiveUp,
    },
    /// The instance is installing a missing dependency.
    #[error("{0} is installing a missing dependency")]
    Fixing(PortOwner),
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
    /// Folder of the instance's log file; `None` keeps its log in memory.
    pub logs_dir: Option<PathBuf>,
    /// Log buffer and file limits.
    pub log_settings: LogSettings,
    /// Missing-dependency handling; `None` = off.
    pub autofix: Option<AutoFix>,
    /// Smart Reload; `None` = off.
    pub reload: Option<Reload>,
}

impl InstancePlan {
    /// The plan for instance `instance_id` of `project`, run from
    /// `project_dir` by `python odoo_bin -c conf`. Ports, database, workers
    /// and extra arguments come from the instance, log settings from the
    /// project, env from both (the instance's own variables win). The log file goes to `<project_dir>/logs/`.
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
        spec.sql_log = instance.sql_log;
        spec.extra_args.clone_from(&instance.extra_args);
        spec.env = instance.merged_env(&project.env);
        Ok(Self {
            owner: PortOwner::new(&project.name, &instance.id),
            http_port: instance.http_port,
            gevent_port: instance.gevent_port,
            spec,
            logs_dir: Some(project_dir.join(LOGS_DIR)),
            log_settings: project.logs,
            autofix: None,
            reload: None,
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
    /// Event bus capacities.
    pub bus: BusOptions,
}

impl Default for ManagerOptions {
    fn default() -> Self {
        Self {
            alloc: AllocOptions::default(),
            stop_timeout: DEFAULT_STOP_TIMEOUT,
            run_dir: None,
            port_retry: true,
            attached_poll: DEFAULT_ATTACHED_POLL,
            bus: BusOptions::default(),
        }
    }
}

/// Something the manager itself reports, next to the supervisor's events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize, utoipa::ToSchema)]
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
    /// The instance's log file could not be opened or written; its log is
    /// kept in memory only until it is started again.
    LogFile {
        /// What failed.
        error: String,
    },
    /// The log reports a missing Python module.
    DependencyMissing {
        /// Top-level import name.
        module: String,
        /// Package that provides it, when one can be named.
        package: Option<String>,
        /// Why the package is trusted; `None` = untrusted (or no package).
        trust: Option<Trust>,
        /// What happens next.
        action: FixAction,
    },
    /// The package is being installed into the instance's venv.
    DependencyInstalling {
        /// Missing module.
        module: String,
        /// Package.
        package: String,
        /// What the installer gets.
        requirements: Vec<Requirement>,
        /// Install number since the instance was started (from 1).
        attempt: u32,
        /// Installs allowed per start.
        max: u32,
    },
    /// The package was installed; the instance restarts.
    DependencyInstalled {
        /// Package.
        package: String,
    },
    /// The install failed; the instance is left as it is.
    DependencyFailed {
        /// Package.
        package: String,
        /// Installer error (with its last output lines).
        error: String,
        /// The system library the build was probably missing.
        #[serde(skip_serializing_if = "Option::is_none")]
        hint: Option<Hint>,
    },
    /// The log reports a missing system library or tool (e.g.
    /// `wkhtmltopdf`). Reported once per library while the instance's log
    /// lives; OdHive never installs system packages.
    SystemLibraryMissing {
        /// What to install, as text.
        hint: Hint,
    },
    /// Smart Reload watches the instance's addons folders.
    ReloadWatching {
        /// Watched folders.
        #[schema(value_type = Vec<String>)]
        roots: Vec<PathBuf>,
        /// Modules found in them.
        modules: usize,
        /// Module folders that link out of their addons folder; changes in
        /// them are not seen.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        #[schema(value_type = Vec<String>)]
        unwatched: Vec<PathBuf>,
    },
    /// The addons folders could not be watched; Smart Reload is off for this
    /// start.
    ReloadFailed {
        /// Why.
        error: String,
    },
    /// Files changed: the instance restarts ([`Action::Restart`], with `-u`
    /// for [`Action::Update`]) or the browser should reload the page
    /// ([`Action::RefreshBrowser`]).
    Reload {
        /// What happens.
        action: Action,
        /// Changed files (at most [`crate::reload::MAX_LISTED_FILES`]).
        #[schema(value_type = Vec<String>)]
        files: Vec<PathBuf>,
        /// Number of changed files.
        file_count: usize,
    },
    /// A new module appeared in an addons folder. It is not installed
    /// automatically; it needs a restart with `-i <module>` or the Apps menu
    /// (after "Update Apps List").
    ModuleAdded {
        /// Module name.
        module: String,
    },
}

/// One event of one instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize, utoipa::ToSchema)]
#[serde(untagged)]
pub enum InstanceEvent {
    /// From the instance's supervisor (attached instances report their
    /// status changes this way too). Never [`Event::Output`]: output lines
    /// reach the log topic as parsed entries.
    Supervisor(Event),
    /// From the manager.
    Manager(Notice),
}

/// An event, with the instance it belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct ManagerEvent {
    /// The instance.
    #[serde(flatten)]
    pub owner: PortOwner,
    /// What happened.
    #[serde(flatten)]
    pub event: InstanceEvent,
}

/// What is known about one instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize, utoipa::ToSchema)]
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
    /// Automatic ports that moved away from their starting point when the
    /// ports were allocated ("8069 busy → 8070").
    pub port_changes: Vec<PortChange>,
    /// Current status.
    pub status: Status,
    /// Dependency auto-fix, when enabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub autofix: Option<FixInfo>,
    /// Smart Reload, when enabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reload: Option<ReloadInfo>,
}

impl InstanceInfo {
    /// No process runs and none will without being asked: the status is
    /// terminal, no missing dependency is being installed, and it did not
    /// crash while Smart Reload watches (a saved fix restarts it). A fix
    /// awaiting confirmation does not count.
    #[must_use]
    pub fn is_done(&self) -> bool {
        let failed = matches!(
            self.status,
            Status::Crashed { .. } | Status::FailedToStart { .. }
        );
        self.status.is_terminal()
            && !self.autofix.as_ref().is_some_and(|f| f.active)
            && !(failed && self.reload.as_ref().is_some_and(|r| r.watching))
    }
}

/// How [`Manager::stop`] ended an instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
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
    bus: EventBus,
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
    port_changes: Vec<PortChange>,
    /// `None` for attached instances.
    log: Option<Arc<LogSlot>>,
    /// `None` when auto-fix is off or the instance is attached.
    fix: Option<Arc<FixState>>,
    /// `None` when Smart Reload is off or the instance is attached.
    reload: Option<Arc<ReloadState>>,
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
            port_changes: self.port_changes.clone(),
            status: self.status.borrow().clone(),
            autofix: self.fix.as_ref().map(|f| f.info()),
            reload: self.reload.as_ref().map(|r| r.info()),
        }
    }

    /// No process is running, none is being stopped and no missing
    /// dependency is being installed.
    fn is_done(&self) -> bool {
        match &self.slot {
            Slot::Supervised(_) | Slot::Failed => {
                self.status.borrow().is_terminal() && !self.is_fixing()
            }
            Slot::Attached(_) => false,
        }
    }

    fn is_fixing(&self) -> bool {
        self.fix.as_ref().is_some_and(|f| f.active())
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
        let bus = EventBus::new(opts.bus);
        Self {
            inner: Arc::new(Inner {
                registry,
                opts,
                bus,
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

    /// The bus the manager publishes to.
    #[must_use]
    pub fn bus(&self) -> &EventBus {
        &self.inner.bus
    }

    /// Spawns, status changes and notices of all instances from now on.
    #[must_use]
    pub fn subscribe(&self) -> Subscription<ManagerEvent> {
        self.inner.bus.instances().subscribe()
    }

    /// Log entries of all instances from now on. After a
    /// [`crate::events::Delivery::Lagged`], [`Manager::read_log`] from the
    /// last sequence number seen fills the gap.
    #[must_use]
    pub fn subscribe_logs(&self) -> Subscription<LogEvent> {
        self.inner.bus.logs().subscribe()
    }

    fn log_of(&self, owner: &PortOwner) -> Option<Arc<LogSlot>> {
        self.inner.lock().get(owner).and_then(|e| e.log.clone())
    }

    /// Up to `limit` buffered entries of the instance, from sequence number
    /// `from` on (see [`crate::logs::store::LogBuffer::read_from`]). `None`
    /// for an unknown or re-attached instance.
    #[must_use]
    pub fn read_log(&self, owner: &PortOwner, from: u64, limit: usize) -> Option<Page> {
        Some(self.log_of(owner)?.read_from(from, limit))
    }

    /// Like [`Manager::read_log`], with the id of the log read. An instance
    /// gets a new log, whose sequence numbers start again at 1, when it is
    /// started after a stop or with other log settings; ids are unique
    /// within the process.
    #[must_use]
    pub fn read_log_with_id(
        &self,
        owner: &PortOwner,
        from: u64,
        limit: usize,
    ) -> Option<(u64, Page)> {
        let log = self.log_of(owner)?;
        Some((log.id(), log.read_from(from, limit)))
    }

    /// The id of the instance's log (see [`Manager::read_log_with_id`]) and
    /// the sequence number its next entry gets.
    #[must_use]
    pub fn log_position(&self, owner: &PortOwner) -> Option<(u64, u64)> {
        let log = self.log_of(owner)?;
        Some((log.id(), log.next_seq()))
    }

    /// The instance's current log; `None` for an unknown or re-attached
    /// instance.
    #[must_use]
    pub fn log(&self, owner: &PortOwner) -> Option<LogHandle> {
        self.log_of(owner).map(LogHandle)
    }

    /// The last `n` buffered entries of the instance.
    #[must_use]
    pub fn tail_log(&self, owner: &PortOwner, n: usize) -> Option<Vec<Arc<Record>>> {
        Some(self.log_of(owner)?.tail(n))
    }

    /// The sequence number the instance's next entry gets: subscribe, then
    /// read up to it to see every entry exactly once.
    #[must_use]
    pub fn log_next_seq(&self, owner: &PortOwner) -> Option<u64> {
        Some(self.log_of(owner)?.next_seq())
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
                        port_changes: Vec::new(),
                        log: None,
                        fix: None,
                        reload: None,
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
                Some(e) if e.is_fixing() => return Err(ManagerError::Fixing(owner)),
                Some(e) if !e.is_done() => return Err(ManagerError::AlreadyRunning(owner)),
                Some(_) => entries.remove(&owner),
                None => None,
            }
        };
        let (status, log) = match old {
            Some(old) => {
                release(old.slot).await;
                (Some(old.status), old.log)
            }
            None => (None, None),
        };
        let mut entries = self.inner.lock();
        if entries.contains_key(&owner) {
            return Err(ManagerError::AlreadyRunning(owner));
        }
        self.inner
            .launch(&mut entries, plan, status, log, None, None, false)
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
            if let Some(fix) = &entry.fix {
                fix.cancel();
            }
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
            if entry.is_fixing() {
                return Err(ManagerError::Fixing(owner.clone()));
            }
            if let Some(fix) = &entry.fix {
                fix.reset();
            }
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

    /// Installs the missing dependency awaiting confirmation (see
    /// [`FixAction::Confirm`]) and restarts the instance, like the automatic
    /// path. Returns once the install is under way (it runs in the
    /// background, so a long install does not hold up an API request); the
    /// outcome is reported as notices. Confirming is what makes an untrusted
    /// package installable.
    ///
    /// # Errors
    /// [`ManagerError::NotRunning`], [`ManagerError::NoPendingFix`], or
    /// [`ManagerError::FixRefused`] when the attempt limit was reached.
    pub async fn confirm_fix(&self, owner: &PortOwner) -> Result<Candidate, ManagerError> {
        let fix = self.fix_of(owner)?;
        let (candidate, attempt) = match fix.take_pending() {
            None => return Err(ManagerError::NoPendingFix(owner.clone())),
            Some(Err(reason)) => {
                return Err(ManagerError::FixRefused {
                    owner: owner.clone(),
                    reason,
                });
            }
            Some(Ok(c)) => c,
        };
        let inner = Arc::downgrade(&self.inner);
        let (owner, task_candidate) = (owner.clone(), candidate.clone());
        tokio::spawn(async move {
            fix::apply(&inner, &owner, &fix, &task_candidate, attempt).await;
        });
        Ok(candidate)
    }

    /// Drops the missing dependency awaiting confirmation.
    ///
    /// # Errors
    /// [`ManagerError::NotRunning`] or [`ManagerError::NoPendingFix`].
    pub fn decline_fix(&self, owner: &PortOwner) -> Result<Candidate, ManagerError> {
        self.fix_of(owner)?
            .decline()
            .ok_or_else(|| ManagerError::NoPendingFix(owner.clone()))
    }

    fn fix_of(&self, owner: &PortOwner) -> Result<Arc<FixState>, ManagerError> {
        let entries = self.inner.lock();
        let entry = entries
            .get(owner)
            .ok_or_else(|| ManagerError::NotRunning(owner.clone()))?;
        entry
            .fix
            .clone()
            .ok_or_else(|| ManagerError::NoPendingFix(owner.clone()))
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
        self.bus.instances().publish(ManagerEvent {
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

    /// Allocates, spawns and inserts. `status` and `log` carry the status
    /// stream and log of an earlier run, so waiters and readers follow the
    /// instance across runs; the log is kept only if it goes to the same
    /// place with the same limits. `fix` and `reload` carry those of the
    /// run the port retry replaces.
    #[allow(clippy::too_many_arguments)]
    fn launch(
        self: &Arc<Self>,
        entries: &mut BTreeMap<PortOwner, Entry>,
        plan: InstancePlan,
        status: Option<Arc<watch::Sender<Status>>>,
        log: Option<Arc<LogSlot>>,
        fix: Option<Arc<FixState>>,
        reload: Option<Arc<ReloadState>>,
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
        let port_changes = ports.changes().to_vec();
        let generation = self.next_generation();
        let status =
            status.unwrap_or_else(|| Arc::new(watch::channel(Status::Starting { pid: 0 }).0));
        let target = LogTarget {
            dir: plan.logs_dir.clone(),
            settings: plan.log_settings,
        };
        let mut log_error = None;
        let log = match log.filter(|l| *l.target() == target) {
            Some(log) => log,
            None => {
                let (log, error) = LogSlot::new(
                    owner.clone(),
                    target,
                    Arc::downgrade(self),
                    self.bus.logs().clone(),
                );
                log_error = error;
                log
            }
        };
        let fix = fix.or_else(|| plan.autofix.clone().map(FixState::new));
        log.set_watcher(
            fix.as_ref()
                .map(|f| f.watch(Arc::downgrade(self), owner.clone())),
        );
        let retry = self.opts.port_retry && !port_retried && plan.ports_automatic();
        let on_event = forward(
            Arc::downgrade(self),
            owner.clone(),
            generation,
            status.clone(),
            log.clone(),
            retry,
        );
        let opts = SuperviseOptions {
            stop_timeout: self.opts.stop_timeout,
            run_dir: self.opts.run_dir.clone(),
        };
        let handle = supervisor::start(plan.spec.clone(), ports, opts, on_event)?;
        let reload = reload.or_else(|| {
            plan.reload
                .clone()
                .and_then(|r| ReloadState::start(r, Arc::downgrade(self), owner.clone()))
        });
        let entry = Entry {
            generation,
            http_port: Some(handle.http_port()),
            gevent_port: Some(handle.gevent_port()),
            db: plan.spec.db.clone(),
            plan: Some(plan),
            status,
            port_retried,
            port_changes,
            log: Some(log),
            fix,
            reload,
            slot: Slot::Supervised(Arc::new(handle)),
        };
        let info = entry.info(&owner);
        if let Some(e) = log_error {
            self.notify(
                &owner,
                Notice::LogFile {
                    error: e.to_string(),
                },
            );
        }
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

/// The supervisor's event callback: feeds output into the instance's log,
/// keeps its status stream and forwards events, and turns the first
/// port-race crash into a retry.
fn forward(
    inner: Weak<Inner>,
    owner: PortOwner,
    generation: u64,
    status: Arc<watch::Sender<Status>>,
    log: Arc<LogSlot>,
    retry: bool,
) -> impl FnMut(Event) + Send + 'static {
    let mut spawns = 0_u32;
    // The file error is reported once; the log then stays in memory.
    let mut file_failed = false;
    move |event| {
        let Some(me) = inner.upgrade() else {
            return;
        };
        let file_error = match &event {
            Event::Output { line, .. } => log.line(line),
            Event::Spawned { pid, .. } => {
                spawns += 1;
                log.started(*pid)
            }
            Event::Status { status: s } => {
                if s.is_terminal() {
                    log.flush();
                }
                None
            }
        };
        if let Some(e) = file_error
            && !file_failed
        {
            file_failed = true;
            me.notify(
                &owner,
                Notice::LogFile {
                    error: e.to_string(),
                },
            );
        }
        match &event {
            Event::Output { .. } => return,
            Event::Spawned { .. } => {}
            Event::Status {
                status: Status::Crashed { exit },
            } if retry && spawns == 1 && exit.port_in_use => {
                tokio::spawn(retry_ports(me, owner.clone(), generation, exit.clone()));
                return;
            }
            Event::Status { status: s } => {
                status.send_replace(s.clone());
            }
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
    let (plan, status, log, fix, reload) = (
        entry.plan.clone(),
        entry.status.clone(),
        entry.log.clone(),
        entry.fix.clone(),
        entry.reload.clone(),
    );
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
    if let Err(e) = inner.launch(
        &mut entries,
        plan.clone(),
        Some(status.clone()),
        log.clone(),
        fix.clone(),
        reload.clone(),
        true,
    ) {
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
                port_changes: Vec::new(),
                log,
                fix,
                reload,
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
