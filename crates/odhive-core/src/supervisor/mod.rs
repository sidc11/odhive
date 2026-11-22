//! Supervisor: runs one `odoo-bin` instance and watches over it.
//!
//! [`start`] spawns `python odoo-bin -c odoo.conf --http-port … --gevent-port …
//! -d … --workers … <extra args>` from an argument vector (never a shell), in
//! its own process group, with stdin closed and stdout/stderr streamed as
//! [`Event::Output`] lines. A background task owns the process and the
//! instance's [`Reservation`], and reports every [`Status`] change.
//!
//! - **Ready:** the instance is `running` once its HTTP port accepts a
//!   connection on `127.0.0.1`.
//! - **Stop:** SIGINT to the group (Odoo's graceful shutdown, like Ctrl-C in a
//!   terminal), then SIGKILL to the group after [`SuperviseOptions::stop_timeout`].
//! - **Crash:** an exit nobody asked for, with a non-zero code or a signal, is
//!   `crashed`; an unrequested exit with code 0 is `stopped`. "Address already
//!   in use" in the output is flagged in [`ExitInfo::port_in_use`], because the
//!   port probe and Odoo's own bind are not atomic.
//! - **Cleanup:** once the main process has exited, whatever is left of its
//!   group (workers, report renderers) is killed, so no orphans remain.
//!   Dropping the [`Supervised`] handle kills the group too.
//!
//! The reservation is held until the handle is shut down or dropped, so a
//! restart reuses the same ports.
//!
//! With [`SuperviseOptions::run_dir`] set, every spawned process gets a PID
//! file (see [`crate::pidfile`]) that is removed once the process is gone, so
//! an instance left behind by an OdHive that was killed hard can be found.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use rustix::process::{Signal, kill_process_group};
use serde::Serialize;
use thiserror::Error;
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior};

use crate::names::{self, NameError};
use crate::pidfile::{PidFileError, PidGuard, PidRecord, ProcessId, RECORD_VERSION, RunDir};
use crate::ports::{PortChange, PortOwner, Reservation};
use crate::proc::{self, GroupKill};
use crate::project::{self, EnvVars, MAX_WORKERS};

#[cfg(test)]
mod tests;

/// Default time a graceful stop may take before the group is killed.
pub const DEFAULT_STOP_TIMEOUT: Duration = Duration::from_secs(15);

/// How often the HTTP port is probed while an instance is starting.
const READY_POLL: Duration = Duration::from_millis(100);

/// Longest a single readiness probe may take.
const READY_CONNECT_TIMEOUT: Duration = Duration::from_millis(250);

/// How long output still in the pipes is read after the process exited.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Output lines kept in [`ExitInfo::tail`].
const TAIL_LINES: usize = 20;

