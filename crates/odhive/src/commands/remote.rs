//! Commands while the OdHive service runs: they go through its API, like the
//! web UI, instead of touching the workspace themselves.
//!
//! `start` and `restart` hand the instances to the service and then show
//! their output and events (from the WebSocket) until they end; Ctrl+C only
//! stops watching, the instances keep running in the service. `logs` reads
//! the service's log buffers, and falls back to the log files for instances
//! the service doesn't run.

use std::collections::HashMap;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, bail};
use futures_util::{SinkExt, StreamExt};
use hyper::Method;
use odhive_core::autofix::Candidate;
use odhive_core::config::SourceEntry;
use odhive_core::launch::Target;
use odhive_core::manager::{FixAction, InstanceEvent, InstanceInfo, Notice};
use odhive_core::ports::PortOwner;
use odhive_core::supervisor::{DEFAULT_STOP_TIMEOUT, Event, Status};
use odhive_core::version::OdooVersion;
use odhive_server::api::instances::{LogPage, MAX_LOG_LIMIT, StopResult};
use odhive_server::api::projects::{InstanceView, Project};
use odhive_server::api::ws::ServerMessage;
use odhive_server::jobs::{Job, JobState, JobStep};
use serde_json::json;
use tokio::net::TcpStream;
use tokio::task::JoinSet;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

use super::project::print_setup_event;
use super::run::{Output, Prompt, Signals};
use super::source::{edition, print_clone_event};
use super::{say, table};
use crate::cli::{Command, EnvArgs, ProjectAddArgs, ProjectCommand, SourceCommand};
use crate::client::{API, Client, REQUEST_TIMEOUT, SLOW_TIMEOUT};

/// How often job progress and instance states are read.
const POLL: Duration = Duration::from_millis(250);

/// Runs `command` through the service. `None` = it has to run locally.
pub async fn dispatch(c: &Client, command: &Command) -> anyhow::Result<Option<ExitCode>> {
    let code = match command {
        Command::Project(ProjectCommand::Add(args)) => project_add(c, args).await?,
        Command::Project(ProjectCommand::Setup(args)) => {
            setup(c, &args.name, &args.env, args.recreate).await?;
            say(format!("ready: `odhive start {}`", args.name));
            ExitCode::SUCCESS
        }
        Command::Project(ProjectCommand::List) => project_list(c).await?,
        Command::Project(ProjectCommand::Remove { name }) => project_remove(c, name).await?,
        Command::Source(SourceCommand::Add {
            path,
            enterprise,
            odoo_version,
        }) => {
            let entry = add_source(c, path, enterprise.then_some(*odoo_version).flatten()).await?;
            print_source(&entry);
            ExitCode::SUCCESS
        }
        Command::Source(SourceCommand::Clone { version }) => source_clone(c, *version).await?,
        Command::Source(SourceCommand::List) => source_list(c).await?,
        Command::Source(SourceCommand::Remove { id }) => source_remove(c, id).await?,
        Command::Start(args) => run(c, &args.target.target, args.quiet, false).await?,
        Command::Restart(args) => run(c, &args.target.target, args.quiet, true).await?,
        Command::Stop(args) => stop(c, &args.target).await?,
        Command::Status(args) => status(c, args.target.as_ref()).await?,
        Command::Logs(args) => {
            return logs(c, &args.target.target, args.follow, args.lines).await;
        }
        Command::Serve(_) | Command::Quit | Command::Service(_) => {
            unreachable!("handled by the service module")
        }
    };
    Ok(Some(code))
}

async fn post_json(
    c: &Client,
    path: &str,
    body: &serde_json::Value,
) -> anyhow::Result<crate::client::Reply> {
    c.call(Method::POST, path, Some(body), REQUEST_TIMEOUT)
        .await
}

// ---------------------------------------------------------------- sources

async fn sources(c: &Client) -> anyhow::Result<Vec<SourceEntry>> {
    c.get(&format!("{API}/sources")).await?.json()
}

fn utf8_absolute(path: &Path, what: &str) -> anyhow::Result<String> {
    let abs = std::path::absolute(path).with_context(|| format!("invalid {what} path"))?;
    abs.to_str()
        .map(str::to_owned)
        .with_context(|| format!("{what} path {} is not UTF-8", abs.display()))
}

