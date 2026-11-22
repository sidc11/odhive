//! `odhive start`, `restart` and `stop`.
//!
//! Until the background service exists (Milestone 5), `start` runs in the
//! foreground: it supervises the instances, prints their log entries (the
//! manager writes them to `<project>/logs/<instance>.log`), and stops them on
//! Ctrl+C. `stop` works
//! from any terminal through the PID files: it interrupts the recorded
//! process, and the supervising `start` sees it exit.
//!
//! A missing Python module is handled by the manager's auto-fix; `start`
//! prints what it does and, in `ask` mode or for an untrusted package, asks
//! on the terminal (without a terminal nothing is installed).
//!
//! Smart Reload (the instance's `reload_mode`) restarts instances when their
//! custom modules change; a crashed instance then waits for the fix instead
//! of ending the run.

use std::collections::{BTreeMap, HashSet};
use std::io::{BufRead, IsTerminal, Write};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, bail};
use odhive_core::autofix::Trust;
use odhive_core::events::{Delivery, LogEvent};
use odhive_core::launch::{self, Target};
use odhive_core::manager::{
    FixAction, InstanceEvent, Manager, ManagerEvent, ManagerOptions, Notice, Recovery,
};
use odhive_core::pidfile::{self, RecordState};
use odhive_core::ports::PortOwner;
use odhive_core::reload::Action;
use odhive_core::requirements::Origin;
use odhive_core::schema::SchemaOrigin;
use odhive_core::supervisor::{DEFAULT_STOP_TIMEOUT, Event, ExitInfo, Status};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::mpsc;
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
    if let Some(e) = &prepared.trusted_error {
        say(format!(
            "warning: requirements could not be read ({e}); only OdHive's built-in package table is trusted for missing modules"
        ));
    }
    for plan in &prepared.plans {
        if let Some(db) = &plan.spec.db
            && let Some(by) = foreign_dbs.get(db)
        {
            bail!("database {db:?} is already used by {by}");
        }
    }

    let mut rx = manager.subscribe();
    let mut log_rx = manager.subscribe_logs();
    let mut out = Output::new(quiet, prepared.plans.len() > 1);
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
    let mut prompt = Prompt::new();
    let mut fixes = JoinSet::new();
    loop {
        let waiting = stopper.is_none() && (prompt.open > 0 || !fixes.is_empty());
        if rx.is_empty() && log_rx.is_empty() && !waiting && all_done(&manager, &ours) {
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
                Some(Delivery::Event(ev)) => {
                    out.event(&manager, &ev);
                    if stopper.is_none()
                        && let InstanceEvent::Manager(Notice::DependencyMissing {
                            module,
                            package: Some(package),
                            action: FixAction::Confirm,
                            ..
                        }) = &ev.event
                    {
                        prompt.ask(&ev.owner, module, package);
                    }
                }
                Some(Delivery::Lagged { missed }) => {
                    say(format!("warning: {missed} instance events were not shown"));
                }
                None => break,
            },
            ev = log_rx.recv() => match ev {
                Some(Delivery::Event(ev)) => out.log(&ev),
                Some(Delivery::Lagged { missed }) => {
                    say(format!("warning: {missed} log entries were not shown; the log file has them"));
                }
                None => break,
            },
            Some((owner, yes)) = prompt.answers.recv() => {
                prompt.open -= 1;
                if stopper.is_some() {
                    continue;
                }
                if yes {
                    let m = manager.clone();
                    fixes.spawn(async move { (owner.clone(), m.confirm_fix(&owner).await) });
                } else if let Ok(c) = manager.decline_fix(&owner) {
                    say(format!("{owner}: not installing {}", c.package));
                }
            }
            Some(done) = fixes.join_next(), if !fixes.is_empty() => {
                if let Ok((owner, Err(e))) = done {
                    say(format!("{owner}: {e}"));
                }
            }
            () = tokio::time::sleep(CHECK_EVERY) => {}
        }
    }
    if forced {
        // Dropping the manager kills the process groups.
        drop(manager);
        return Ok(ExitCode::FAILURE);
    }
    for ev in log_rx.drain() {
        out.log(&ev);
    }
    for ev in rx.drain() {
        out.event(&manager, &ev);
    }
    if let Some(s) = stopper {
        let _ = s.await;
    }
    // Ended instances stay listed (ports held) until stopped.
    stop_all(&manager, &ours).await;
    drop(foreign_ports);
    Ok(if out.failed() {
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
        .all(|o| manager.get(o).is_none_or(|i| i.is_done()))
}

/// Asks the terminal whether to install packages, one question at a time.
/// The reading thread is detached: an unanswered question never keeps OdHive
/// from exiting.
struct Prompt {
    questions: Option<std::sync::mpsc::Sender<(PortOwner, String)>>,
    answers: mpsc::UnboundedReceiver<(PortOwner, bool)>,
    tx: mpsc::UnboundedSender<(PortOwner, bool)>,
    /// Questions not answered yet.
    open: usize,
}

impl Prompt {
    fn new() -> Self {
        let (tx, answers) = mpsc::unbounded_channel();
        Self {
            questions: None,
            answers,
            tx,
            open: 0,
        }
    }

    fn ask(&mut self, owner: &PortOwner, module: &str, package: &str) {
        if !std::io::stdin().is_terminal() {
            say(format!(
                "{owner}: not installing {package} without confirmation; run `odhive start` in a terminal to be asked, or install it into the project's .venv yourself"
            ));
            return;
        }
        let questions = self.questions.get_or_insert_with(|| {
            let (q_tx, q_rx) = std::sync::mpsc::channel::<(PortOwner, String)>();
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                let stdin = std::io::stdin();
                for (owner, question) in q_rx {
                    {
                        let mut out = std::io::stdout().lock();
                        let _ = write!(out, "{question} [y/N] ");
                        let _ = out.flush();
                    }
                    let mut line = String::new();
                    let yes = stdin.lock().read_line(&mut line).is_ok()
                        && matches!(line.trim(), "y" | "Y" | "yes");
                    if tx.send((owner, yes)).is_err() {
                        return;
                    }
                }
            });
            q_tx
        });
        let question =
            format!("{owner}: install {package} (for module {module:?}) into the project's venv?");
        if questions.send((owner.clone(), question)).is_ok() {
            self.open += 1;
        }
    }
}