/// Errors returned by the supervisor.
#[derive(Debug, Error)]
pub enum SupervisorError {
    /// The database name is not valid.
    #[error(transparent)]
    Name(#[from] NameError),
    /// An extra argument sets an option OdHive manages itself.
    #[error("extra argument {0:?} sets an option OdHive manages; remove it")]
    ManagedOption(String),
    /// Too many workers.
    #[error("workers = {0} is above the limit of {MAX_WORKERS}")]
    TooManyWorkers(u32),
    /// The process could not be started.
    #[error("failed to start {program}: {source}")]
    Spawn {
        /// Program that failed to start.
        program: PathBuf,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
    /// A restart stopped the process but could not start it again.
    #[error("restart failed: {0}")]
    Restart(String),
    /// The PID file could not be written; the process was killed again.
    #[error("could not write the pid file: {0}")]
    PidFile(#[from] PidFileError),
    /// The supervisor task is gone (the runtime is shutting down).
    #[error("the supervisor for {0} is no longer running")]
    Gone(PortOwner),
}

/// What to run for one instance. Ports come from the [`Reservation`].
#[derive(Debug, Clone)]
pub struct LaunchSpec {
    /// Interpreter that runs `odoo-bin` (the project venv's Python).
    pub python: PathBuf,
    /// `odoo-bin` script of the Community source.
    pub odoo_bin: PathBuf,
    /// Generated `odoo.conf`, passed as `-c`.
    pub conf: Option<PathBuf>,
    /// Working directory (the project folder).
    pub cwd: PathBuf,
    /// Database, passed as `-d`.
    pub db: Option<String>,
    /// `--workers` (0 = threaded).
    pub workers: u32,
    /// Extra arguments, appended as separate argv entries.
    pub extra_args: Vec<String>,
    /// Environment variables set on top of OdHive's own environment.
    pub env: EnvVars,
}

impl LaunchSpec {
    /// A spec with no config file, database, workers, extra arguments or env.
    #[must_use]
    pub fn new(python: PathBuf, odoo_bin: PathBuf, cwd: PathBuf) -> Self {
        Self {
            python,
            odoo_bin,
            conf: None,
            cwd,
            db: None,
            workers: 0,
            extra_args: Vec::new(),
            env: EnvVars::default(),
        }
    }

    /// Checks what [`crate::project::ProjectConfig::validate`] checks too, so
    /// a spec built by hand cannot smuggle in a managed option or a bad name.
    ///
    /// # Errors
    /// [`SupervisorError::Name`], [`SupervisorError::ManagedOption`] or
    /// [`SupervisorError::TooManyWorkers`].
    pub fn validate(&self) -> Result<(), SupervisorError> {
        if let Some(db) = &self.db {
            names::validate_db_name(db)?;
        }
        if self.workers > MAX_WORKERS {
            return Err(SupervisorError::TooManyWorkers(self.workers));
        }
        if let Some(arg) = self
            .extra_args
            .iter()
            .find(|a| project::is_managed_option(a))
        {
            return Err(SupervisorError::ManagedOption(arg.clone()));
        }
        for key in self.env.0.keys() {
            names::validate_env_key(key)?;
        }
        Ok(())
    }

    /// Arguments after `odoo-bin` for the given ports.
    #[must_use]
    pub fn odoo_args(&self, http: u16, gevent: u16) -> Vec<OsString> {
        self.odoo_args_with(http, gevent, &[])
    }

    /// [`LaunchSpec::odoo_args`] followed by `once`: arguments for one
    /// process only (e.g. `-u <module>` after a data file changed).
    #[must_use]
    pub fn odoo_args_with(&self, http: u16, gevent: u16, once: &[OsString]) -> Vec<OsString> {
        let mut args: Vec<OsString> = Vec::new();
        if let Some(conf) = &self.conf {
            args.push("-c".into());
            args.push(conf.into());
        }
        args.push("--http-port".into());
        args.push(http.to_string().into());
        args.push("--gevent-port".into());
        args.push(gevent.to_string().into());
        if let Some(db) = &self.db {
            args.push("-d".into());
            args.push(db.into());
        }
        args.push("--workers".into());
        args.push(self.workers.to_string().into());
        args.extend(self.extra_args.iter().map(OsString::from));
        args.extend(once.iter().cloned());
        args
    }

    /// The full command line (`python odoo-bin …`), for display.
    #[must_use]
    pub fn argv(&self, http: u16, gevent: u16) -> Vec<OsString> {
        let mut argv = vec![self.python.clone().into(), self.odoo_bin.clone().into()];
        argv.extend(self.odoo_args(http, gevent));
        argv
    }

    fn spawn(&self, http: u16, gevent: u16, once: &[OsString]) -> Result<Child, SupervisorError> {
        let mut cmd = Command::new(&self.python);
        cmd.arg(&self.odoo_bin)
            .args(self.odoo_args_with(http, gevent, once))
            .current_dir(&self.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        for key in proc::PYTHON_ENV_OVERRIDES {
            cmd.env_remove(key);
        }
        cmd.env("PYTHONUNBUFFERED", "1");
        cmd.envs(&self.env.0);
        cmd.spawn().map_err(|source| SupervisorError::Spawn {
            program: self.python.clone(),
            source,
        })
    }
}

/// Supervisor settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuperviseOptions {
    /// Time between SIGINT and SIGKILL when stopping.
    pub stop_timeout: Duration,
    /// Where PID files are kept; `None` writes none.
    pub run_dir: Option<RunDir>,
}

impl Default for SuperviseOptions {
    fn default() -> Self {
        Self {
            stop_timeout: DEFAULT_STOP_TIMEOUT,
            run_dir: None,
        }
    }
}

/// How a process ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExitInfo {
    /// Exit code, if it exited normally.
    pub code: Option<i32>,
    /// Signal that killed it, if any.
    pub signal: Option<i32>,
    /// The output said a port was already in use.
    pub port_in_use: bool,
    /// Last output lines, joined with `\n`.
    pub tail: String,
}

impl ExitInfo {
    fn new(status: ExitStatus, port_in_use: bool, tail: String) -> Self {
        Self {
            code: status.code(),
            signal: status.signal(),
            port_in_use,
            tail,
        }
    }

    /// Exited with code 0.
    #[must_use]
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// State of a supervised instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Status {
    /// Spawned; the HTTP port is not accepting connections yet.
    Starting {
        /// Process id (also the process group id).
        pid: u32,
    },
    /// The HTTP port accepts connections.
    Running {
        /// Process id.
        pid: u32,
    },
    /// A stop or restart was requested; waiting for the process to exit.
    Stopping {
        /// Process id.
        pid: u32,
    },
    /// Exited on request, or by itself with code 0.
    Stopped {
        /// How it ended.
        exit: ExitInfo,
    },
    /// Exited by itself with a non-zero code or a signal.
    Crashed {
        /// How it ended.
        exit: ExitInfo,
    },
    /// A restart could not spawn the process.
    FailedToStart {
        /// Why.
        error: String,
    },
}

impl Status {
    /// No process is running.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Stopped { .. } | Self::Crashed { .. } | Self::FailedToStart { .. }
        )
    }