async fn add_source(
    c: &Client,
    path: &Path,
    enterprise: Option<OdooVersion>,
) -> anyhow::Result<SourceEntry> {
    let body = json!({
        "path": utf8_absolute(path, "source")?,
        "enterprise_version": enterprise.map(|v| v.to_string()),
    });
    post_json(c, &format!("{API}/sources"), &body).await?.json()
}

fn print_source(entry: &SourceEntry) {
    say(format!(
        "source {} (Odoo {} {}) at {}",
        entry.id,
        entry.odoo_version,
        edition(entry.edition),
        entry.path.display()
    ));
}

async fn source_clone(c: &Client, version: OdooVersion) -> anyhow::Result<ExitCode> {
    let job: Job = post_json(
        c,
        &format!("{API}/sources/clone"),
        &json!({ "version": version.to_string() }),
    )
    .await?
    .json()?;
    let job = follow_job(c, job).await?;
    if let Some(odhive_server::jobs::JobResult::Clone { source }) = job.result {
        say(format!("source {} at {}", source.id, source.path.display()));
    }
    Ok(ExitCode::SUCCESS)
}

async fn source_list(c: &Client) -> anyhow::Result<ExitCode> {
    let list = sources(c).await?;
    if list.is_empty() {
        say("no sources; add one with `odhive source add <path>` or `odhive source clone 19.0`");
        return Ok(ExitCode::SUCCESS);
    }
    let rows: Vec<Vec<String>> = list
        .iter()
        .map(|s| {
            vec![
                s.id.clone(),
                s.odoo_version.to_string(),
                edition(s.edition).into(),
                s.path.display().to_string(),
            ]
        })
        .collect();
    table(&["ID", "ODOO", "EDITION", "PATH"], &rows);
    Ok(ExitCode::SUCCESS)
}

async fn source_remove(c: &Client, id: &str) -> anyhow::Result<ExitCode> {
    // Looked up first: the id goes into the URL only once it is known.
    let Some(entry) = sources(c).await?.into_iter().find(|s| s.id == id) else {
        bail!("no source with id {id:?}");
    };
    c.request(Method::DELETE, &format!("{API}/sources/{}", entry.id))
        .await?
        .check()?;
    say(format!(
        "removed source {}; files kept at {}",
        entry.id,
        entry.path.display()
    ));
    Ok(ExitCode::SUCCESS)
}

// --------------------------------------------------------------- projects

/// A registered source id, or a path registered on the way.
async fn source_id(
    c: &Client,
    known: &[SourceEntry],
    arg: &str,
    enterprise_of: Option<OdooVersion>,
) -> anyhow::Result<SourceEntry> {
    if let Some(s) = known.iter().find(|s| s.id == arg) {
        return Ok(s.clone());
    }
    let path = Path::new(arg);
    if !path.is_dir() {
        bail!("{arg:?} is neither a registered source id nor a folder");
    }
    add_source(c, path, enterprise_of).await
}

async fn project_add(c: &Client, args: &ProjectAddArgs) -> anyhow::Result<ExitCode> {
    let known = sources(c).await?;
    let community = source_id(c, &known, &args.source, None).await?;
    let enterprise = match &args.enterprise {
        Some(e) => Some(source_id(c, &known, e, Some(community.odoo_version)).await?),
        None => None,
    };
    let addons = args
        .addons
        .iter()
        .map(|a| utf8_absolute(a, "addons"))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let python = args
        .env
        .python
        .as_deref()
        .map(|p| utf8_absolute(p, "--python"))
        .transpose()?;
    let body = json!({
        "name": args.name,
        "community": community.id,
        "enterprise": enterprise.map(|e| e.id),
        "addons_paths": addons,
        "python": python,
    });
    let project: Project = post_json(c, &format!("{API}/projects"), &body)
        .await?
        .json()?;
    say(format!(
        "created project {} (Odoo {}) at {}",
        project.name, community.odoo_version, project.path
    ));
    if args.no_setup {
        say(format!(
            "next: `odhive project setup {}`, then `odhive start {}`",
            args.name, args.name
        ));
        return Ok(ExitCode::SUCCESS);
    }
    // The interpreter is part of the project now.
    let env = EnvArgs {
        python: None,
        no_requirements: args.env.no_requirements,
    };
    setup(c, &args.name, &env, false).await?;
    say(format!("ready: `odhive start {}`", args.name));
    Ok(ExitCode::SUCCESS)
}

