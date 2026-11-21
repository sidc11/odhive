//! `odhive start`, `restart` and `stop`.
//!
//! Until the background service exists (Milestone 5), `start` runs in the
//! foreground: it supervises the instances, prints their output, writes it to
//! `<project>/logs/<instance>.log`, and stops them on Ctrl+C. `stop` works
//! from any terminal through the PID files: it interrupts the recorded
//! process, and the supervising `start` sees it exit.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, bail};
use odhive_core::launch::{self, LoadedProject, Target};
use odhive_core::logs::{self, LogFile};
use odhive_core::manager::{
    InstanceEvent, Manager, ManagerEvent, ManagerOptions, Notice, Recovery,
};
use odhive_core::pidfile::{self, RecordState};
use odhive_core::ports::PortOwner;
use odhive_core::schema::SchemaOrigin;
use odhive_core::supervisor::{DEFAULT_STOP_TIMEOUT, Event, ExitInfo, Status};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinSet;

use super::{Ctx, say};

/// How often the run loop re-checks whether every instance has ended.
const CHECK_EVERY: Duration = Duration::from_millis(250);

pub async fn start(ctx: &Ctx, target: &Target, quiet: bool) -> anyhow::Result<ExitCode> {
    // Installed before anything is spawned: a Ctrl+C during startup is then
    // handled by the loop below instead of killing OdHive and orphaning Odoo.
    let mut signals = Signals::new()?;
    let project = launch::load_project(&ctx.cfg, &target.project)?;
    let selected = project.select(target)?;
    let run = ctx.run_dir();
    let manager = Manager::new(ManagerOptions {
        run_dir: Some(run.clone()),
        ..ManagerOptions::default()
    });

    let mut attached = HashSet::new();
    let mut elsewhere = Vec::new();
    for r in manager.recover()? {
        match r {
            Recovery::Attached { owner, .. } => {
                attached.insert(owner);
            }
            Recovery::SupervisedElsewhere { owner, pid, by } => {
                if target.matches(&owner) {
                    bail!(
                        "{owner} is already running (pid {pid}, in odhive process {by}); \
                         use `odhive restart {target}` or `odhive stop {target}`"
                    );
                }
                elsewhere.push(owner);
            }
            Recovery::Stale { owner, pid, .. } => {
                tracing::info!(instance = %owner, pid, "removed stale pid file");
            }
            Recovery::Invalid { path, reason } => {
                say(format!(
                    "warning: removed unusable {}: {reason}",
                    path.display()
                ));
            }
        }
    }
    // Instances that other OdHive processes run keep their ports and databases.
    let mut foreign_ports = Vec::new();
    let mut foreign_dbs = BTreeMap::new();
    for owner in elsewhere {
        if let Ok(Some(rec)) = run.read(&owner) {
            if let Ok(r) = manager
                .registry()
                .reserve(owner.clone(), rec.http_port, rec.gevent_port)
            {
                foreign_ports.push(r);
            }
            if let Some(db) = rec.db {
                foreign_dbs.insert(db, owner);
            }
        }
    }

    let prepared = launch::prepare(&ctx.cfg, &ctx.paths, &project, &selected).await?;
    for p in &prepared.skipped_addons {
        say(format!(
            "note: {} has no modules yet; left out of addons_path",
            p.display()
        ));
    }
    if let SchemaOrigin::Bundled { reason } = &prepared.schema_origin {
        tracing::info!(%reason, "using the bundled Odoo option list");
    }
    for plan in &prepared.plans {
        if let Some(db) = &plan.spec.db
            && let Some(by) = foreign_dbs.get(db)
        {
            bail!("database {db:?} is already used by {by}");
        }
    }

    let mut rx = manager.subscribe();
    let mut out = Output::new(&project, quiet, prepared.plans.len() > 1);
    let mut ours = Vec::new();
    for plan in prepared.plans {
        let owner = plan.owner.clone();
        let started = if attached.contains(&owner) {
            say(format!(
                "{owner}: left running by an earlier OdHive; restarting it here to capture its output"
            ));
            manager.replace(plan).await
        } else {
            manager.start(plan).await
        };
        if let Err(e) = started {
            stop_all(&manager, &ours).await;
            return Err(e).with_context(|| format!("could not start {owner}"));
        }
        ours.push(owner);
    }

    let mut stopper = None;
    let mut forced = false;
    loop {
        if rx.is_empty() && all_done(&manager, &ours) {
            break;
        }
        tokio::select! {
            () = signals.next() => {
                if stopper.is_some() {
                    say("forcing stop");
                    forced = true;
                    break;
                }
                say("stopping… (press Ctrl+C again to force)");
                let (m, o) = (manager.clone(), ours.clone());
                stopper = Some(tokio::spawn(async move { stop_all(&m, &o).await }));
            }
            ev = rx.recv() => match ev {
                Ok(ev) => out.event(&manager, &ev),
                Err(RecvError::Lagged(n)) => {
                    say(format!("warning: {n} events were dropped; the log file misses them too"));
                }
                Err(RecvError::Closed) => break,
            },
            () = tokio::time::sleep(CHECK_EVERY) => {}
        }
    }
    if forced {
        // Dropping the manager kills the process groups.
        drop(manager);
        return Ok(ExitCode::FAILURE);
    }
    while let Ok(ev) = rx.try_recv() {
        out.event(&manager, &ev);
    }
    if let Some(s) = stopper {
        let _ = s.await;
    }
    // Ended instances stay listed (ports held) until stopped.
    stop_all(&manager, &ours).await;
    drop(foreign_ports);
    Ok(if out.failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

pub async fn restart(ctx: &Ctx, target: &Target, quiet: bool) -> anyhow::Result<ExitCode> {
    stop_running(ctx, target).await?;
    start(ctx, target, quiet).await
}

pub async fn stop(ctx: &Ctx, target: &Target) -> anyhow::Result<ExitCode> {
    if stop_running(ctx, target).await? == 0 {
        say(format!("{target}: nothing is running"));
    }
    Ok(ExitCode::SUCCESS)
}

/// Stops every running instance `target` selects; returns how many.
async fn stop_running(ctx: &Ctx, target: &Target) -> anyhow::Result<usize> {
    let project = launch::load_project(&ctx.cfg, &target.project)?;
    let run = ctx.run_dir();
    let mut jobs = JoinSet::new();
    let mut count = 0;
    for state in launch::states(&run, &project, target)? {
        let Some(record) = state.record else {
            continue;
        };
        let running = matches!(state.run, Some(RecordState::Running { .. }));
        if running {
            say(format!("stopping {}…", state.owner));
            count += 1;
        }
        let run = run.clone();
        jobs.spawn(async move {
            let result = pidfile::stop_recorded(&run, &record, DEFAULT_STOP_TIMEOUT).await;
            (state.owner, running, result)
        });
    }
    let mut failed = false;
    while let Some(job) = jobs.join_next().await {
        let (owner, running, result) = job.context("stop task failed")?;
        match result {
            Ok(s) if running && s.killed => say(format!(
                "{owner}: killed (did not stop within {}s)",
                DEFAULT_STOP_TIMEOUT.as_secs()
            )),
            Ok(_) if running => say(format!("{owner}: stopped")),
            Ok(_) => {}
            Err(e) => {
                say(format!("{owner}: {e}"));
                failed = true;
            }
        }
    }
    if failed {
        bail!("some instances could not be stopped");
    }
    Ok(count)
}

/// SIGINT (Ctrl+C), SIGTERM and SIGHUP (terminal closed) all mean "stop".
struct Signals {
    int: Signal,
    term: Signal,
    hup: Signal,
}

impl Signals {
    fn new() -> anyhow::Result<Self> {
        Ok(Self {
            int: signal(SignalKind::interrupt())?,
            term: signal(SignalKind::terminate())?,
            hup: signal(SignalKind::hangup())?,
        })
    }

    async fn next(&mut self) {
        tokio::select! {
            _ = self.int.recv() => {}
            _ = self.term.recv() => {}
            _ = self.hup.recv() => {}
        }
    }
}

async fn stop_all(manager: &Manager, owners: &[PortOwner]) {
    let mut jobs = JoinSet::new();
    for o in owners {
        let (m, o) = (manager.clone(), o.clone());
        jobs.spawn(async move { m.stop(&o).await });
    }
    while jobs.join_next().await.is_some() {}
}

fn all_done(manager: &Manager, owners: &[PortOwner]) -> bool {
    owners
        .iter()
        .all(|o| manager.get(o).is_none_or(|i| i.status.is_terminal()))
}

/// Prints events and writes output lines to the log files.
struct Output {
    logs_dir: std::path::PathBuf,
    quiet: bool,
    prefix: bool,
    files: HashMap<PortOwner, Option<LogFile>>,
    failed: bool,
}

impl Output {
    fn new(project: &LoadedProject, quiet: bool, prefix: bool) -> Self {
        Self {
            logs_dir: project.logs_dir(),
            quiet,
            prefix,
            files: HashMap::new(),
            failed: false,
        }
    }

    fn log(&mut self, owner: &PortOwner, line: &str) {
        let file = self.files.entry(owner.clone()).or_insert_with(|| {
            let opened = logs::log_path(&self.logs_dir, &owner.instance)
                .and_then(|p| LogFile::open(&p, logs::DEFAULT_MAX_BYTES));
            match opened {
                Ok(f) => Some(f),
                Err(e) => {
                    say(format!("warning: {owner}: cannot write its log file: {e}"));
                    None
                }
            }
        });
        if let Some(f) = file
            && let Err(e) = f.write_line(line)
        {
            say(format!("warning: {owner}: log file write failed: {e}"));
            *file = None;
        }
    }

    fn event(&mut self, manager: &Manager, ev: &ManagerEvent) {
        let owner = &ev.owner;
        match &ev.event {
            InstanceEvent::Supervisor(Event::Spawned {
                pid,
                http_port,
                changes,
                ..
            }) => {
                for c in changes {
                    say(format!("{owner}: {c}"));
                }
                say(format!(
                    "{owner}: starting (pid {pid}) on http://localhost:{http_port}"
                ));
                self.log(owner, &format!("----- odhive: started pid {pid} -----"));
            }
            InstanceEvent::Supervisor(Event::Output { line, .. }) => {
                self.log(owner, line);
                if !self.quiet {
                    if self.prefix {
                        say(format!("[{}] {line}", owner.instance));
                    } else {
                        say(line);
                    }
                }
            }
            InstanceEvent::Supervisor(Event::Status { status }) => match status {
                Status::Running { .. } => {
                    let port = manager.get(owner).and_then(|i| i.http_port);
                    say(match port {
                        Some(p) => format!("{owner}: running at http://localhost:{p}"),
                        None => format!("{owner}: running"),
                    });
                }
                Status::Stopped { .. } => say(format!("{owner}: stopped")),
                Status::Crashed { exit } => {
                    self.failed = true;
                    say(format!("{owner}: crashed ({})", describe(exit)));
                    if exit.port_in_use {
                        say(
                            "hint: its port is in use by another program; free it or change http_port in odhive.toml",
                        );
                    }
                }
                Status::FailedToStart { error } => {
                    self.failed = true;
                    say(format!("{owner}: failed to start: {error}"));
                }
                Status::Starting { .. } | Status::Stopping { .. } => {}
            },
            InstanceEvent::Manager(Notice::PortRetry { http_port, .. }) => {
                say(format!(
                    "{owner}: port {http_port} was taken at the last moment; retrying on other ports"
                ));
            }
            InstanceEvent::Manager(Notice::Attached { .. }) => {}
        }
    }
}

fn describe(exit: &ExitInfo) -> String {
    match (exit.code, exit.signal) {
        (Some(code), _) => format!("exit code {code}"),
        (None, Some(sig)) => format!("killed by signal {sig}"),
        (None, None) => "unknown exit".into(),
    }
}