    /// Process id, while there is a process.
    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        match self {
            Self::Starting { pid } | Self::Running { pid } | Self::Stopping { pid } => Some(*pid),
            _ => None,
        }
    }
}

/// Output stream a line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stream {
    /// Standard output.
    Stdout,
    /// Standard error (where Odoo logs by default).
    Stderr,
}

/// Something that happened to a supervised instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// A process was spawned (first start or restart).
    Spawned {
        /// Process id.
        pid: u32,
        /// HTTP port.
        http_port: u16,
        /// Gevent port.
        gevent_port: u16,
        /// Automatic ports that moved ("http port 8069 busy → using 8070").
        /// Only reported on the first start.
        changes: Vec<PortChange>,
    },
    /// One output line.
    Output {
        /// Where it came from.
        stream: Stream,
        /// The line, trimmed, at most 4 KiB.
        line: String,
    },
    /// The status changed.
    Status {
        /// New status.
        status: Status,
    },
}

type OnEvent = Box<dyn FnMut(Event) + Send>;

enum Cmd {
    Stop(oneshot::Sender<Status>),
    /// With arguments for the new process only.
    Restart(oneshot::Sender<Result<u32, String>>, Vec<OsString>),
}

/// Handle to a supervised instance. Dropping it kills the process group and
/// releases the ports; use [`Supervised::shutdown`] for a graceful end.
#[derive(Debug)]
pub struct Supervised {
    owner: PortOwner,
    http: u16,
    gevent: u16,
    cmds: mpsc::UnboundedSender<Cmd>,
    status: watch::Receiver<Status>,
    task: Option<JoinHandle<()>>,
}

/// Spawns the instance described by `spec` on the ports of `ports` and
/// supervises it in a background task. Must be called within a Tokio runtime.
///
/// `on_event` receives every [`Event`], starting with [`Event::Spawned`].
///
/// # Errors
/// A validation error from [`LaunchSpec::validate`],
/// [`SupervisorError::Spawn`] when the process cannot be started, or
/// [`SupervisorError::PidFile`] when its PID file cannot be written.
pub fn start(
    spec: LaunchSpec,
    ports: Reservation,
    opts: SuperviseOptions,
    on_event: impl FnMut(Event) + Send + 'static,
) -> Result<Supervised, SupervisorError> {
    spec.validate()?;
    let (http, gevent) = (ports.http(), ports.gevent());
    let child = spec.spawn(http, gevent, &[])?;
    let pid = child.id().unwrap_or_default();
    let me = opts.run_dir.as_ref().and_then(|_| ProcessId::current());
    let pid_file = match track(&opts, &spec, &ports, me, pid) {
        Ok(guard) => guard,
        Err(e) => {
            drop(GroupKill(proc::group_of(&child)));
            return Err(e.into());
        }
    };
    let mut on_event: OnEvent = Box::new(on_event);
    on_event(Event::Spawned {
        pid,
        http_port: http,
        gevent_port: gevent,
        changes: ports.changes().to_vec(),
    });
    let initial = Status::Starting { pid };
    on_event(Event::Status {
        status: initial.clone(),
    });
    let (status_tx, status) = watch::channel(initial);
    let (cmds, cmd_rx) = mpsc::unbounded_channel();
    let owner = ports.owner().clone();
    let task = Task {
        spec,
        opts,
        ports,
        on_event,
        status: status_tx,
        cmds: cmd_rx,
        cmds_open: true,
        me,
        pid_file,
    };
    Ok(Supervised {
        owner,
        http,
        gevent,
        cmds,
        status,
        task: Some(tokio::spawn(task.run(child))),
    })
}