async fn setup(c: &Client, name: &str, env: &EnvArgs, recreate: bool) -> anyhow::Result<()> {
    let python = env
        .python
        .as_deref()
        .map(|p| utf8_absolute(p, "--python"))
        .transpose()?;
    let body = json!({
        "python": python,
        "recreate": recreate,
        "skip_requirements": env.no_requirements,
    });
    let failed = || {
        format!("setting up {name} failed; fix the problem and run `odhive project setup {name}`")
    };
    let job: Job = post_json(c, &format!("{API}/projects/{name}/setup"), &body)
        .await?
        .json()
        .with_context(failed)?;
    follow_job(c, job).await.with_context(failed)?;
    Ok(())
}

fn print_create_event(e: odhive_core::db::CreateEvent) {
    use odhive_core::db::CreateEvent;
    match e {
        CreateEvent::Step { step } => say(format!("{step}…")),
        CreateEvent::Module { name, index, total } => {
            say(format!("  module {name} ({index}/{total})"));
        }
        CreateEvent::Output { .. } => {}
    }
}

fn print_restore_event(e: odhive_core::db::RestoreEvent) {
    use odhive_core::db::RestoreEvent;
    match e {
        RestoreEvent::Step { step } => say(format!("{step}…")),
        RestoreEvent::Inspected {
            format,
            unpacked_bytes,
            filestore_files,
        } => say(format!(
            "  {format:?} dump, {unpacked_bytes} bytes unpacked, {filestore_files} filestore files"
        )),
        RestoreEvent::Output { .. } => {}
    }
}

fn print_delete_event(e: odhive_core::db::DeleteEvent) {
    use odhive_core::db::DeleteEvent;
    match e {
        DeleteEvent::Step { step } => say(format!("{step}…")),
        DeleteEvent::Output { .. } => {}
    }
}

fn print_duplicate_event(e: odhive_core::db::DuplicateEvent) {
    use odhive_core::db::DuplicateEvent;
    match e {
        DuplicateEvent::Step { step } => say(format!("{step}…")),
        DuplicateEvent::Output { .. } => {}
    }
}

fn print_backup_event(e: odhive_core::db::BackupEvent) {
    use odhive_core::db::BackupEvent;
    match e {
        BackupEvent::Step { step } => say(format!("{step}…")),
        BackupEvent::Output { .. } => {}
    }
}

fn print_snapshot_event(e: odhive_core::db::SnapshotEvent) {
    use odhive_core::db::SnapshotEvent;
    match e {
        SnapshotEvent::Step { step } => say(format!("{step}…")),
        SnapshotEvent::Output { .. } => {}
    }
}

/// Prints a job's progress until it ends; Ctrl+C cancels it.
async fn follow_job(c: &Client, mut job: Job) -> anyhow::Result<Job> {
    let mut signals = Signals::new()?;
    let mut steps = 0;
    let mut lines = 0u64;
    let mut phase = String::new();
    let mut cancelling = false;
    loop {
        for step in job.steps.iter().skip(steps) {
            match step.clone() {
                JobStep::Setup(e) => print_setup_event(e),
                JobStep::Clone(e) => print_clone_event(&mut phase, e),
                JobStep::CreateDatabase(e) => print_create_event(e),
                JobStep::RestoreDatabase(e) => print_restore_event(e),
                JobStep::DeleteDatabase(e) => print_delete_event(e),
                JobStep::DuplicateDatabase(e) => print_duplicate_event(e),
                JobStep::BackupDatabase(e) => print_backup_event(e),
                JobStep::SnapshotDatabase(e) | JobStep::RestoreSnapshot(e) => {
                    print_snapshot_event(e);
                }
            }
        }
        steps = steps.max(job.steps.len());
        let total = job.output_dropped + job.output.len() as u64;
        let new = usize::try_from(total.saturating_sub(lines)).unwrap_or(usize::MAX);
        for line in job.output.iter().skip(job.output.len().saturating_sub(new)) {
            say(format!("  {line}"));
        }
        lines = lines.max(total);
        if job.state != JobState::Running {
            break;
        }
        tokio::select! {
            () = signals.next(), if !cancelling => {
                say("cancelling…");
                cancelling = true;
                let reply = c
                    .call(Method::POST, &format!("{API}/jobs/{}/cancel", job.id), None::<&()>, SLOW_TIMEOUT)
                    .await?;
                job = reply.json()?;
                continue;
            }
            () = tokio::time::sleep(POLL) => {}
        }
        job = c.get(&format!("{API}/jobs/{}", job.id)).await?.json()?;
    }
    match job.state {
        JobState::Succeeded => Ok(job),
        JobState::Failed => bail!(
            "{}",
            job.error
                .as_ref()
                .map_or("the job failed", |e| e.message.as_str())
        ),
        JobState::Cancelled => bail!("cancelled"),
        JobState::Running => unreachable!("loop ends on a final state"),
    }
}