/// Prints instance events and log entries.
struct Output {
    quiet: bool,
    prefix: bool,
    /// Instances whose last outcome is a crash or a failed start.
    failed: HashSet<PortOwner>,
}

impl Output {
    fn new(quiet: bool, prefix: bool) -> Self {
        Self {
            quiet,
            prefix,
            failed: HashSet::new(),
        }
    }

    fn failed(&self) -> bool {
        !self.failed.is_empty()
    }

    fn log(&self, ev: &LogEvent) {
        if self.quiet {
            return;
        }
        let text = ev.record.entry.to_string();
        if self.prefix {
            for line in text.lines() {
                say(format!("[{}] {line}", ev.owner.instance));
            }
        } else {
            say(text);
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
            }
            // The manager turns output into log entries.
            InstanceEvent::Supervisor(Event::Output { .. })
            | InstanceEvent::Manager(Notice::Attached { .. }) => {}
            InstanceEvent::Supervisor(Event::Status { status }) => match status {
                Status::Running { .. } => {
                    // Recovered, e.g. after a missing dependency was installed.
                    self.failed.remove(owner);
                    let port = manager.get(owner).and_then(|i| i.http_port);
                    say(match port {
                        Some(p) => format!("{owner}: running at http://localhost:{p}"),
                        None => format!("{owner}: running"),
                    });
                }
                Status::Stopped { .. } => say(format!("{owner}: stopped")),
                Status::Crashed { exit } => {
                    self.failed.insert(owner.clone());
                    say(format!("{owner}: crashed ({})", describe(exit)));
                    if exit.port_in_use {
                        say(
                            "hint: its port is in use by another program; free it or change http_port in odhive.toml",
                        );
                    }
                    if reloading(manager, owner) {
                        say(format!(
                            "{owner}: waiting for a file change to restart it (Ctrl+C to quit)"
                        ));
                    }
                }
                Status::FailedToStart { error } => {
                    self.failed.insert(owner.clone());
                    say(format!("{owner}: failed to start: {error}"));
                }
                Status::Starting { .. } | Status::Stopping { .. } => {}
            },
            InstanceEvent::Manager(Notice::PortRetry { http_port, .. }) => {
                say(format!(
                    "{owner}: port {http_port} was taken at the last moment; retrying on other ports"
                ));
            }
            InstanceEvent::Manager(Notice::LogFile { error }) => {
                say(format!(
                    "warning: {owner}: log file failed ({error}); its log is kept in memory only"
                ));
            }
            InstanceEvent::Manager(Notice::DependencyMissing {
                module,
                package,
                trust,
                action,
            }) => {
                let pkg = package.as_deref().unwrap_or("?");
                say(match action {
                    FixAction::Install => format!(
                        "{owner}: missing Python module {module:?}; installing {pkg} ({})",
                        trust.as_ref().map_or_else(String::new, trust_text)
                    ),
                    FixAction::Confirm => match trust {
                        Some(t) => format!(
                            "{owner}: missing Python module {module:?}; {pkg} ({}) waits for confirmation (autofix_mode = \"ask\")",
                            trust_text(t)
                        ),
                        None => format!(
                            "{owner}: missing Python module {module:?}; {pkg} is not in the project's requirements or OdHive's package table, so it is not installed without confirmation"
                        ),
                    },
                    FixAction::Report => format!(
                        "{owner}: missing Python module {module:?} (package {pkg}); autofix_mode is \"off\", install it into the project's .venv"
                    ),
                    FixAction::GiveUp { reason } => {
                        format!("{owner}: missing Python module {module:?}: {reason}")
                    }
                });
            }
            InstanceEvent::Manager(Notice::DependencyInstalling {
                package,
                attempt,
                max,
                ..
            }) => say(format!(
                "{owner}: installing {package} (attempt {attempt}/{max})…"
            )),
            InstanceEvent::Manager(Notice::DependencyInstalled { package }) => {
                say(format!("{owner}: installed {package}; restarting"));
            }
            // The error ends with the system-library hint, when there is one.
            InstanceEvent::Manager(Notice::DependencyFailed { package, error, .. }) => {
                say(format!("{owner}: installing {package} failed: {error}"));
            }
            InstanceEvent::Manager(Notice::SystemLibraryMissing { hint }) => {
                say(format!("{owner}: hint: {hint}"));
            }
            InstanceEvent::Manager(Notice::ReloadWatching {
                roots,
                modules,
                unwatched,
            }) => {
                let roots: Vec<String> = roots.iter().map(|r| r.display().to_string()).collect();
                say(format!(
                    "{owner}: smart reload watching {} ({modules} modules)",
                    roots.join(", ")
                ));
                for u in unwatched {
                    say(format!(
                        "note: {} links outside its addons folder and is not watched; add its real folder to addons_paths",
                        u.display()
                    ));
                }
            }
            InstanceEvent::Manager(Notice::ReloadFailed { error }) => {
                say(format!("warning: {owner}: smart reload is off: {error}"));
            }
            InstanceEvent::Manager(Notice::Reload {
                action,
                files,
                file_count,
            }) => {
                let changed = match (files.first(), file_count) {
                    (Some(f), 1) => f.display().to_string(),
                    _ => format!("{file_count} files"),
                };
                say(match action {
                    Action::Restart => format!("{owner}: {changed} changed; restarting"),
                    Action::Update { modules } => format!(
                        "{owner}: {changed} changed; restarting with -u {}",
                        modules.join(",")
                    ),
                    Action::RefreshBrowser { modules } => format!(
                        "{owner}: static files of {} changed; refresh the browser",
                        modules.join(", ")
                    ),
                });
            }
            InstanceEvent::Manager(Notice::ModuleAdded { module }) => {
                say(format!(
                    "{owner}: new module {module}; install it from Apps (after Update Apps List) or with -i {module}"
                ));
            }
        }
    }
}

fn reloading(manager: &Manager, owner: &PortOwner) -> bool {
    manager
        .get(owner)
        .and_then(|i| i.reload)
        .is_some_and(|r| r.watching)
}

fn trust_text(trust: &Trust) -> String {
    match trust {
        Trust::Table => "from OdHive's package table".into(),
        Trust::Listed {
            origin: Origin::File { path, line },
        } => format!("listed in {}:{line}", path.display()),
        Trust::Listed {
            origin: Origin::Manifest { module, .. },
        } => format!("required by module {module}"),
        Trust::Listed {
            origin: Origin::Missing { .. },
        } => "requested".into(),
    }
}

fn describe(exit: &ExitInfo) -> String {
    match (exit.code, exit.signal) {
        (Some(code), _) => format!("exit code {code}"),
        (None, Some(sig)) => format!("killed by signal {sig}"),
        (None, None) => "unknown exit".into(),
    }
}