impl Supervised {
    /// The instance.
    #[must_use]
    pub fn owner(&self) -> &PortOwner {
        &self.owner
    }

    /// HTTP port.
    #[must_use]
    pub fn http_port(&self) -> u16 {
        self.http
    }

    /// Gevent port.
    #[must_use]
    pub fn gevent_port(&self) -> u16 {
        self.gevent
    }

    /// Current status.
    #[must_use]
    pub fn status(&self) -> Status {
        self.status.borrow().clone()
    }

    /// A receiver that sees every status change.
    #[must_use]
    pub fn watch(&self) -> watch::Receiver<Status> {
        self.status.clone()
    }

    /// Waits until the status matches `f`, and returns it.
    ///
    /// # Errors
    /// [`SupervisorError::Gone`] if the supervisor ended first.
    pub async fn wait_for(
        &self,
        f: impl FnMut(&Status) -> bool,
    ) -> Result<Status, SupervisorError> {
        let mut rx = self.status.clone();
        rx.wait_for(f)
            .await
            .map(|s| s.clone())
            .map_err(|_| self.gone())
    }

    /// Stops the process gracefully (SIGINT, then SIGKILL after the stop
    /// timeout) and returns the final status. Already stopped: returns it.
    ///
    /// # Errors
    /// [`SupervisorError::Gone`] if the supervisor ended.
    pub async fn stop(&self) -> Result<Status, SupervisorError> {
        let (tx, rx) = oneshot::channel();
        self.cmds.send(Cmd::Stop(tx)).map_err(|_| self.gone())?;
        rx.await.map_err(|_| self.gone())
    }

    /// Stops the process (if running) and starts it again on the same ports.
    /// Returns the new process id.
    ///
    /// # Errors
    /// [`SupervisorError::Restart`] if the new process cannot be started (the
    /// status is then `failed_to_start`), or [`SupervisorError::Gone`].
    pub async fn restart(&self) -> Result<u32, SupervisorError> {
        self.restart_with(Vec::new()).await
    }

    /// Like [`Supervised::restart`], with `once` appended to the new
    /// process's arguments (see [`LaunchSpec::odoo_args_with`]). Later
    /// restarts do not repeat them. Callers build `once` themselves; it is
    /// passed as separate argv entries, never through a shell.
    ///
    /// # Errors
    /// See [`Supervised::restart`].
    pub async fn restart_with(&self, once: Vec<OsString>) -> Result<u32, SupervisorError> {
        let (tx, rx) = oneshot::channel();
        self.cmds
            .send(Cmd::Restart(tx, once))
            .map_err(|_| self.gone())?;
        rx.await
            .map_err(|_| self.gone())?
            .map_err(SupervisorError::Restart)
    }

    /// Stops the process, ends the supervisor and releases the ports.
    ///
    /// # Errors
    /// [`SupervisorError::Gone`] if the supervisor had already ended.
    pub async fn shutdown(mut self) -> Result<Status, SupervisorError> {
        let status = self.stop().await;
        if let Some(task) = self.task.take() {
            // Closing the command channel ends the idle task.
            let (closed, _) = mpsc::unbounded_channel();
            self.cmds = closed;
            let _ = task.await;
        }
        status
    }

    fn gone(&self) -> SupervisorError {
        SupervisorError::Gone(self.owner.clone())
    }
}

impl Drop for Supervised {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            // Dropping the task drops the child: its group is killed.
            task.abort();
        }
    }
}

/// What a command asked for while a process ran.
enum Requested {
    Stop(Option<oneshot::Sender<Status>>),
    Restart(oneshot::Sender<Result<u32, String>>, Vec<OsString>),
}