async fn projects(c: &Client) -> anyhow::Result<Vec<Project>> {
    c.get(&format!("{API}/projects")).await?.json()
}

async fn project(c: &Client, name: &str) -> anyhow::Result<Project> {
    c.get(&format!("{API}/projects/{name}")).await?.json()
}

/// A process runs for the instance, here or outside the service.
fn is_running(i: &InstanceView) -> bool {
    i.external.is_some() || i.live.as_ref().is_some_and(|l| l.status.pid().is_some())
}

async fn project_list(c: &Client) -> anyhow::Result<ExitCode> {
    let list = projects(c).await?;
    if list.is_empty() {
        say("no projects; create one with `odhive project add <name> --source <id|path>`");
        return Ok(ExitCode::SUCCESS);
    }
    let rows: Vec<Vec<String>> = list
        .iter()
        .map(|p| match &p.error {
            Some(e) => vec![
                p.name.clone(),
                "?".into(),
                format!("error: {e}"),
                p.path.clone(),
            ],
            None => {
                let running = p.instances.iter().filter(|i| is_running(i)).count();
                vec![
                    p.name.clone(),
                    p.odoo_version.clone().unwrap_or_else(|| "?".into()),
                    format!("{running}/{} running", p.instances.len()),
                    p.path.clone(),
                ]
            }
        })
        .collect();
    table(&["NAME", "ODOO", "INSTANCES", "PATH"], &rows);
    Ok(ExitCode::SUCCESS)
}

async fn project_remove(c: &Client, name: &str) -> anyhow::Result<ExitCode> {
    let p = project(c, name).await?;
    let running: Vec<String> = p
        .instances
        .iter()
        .filter(|i| is_running(i))
        .map(|i| PortOwner::new(name, &i.id).to_string())
        .collect();
    if !running.is_empty() {
        bail!(
            "{} still running; stop it first with `odhive stop {name}`",
            running.join(", ")
        );
    }
    c.request(Method::DELETE, &format!("{API}/projects/{name}"))
        .await?
        .check()?;
    say(format!(
        "removed project {name}; its folder is kept at {}",
        p.path
    ));
    Ok(ExitCode::SUCCESS)
}

// -------------------------------------------------------------- instances

/// The instances of `p` that `target` names.
fn select<'a>(p: &'a Project, target: &Target) -> anyhow::Result<Vec<&'a InstanceView>> {
    if let Some(e) = &p.error {
        bail!("project {} cannot be read: {e}", p.name);
    }
    let picked: Vec<&InstanceView> = p
        .instances
        .iter()
        .filter(|i| target.instance.as_ref().is_none_or(|id| *id == i.id))
        .collect();
    if picked.is_empty() {
        match &target.instance {
            Some(id) => bail!("project {} has no instance {id:?}", p.name),
            None => bail!("project {} has no instances", p.name),
        }
    }
    Ok(picked)
}

fn instance_path(owner: &PortOwner, action: &str) -> String {
    format!(
        "{API}/projects/{}/instances/{}/{action}",
        owner.project, owner.instance
    )
}

