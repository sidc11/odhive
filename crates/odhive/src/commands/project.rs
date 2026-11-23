//! `odhive project …`

use std::process::ExitCode;

use anyhow::{Context, bail};
use odhive_core::launch::{self, Target};
use odhive_core::project::{DEFAULT_INSTANCE, Instance};
use odhive_core::python::Compat;
use odhive_core::requirements::{InstallEvent, Origin};
use odhive_core::setup::{self, EnvOptions, NewProject, SetupEvent};
use odhive_core::venv::VenvEvent;

use super::{Ctx, say, table};
use crate::cli::{EnvArgs, ProjectAddArgs, ProjectSetupArgs};

pub async fn add(ctx: &mut Ctx, args: ProjectAddArgs) -> anyhow::Result<ExitCode> {
    let community = setup::community_source(&mut ctx.cfg, &args.source)?;
    let enterprise = args
        .enterprise
        .as_deref()
        .map(|e| setup::enterprise_source(&mut ctx.cfg, e, community.odoo_version))
        .transpose()?;
    let python = args
        .env
        .python
        .as_deref()
        .map(std::path::absolute)
        .transpose()
        .context("invalid --python path")?;
    let new = NewProject {
        name: args.name.clone(),
        community: community.id.clone(),
        enterprise: enterprise.map(|e| e.id),
        addons_paths: args.addons,
        python: python.clone(),
        instance: Instance::new(DEFAULT_INSTANCE),
    };
    let mut project = setup::create_project(&mut ctx.cfg, &ctx.ws, &new)?;
    ctx.save()?;
    say(format!(
        "created project {} (Odoo {}) at {}",
        args.name,
        community.odoo_version,
        project.dir().display()
    ));
    if args.no_setup {
        say(format!(
            "next: `odhive project setup {}`, then `odhive start {}`",
            args.name, args.name
        ));
        return Ok(ExitCode::SUCCESS);
    }
    run_setup(ctx, &mut project, &args.env, false).await?;
    say(format!("ready: `odhive start {}`", args.name));
    Ok(ExitCode::SUCCESS)
}

pub async fn setup(ctx: &mut Ctx, args: ProjectSetupArgs) -> anyhow::Result<ExitCode> {
    let mut project = launch::load_project(&ctx.cfg, &args.name)?;
    run_setup(ctx, &mut project, &args.env, args.recreate).await?;
    say(format!("ready: `odhive start {}`", args.name));
    Ok(ExitCode::SUCCESS)
}

async fn run_setup(
    ctx: &Ctx,
    project: &mut launch::LoadedProject,
    env: &EnvArgs,
    recreate: bool,
) -> anyhow::Result<()> {
    let mut opts = EnvOptions::from_env();
    opts.python = env
        .python
        .as_deref()
        .map(std::path::absolute)
        .transpose()
        .context("invalid --python path")?;
    opts.recreate = recreate;
    opts.install = !env.no_requirements;
    let name = project.config.name.clone();
    setup::setup_env(&ctx.cfg, project, &opts, print_setup_event)
        .await
        .with_context(|| {
            format!(
                "setting up {name} failed; fix the problem and run `odhive project setup {name}`"
            )
        })?;
    Ok(())
}

pub(super) fn print_setup_event(event: SetupEvent) {
    match event {
        SetupEvent::Python {
            path,
            version,
            compat,
            support,
        } => {
            say(format!("Python {version} at {}", path.display()));
            if compat == Compat::TooNew {
                say(format!(
                    "warning: newer than this Odoo supports ({support}); some requirements may not build"
                ));
            }
        }
        SetupEvent::Venv(VenvEvent::Removing { dir }) => {
            say(format!("removing {}", dir.display()));
        }
        SetupEvent::Venv(VenvEvent::Started { tool, dir, .. }) => {
            say(format!("creating {} with {tool}", dir.display()));
        }
        SetupEvent::Venv(VenvEvent::Output { line })
        | SetupEvent::Install(InstallEvent::Output { line }) => say(format!("  {line}")),
        SetupEvent::Venv(VenvEvent::Finished { python, version }) => {
            say(format!(
                "venv ready: {} (Python {version})",
                python.display()
            ));
        }
        SetupEvent::VenvKept { dir } => {
            say(format!(
                "keeping {} (use --recreate to rebuild it)",
                dir.display()
            ));
        }
        SetupEvent::Collected {
            count,
            modules,
            skipped,
            warnings,
        } => {
            say(format!(
                "{count} requirements (Odoo requirements.txt + {modules} module manifests)"
            ));
            for s in skipped {
                say(format!(
                    "  skipped {:?} from {}: {}",
                    s.text,
                    origin(&s.origin),
                    s.reason
                ));
            }
            for w in warnings {
                say(format!("  warning: {}: {}", w.path.display(), w.message));
            }
        }
        SetupEvent::Install(InstallEvent::Started { tool, count, .. }) => {
            say(format!("installing {count} requirements with {tool}"));
        }
        SetupEvent::Install(InstallEvent::Finished { count }) => {
            say(format!("installed {count} requirements"));
        }
    }
}

fn origin(o: &Origin) -> String {
    match o {
        Origin::File { path, line } => format!("{}:{line}", path.display()),
        Origin::Manifest { module, .. } => format!("module {module}"),
        Origin::Missing { module } => format!("missing module {module}"),
    }
}

pub fn list(ctx: &Ctx) -> anyhow::Result<ExitCode> {
    if ctx.cfg.projects.is_empty() {
        say("no projects; create one with `odhive project add <name> --source <id|path>`");
        return Ok(ExitCode::SUCCESS);
    }
    let run = ctx.run_dir();
    let mut rows = Vec::new();
    for entry in &ctx.cfg.projects {
        let row = match launch::load_project(&ctx.cfg, &entry.name) {
            Ok(p) => {
                let odoo = p
                    .config
                    .resolve_sources(&ctx.cfg)
                    .map_or_else(|e| format!("? ({e})"), |(c, _)| c.odoo_version.to_string());
                let states = launch::states(&run, &p, &Target::project(&entry.name))?;
                let running = states.iter().filter(|s| s.is_running()).count();
                vec![
                    entry.name.clone(),
                    odoo,
                    format!("{running}/{} running", states.len()),
                    entry.path.display().to_string(),
                ]
            }
            Err(e) => vec![
                entry.name.clone(),
                "?".into(),
                format!("error: {e}"),
                entry.path.display().to_string(),
            ],
        };
        rows.push(row);
    }
    table(&["NAME", "ODOO", "INSTANCES", "PATH"], &rows);
    Ok(ExitCode::SUCCESS)
}

pub fn remove(ctx: &mut Ctx, name: &str) -> anyhow::Result<ExitCode> {
    let running: Vec<String> = launch::running_records(&ctx.run_dir())?
        .iter()
        .filter(|r| r.project == name)
        .map(|r| r.owner().to_string())
        .collect();
    if !running.is_empty() {
        bail!(
            "{} still running; stop it first with `odhive stop {name}`",
            running.join(", ")
        );
    }
    let Some(entry) = setup::unregister_project(&mut ctx.cfg, name) else {
        bail!("no project named {name:?}");
    };
    ctx.save()?;
    say(format!(
        "removed project {name}; its folder is kept at {}",
        entry.path.display()
    ));
    Ok(ExitCode::SUCCESS)
}