struct Task {
    spec: LaunchSpec,
    opts: SuperviseOptions,
    ports: Reservation,
    on_event: OnEvent,
    status: watch::Sender<Status>,
    cmds: mpsc::UnboundedReceiver<Cmd>,
    cmds_open: bool,
    /// This OdHive process, recorded in PID files.
    me: Option<ProcessId>,
    /// PID file of the current process; removed when dropped.
    pid_file: Option<PidGuard>,
}

/// Writes the PID file for `pid`, when PID files are on.
fn track(
    opts: &SuperviseOptions,
    spec: &LaunchSpec,
    ports: &Reservation,
    me: Option<ProcessId>,
    pid: u32,
) -> Result<Option<PidGuard>, PidFileError> {
    let Some(run) = &opts.run_dir else {
        return Ok(None);
    };
    let Some(process) = ProcessId::of(pid) else {
        // Exited and reaped already: nothing is left to find later.
        return Ok(None);
    };
    let owner = ports.owner();
    let record = PidRecord {
        version: RECORD_VERSION,
        project: owner.project.clone(),
        instance: owner.instance.clone(),
        process,
        http_port: ports.http(),
        gevent_port: ports.gevent(),
        db: spec.db.clone(),
        odoo_bin: spec.odoo_bin.clone(),
        supervisor: me,
    };
    PidGuard::write(run, &record).map(Some)
}

impl Task {
    async fn run(mut self, first: Child) {
        let mut child = Some(first);
        loop {
            if let Some(c) = child.take() {
                let (status, requested) = self.run_process(c).await;
                self.set(status.clone());
                match requested {
                    Some(Requested::Stop(Some(reply))) => {
                        let _ = reply.send(status);
                    }
                    Some(Requested::Restart(reply, once)) => {
                        child = self.respawn(reply, &once);
                        continue;
                    }
                    Some(Requested::Stop(None)) | None => {}
                }
            }
            if !self.cmds_open {
                return;
            }
            // Idle: no process; wait for a restart or the end.
            match self.cmds.recv().await {
                None => return,
                Some(Cmd::Stop(reply)) => {
                    let _ = reply.send(self.status.borrow().clone());
                }
                Some(Cmd::Restart(reply, once)) => child = self.respawn(reply, &once),
            }
        }
    }

    fn respawn(
        &mut self,
        reply: oneshot::Sender<Result<u32, String>>,
        once: &[OsString],
    ) -> Option<Child> {
        let (http, gevent) = (self.ports.http(), self.ports.gevent());
        match self.spec.spawn(http, gevent, once) {
            Ok(child) => {
                let pid = child.id().unwrap_or_default();
                match track(&self.opts, &self.spec, &self.ports, self.me, pid) {
                    Ok(guard) => self.pid_file = guard,
                    Err(e) => {
                        drop(GroupKill(proc::group_of(&child)));
                        let error = SupervisorError::from(e).to_string();
                        self.set(Status::FailedToStart {
                            error: error.clone(),
                        });
                        let _ = reply.send(Err(error));
                        return None;
                    }
                }
                (self.on_event)(Event::Spawned {
                    pid,
                    http_port: http,
                    gevent_port: gevent,
                    changes: Vec::new(),
                });
                self.set(Status::Starting { pid });
                let _ = reply.send(Ok(pid));
                Some(child)
            }
            Err(e) => {
                let error = e.to_string();
                self.set(Status::FailedToStart {
                    error: error.clone(),
                });
                let _ = reply.send(Err(error));
                None
            }
        }
    }

    fn set(&mut self, status: Status) {
        self.status.send_replace(status.clone());
        (self.on_event)(Event::Status { status });
    }