async fn status(c: &Client, target: Option<&Target>) -> anyhow::Result<ExitCode> {
    let list = match target {
        Some(t) => vec![project(c, &t.project).await?],
        None => projects(c).await?,
    };
    if list.is_empty() {
        say("no projects; create one with `odhive project add <name> --source <id|path>`");
        return Ok(ExitCode::SUCCESS);
    }
    let mut rows = Vec::new();
    let mut detached = false;
    for p in &list {
        if let Some(e) = &p.error {
            rows.push(vec![p.name.clone(), format!("error: {e}"), String::new()]);
            continue;
        }
        let picked = match target {
            Some(t) => select(p, t)?,
            None => p.instances.iter().collect(),
        };
        for i in picked {
            let owner = PortOwner::new(&p.name, &i.id);
            let (state, pid, port) = match (&i.live, &i.external) {
                (Some(info), _) => {
                    let pid = info.status.pid();
                    (state_text(&info.status), pid, pid.and(info.http_port))
                }
                (None, Some(e)) => {
                    detached |= e.supervisor.is_none();
                    (
                        if e.supervisor.is_some() {
                            "running, in a terminal"
                        } else {
                            "running, detached"
                        },
                        Some(e.pid),
                        Some(e.http_port),
                    )
                }
                (None, None) => ("stopped", None, None),
            };
            rows.push(vec![
                owner.to_string(),
                state.into(),
                pid.map(|p| p.to_string()).unwrap_or_default(),
                port.map(|p| format!("http://localhost:{p}"))
                    .unwrap_or_default(),
                i.db.clone(),
            ]);
        }
    }
    table(&["INSTANCE", "STATE", "PID", "URL", "DATABASE"], &rows);
    if detached {
        say(
            "detached: started by an OdHive that has exited; its output is not captured. `odhive stop` it, then `odhive start` it in the service.",
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn state_text(s: &Status) -> &'static str {
    match s {
        Status::Starting { .. } => "starting",
        Status::Running { .. } => "running",
        Status::Stopping { .. } => "stopping",
        Status::Stopped { .. } => "stopped",
        Status::Crashed { .. } => "crashed",
        Status::FailedToStart { .. } => "failed to start",
    }
}

async fn stop(c: &Client, target: &Target) -> anyhow::Result<ExitCode> {
    let p = project(c, &target.project).await?;
    let mut jobs = JoinSet::new();
    let mut count = 0;
    for i in select(&p, target)? {
        let owner = PortOwner::new(&p.name, &i.id);
        if is_running(i) {
            say(format!("stopping {owner}…"));
            count += 1;
        } else if i.live.is_none() {
            continue;
        }
        let c = c.clone();
        jobs.spawn(async move {
            let path = instance_path(&owner, "stop");
            let result = c
                .call(Method::POST, &path, None::<&()>, SLOW_TIMEOUT)
                .await
                .and_then(|r| r.json::<StopResult>());
            (owner, result)
        });
    }
    let mut failed = false;
    while let Some(job) = jobs.join_next().await {
        let (owner, result) = job.context("stop task failed")?;
        match result {
            Ok(r) if r.was_running && r.killed => say(format!(
                "{owner}: killed (did not stop within {}s)",
                DEFAULT_STOP_TIMEOUT.as_secs()
            )),
            Ok(r) if r.was_running => say(format!("{owner}: stopped")),
            Ok(_) => {}
            Err(e) => {
                say(format!("{owner}: {e:#}"));
                failed = true;
            }
        }
    }
    if failed {
        bail!("some instances could not be stopped");
    }
    if count == 0 {
        say(format!("{target}: nothing is running"));
    }
    Ok(ExitCode::SUCCESS)
}

/// `start` / `restart`: hands the instances to the service, then shows what
/// they do until they end or Ctrl+C.
async fn run(c: &Client, target: &Target, quiet: bool, restart: bool) -> anyhow::Result<ExitCode> {
    let mut signals = Signals::new()?;
    let p = project(c, &target.project).await?;
    let owners: Vec<PortOwner> = select(&p, target)?
        .into_iter()
        .map(|i| PortOwner::new(&p.name, &i.id))
        .collect();
    // Subscribed first, so no output or event of the new processes is missed.
    let mut stream = if quiet {
        None
    } else {
        Some(Stream::open(c, &owners).await?)
    };
    let action = if restart { "restart" } else { "start" };
    for owner in &owners {
        let info: InstanceInfo = c
            .call(
                Method::POST,
                &instance_path(owner, action),
                None::<&()>,
                SLOW_TIMEOUT,
            )
            .await?
            .json()
            .with_context(|| format!("could not {action} {owner}"))?;
        if stream.is_none() {
            say(format!(
                "{owner}: starting in the OdHive service (pid {})",
                info.status
                    .pid()
                    .map_or_else(|| "?".into(), |p| p.to_string())
            ));
        }
    }
    let Some(stream) = stream.as_mut() else {
        say(format!(
            "`odhive logs -f {target}` shows the output, `odhive stop {target}` stops it"
        ));
        return Ok(ExitCode::SUCCESS);
    };
    let mut out = Output::new(false, owners.len() > 1);
    let mut prompt = Prompt::new();
    let mut fixes = JoinSet::new();
    let mut infos: HashMap<PortOwner, InstanceInfo> = HashMap::new();
    let mut ending = false;
    loop {
        tokio::select! {
            () = signals.next() => {
                say(format!(
                    "{target} keeps running in the OdHive service; `odhive stop {target}` stops it"
                ));
                return Ok(ExitCode::SUCCESS);
            }
            msg = stream.next() => match msg? {
                Some(ServerMessage::Logs { project, instance, records, .. }) => {
                    let owner = PortOwner::new(&project, &instance);
                    for r in &records {
                        out.record(&owner, r);
                    }
                }
                Some(ServerMessage::Gap { project, instance, skipped }) => say(format!(
                    "warning: {skipped} log entries of {project}#{instance} were not shown; the log file has them"
                )),
                Some(ServerMessage::Events { events }) => {
                    // A new process has new ports: read them before printing.
                    if events.iter().any(|e| matches!(e.event, InstanceEvent::Supervisor(Event::Spawned { .. }))) {
                        infos = instances(c).await?;
                    }
                    for ev in events.iter().filter(|e| owners.contains(&e.owner)) {
                        out.event(infos.get(&ev.owner), ev);
                        if let InstanceEvent::Manager(Notice::DependencyMissing {
                            module,
                            package: Some(package),
                            action: FixAction::Confirm,
                            ..
                        }) = &ev.event
                        {
                            prompt.ask(&ev.owner, module, package);
                        }
                    }
                }
                Some(ServerMessage::Snapshot { instances }) => {
                    infos = instances.into_iter().map(|i| (i.owner.clone(), i)).collect();
                }
                Some(ServerMessage::Error(e)) => say(format!("warning: {}", e.message)),
                Some(_) => {}
                None => {
                    say("the OdHive service stopped");
                    return Ok(ExitCode::FAILURE);
                }
            },
            Some((owner, yes)) = prompt.answers.recv() => {
                prompt.open -= 1;
                let c = c.clone();
                let action = if yes { "fix/confirm" } else { "fix/decline" };
                fixes.spawn(async move {
                    let r = c
                        .call(Method::POST, &instance_path(&owner, action), None::<&()>, SLOW_TIMEOUT)
                        .await
                        .and_then(|r| r.json::<Candidate>());
                    (owner, yes, r)
                });
            }
            Some(done) = fixes.join_next(), if !fixes.is_empty() => match done {
                Ok((owner, false, Ok(c))) => say(format!("{owner}: not installing {}", c.package)),
                Ok((owner, _, Err(e))) => say(format!("{owner}: {e:#}")),
                _ => {}
            },
            () = tokio::time::sleep(POLL) => {
                infos = instances(c).await?;
                let done = prompt.open == 0
                    && fixes.is_empty()
                    && owners.iter().all(|o| infos.get(o).is_none_or(InstanceInfo::is_done));
                // One more round for messages already on their way.
                if done && ending {
                    break;
                }
                ending = done;
            }
        }
    }
    Ok(if out.failed() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// What the service knows about the instances it runs.
async fn instances(c: &Client) -> anyhow::Result<HashMap<PortOwner, InstanceInfo>> {
    let list: Vec<InstanceInfo> = c.get(&format!("{API}/instances")).await?.json()?;
    Ok(list.into_iter().map(|i| (i.owner.clone(), i)).collect())
}

/// `logs`: from the service's buffers, when it runs every instance asked
/// for; `None` sends the command to the log files.
async fn logs(
    c: &Client,
    target: &Target,
    follow: bool,
    lines: usize,
) -> anyhow::Result<Option<ExitCode>> {
    let p = project(c, &target.project).await?;
    let owners: Vec<PortOwner> = select(&p, target)?
        .into_iter()
        .map(|i| PortOwner::new(&p.name, &i.id))
        .collect();
    let out = Output::new(false, owners.len() > 1);
    let lines = lines.clamp(1, MAX_LOG_LIMIT);
    let mut pages = Vec::new();
    for owner in &owners {
        let reply = c
            .get(&format!("{}?limit={lines}", instance_path(owner, "logs")))
            .await?;
        if reply.error_code().as_deref() == Some("not_running") {
            return Ok(None);
        }
        pages.push((owner, reply.json::<LogPage>()?));
    }
    for (owner, page) in &pages {
        for r in &page.records {
            out.record(owner, r);
        }
    }
    if !follow {
        return Ok(Some(ExitCode::SUCCESS));
    }
    let from: Vec<(PortOwner, u64)> = pages
        .into_iter()
        .map(|(o, page)| (o.clone(), page.next))
        .collect();
    let mut stream = Stream::open_from(c, &from).await?;
    let mut signals = Signals::new()?;
    loop {
        tokio::select! {
            () = signals.next() => return Ok(Some(ExitCode::SUCCESS)),
            msg = stream.next() => match msg? {
                Some(ServerMessage::Logs { project, instance, records, .. }) => {
                    let owner = PortOwner::new(&project, &instance);
                    for r in &records {
                        out.record(&owner, r);
                    }
                }
                Some(ServerMessage::Gap { project, instance, skipped }) => say(format!(
                    "warning: {skipped} log entries of {project}#{instance} were not shown; the log file has them"
                )),
                Some(_) => {}
                None => {
                    say("the OdHive service stopped");
                    return Ok(Some(ExitCode::FAILURE));
                }
            },
        }
    }
}

/// The service's WebSocket, subscribed to instance logs (and events).
struct Stream {
    ws: WebSocketStream<TcpStream>,
}

impl Stream {
    /// Follows new log entries and the events of `owners`.
    async fn open(c: &Client, owners: &[PortOwner]) -> anyhow::Result<Self> {
        let mut s = Self {
            ws: c.websocket().await?,
        };
        s.send(json!({ "op": "subscribe_events" })).await?;
        for o in owners {
            s.send(json!({
                "op": "subscribe_logs",
                "project": o.project,
                "instance": o.instance,
            }))
            .await?;
        }
        s.wait_subscribed(owners.len(), true).await?;
        Ok(s)
    }

    /// Follows the logs of the owners from the given sequence numbers.
    async fn open_from(c: &Client, from: &[(PortOwner, u64)]) -> anyhow::Result<Self> {
        let mut s = Self {
            ws: c.websocket().await?,
        };
        for (o, seq) in from {
            s.send(json!({
                "op": "subscribe_logs",
                "project": o.project,
                "instance": o.instance,
                "from": seq,
            }))
            .await?;
        }
        s.wait_subscribed(from.len(), false).await?;
        Ok(s)
    }

    async fn send(&mut self, msg: serde_json::Value) -> anyhow::Result<()> {
        self.ws
            .send(Message::text(msg.to_string()))
            .await
            .context("lost the connection to the OdHive service")
    }

    /// Waits until the service confirmed `logs` log subscriptions (and the
    /// event subscription), so nothing after this point is missed.
    async fn wait_subscribed(&mut self, mut logs: usize, mut events: bool) -> anyhow::Result<()> {
        let wait = async {
            while logs > 0 || events {
                match self.next().await? {
                    Some(ServerMessage::Subscribed { .. }) => logs -= 1,
                    Some(ServerMessage::Snapshot { .. }) => events = false,
                    Some(ServerMessage::Error(e)) => bail!("{}", e.message),
                    Some(_) => {}
                    None => bail!("the OdHive service closed the event stream"),
                }
            }
            Ok(())
        };
        tokio::time::timeout(REQUEST_TIMEOUT, wait)
            .await
            .map_err(|_| anyhow::anyhow!("the OdHive service did not answer in time"))?
    }

    /// The next message; `None` once the service closed the connection.
    async fn next(&mut self) -> anyhow::Result<Option<ServerMessage>> {
        loop {
            match self.ws.next().await {
                None | Some(Ok(Message::Close(_))) => return Ok(None),
                Some(Ok(Message::Text(text))) => {
                    return serde_json::from_str(text.as_str())
                        .map(Some)
                        .context("unexpected message from the OdHive service");
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => {
                    return Err(e).context("lost the connection to the OdHive service");
                }
            }
        }
    }
}