    /// Runs one process until it exits; returns its final status and the
    /// command that ended it, if any.
    async fn run_process(&mut self, mut child: Child) -> (Status, Option<Requested>) {
        let pid = child.id().unwrap_or_default();
        let mut group = GroupKill(proc::group_of(&child));
        let (tx, mut lines) = mpsc::unbounded_channel();
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");
        tokio::spawn(proc::forward_lines_with(stdout, tx.clone(), |l| {
            (Stream::Stdout, l)
        }));
        tokio::spawn(proc::forward_lines_with(stderr, tx, |l| {
            (Stream::Stderr, l)
        }));

        let http = SocketAddr::from((Ipv4Addr::LOCALHOST, self.ports.http()));
        let mut ready_tick = tokio::time::interval(READY_POLL);
        ready_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut ready = false;
        let mut requested: Option<Requested> = None;
        let mut kill_at: Option<Instant> = None;
        let mut lines_open = true;
        let mut tail = Tail::default();

        let exit = loop {
            tokio::select! {
                line = lines.recv(), if lines_open => match line {
                    Some(line) => self.output(line, &mut tail),
                    None => lines_open = false,
                },
                status = child.wait() => break status,
                cmd = self.cmds.recv(), if requested.is_none() && self.cmds_open => {
                    requested = Some(match cmd {
                        Some(Cmd::Stop(reply)) => Requested::Stop(Some(reply)),
                        Some(Cmd::Restart(reply, once)) => Requested::Restart(reply, once),
                        None => {
                            self.cmds_open = false;
                            Requested::Stop(None)
                        }
                    });
                    signal_group(&group, Signal::INT);
                    kill_at = Some(Instant::now() + self.opts.stop_timeout);
                    self.set(Status::Stopping { pid });
                },
                () = sleep_until(kill_at), if kill_at.is_some() => {
                    signal_group(&group, Signal::KILL);
                    kill_at = None;
                },
                _ = ready_tick.tick(), if !ready && requested.is_none() => {
                    if accepts(http).await {
                        ready = true;
                        self.set(Status::Running { pid });
                    }
                },
            }
        };
        // The leader is reaped, but its pid number cannot be handed out again
        // while group members still use it as their group id, and pids are
        // allocated sequentially, so killing the group right now only hits
        // what is left of it (workers, report renderers).
        signal_group(&group, Signal::KILL);
        group.0 = None;
        // Nothing of the process is left: forget its PID file.
        self.pid_file = None;
        // Leftovers are dead, so the pipes close; read what is still in them.
        if lines_open {
            let drain = async {
                while let Some(line) = lines.recv().await {
                    self.output(line, &mut tail);
                }
            };
            let _ = tokio::time::timeout(DRAIN_TIMEOUT, drain).await;
        }
        let info = match exit {
            Ok(status) => ExitInfo::new(status, tail.port_in_use, tail.joined()),
            Err(e) => ExitInfo {
                code: None,
                signal: None,
                port_in_use: tail.port_in_use,
                tail: format!("{}\nwaiting for the process failed: {e}", tail.joined()),
            },
        };
        let status = if requested.is_some() || info.success() {
            Status::Stopped { exit: info }
        } else {
            Status::Crashed { exit: info }
        };
        (status, requested)
    }

    fn output(&mut self, (stream, line): (Stream, String), tail: &mut Tail) {
        tail.push(&line);
        (self.on_event)(Event::Output { stream, line });
    }
}

#[derive(Default)]
struct Tail {
    lines: VecDeque<String>,
    port_in_use: bool,
}

impl Tail {
    fn push(&mut self, line: &str) {
        if is_port_in_use(line) {
            self.port_in_use = true;
        }
        if self.lines.len() == TAIL_LINES {
            self.lines.pop_front();
        }
        self.lines.push_back(line.to_owned());
    }

    fn joined(&self) -> String {
        Vec::from(self.lines.clone()).join("\n")
    }
}

/// Whether an output line reports a failed bind ("[Errno 98] Address already
/// in use" on Linux, errno 48 on macOS).
fn is_port_in_use(line: &str) -> bool {
    line.contains("Address already in use")
}

fn signal_group(group: &GroupKill, signal: Signal) {
    if let Some(pid) = group.0 {
        // ESRCH (already gone) is fine.
        let _ = kill_process_group(pid, signal);
    }
}

async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

async fn accepts(addr: SocketAddr) -> bool {
    match tokio::time::timeout(READY_CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
        Ok(Ok(stream)) => !connected_to_itself(&stream),
        _ => false,
    }
}

/// A connect to a loopback port in the ephemeral range can get that very
/// port as its local port and connect to itself (TCP simultaneous open).
/// Nothing listens then, and while the socket lives the instance cannot bind
/// the port; it is closed right away.
fn connected_to_itself(stream: &TcpStream) -> bool {
    matches!((stream.local_addr(), stream.peer_addr()), (Ok(l), Ok(p)) if l == p)
}
