//! `odhive source …`

use std::path::Path;
use std::process::ExitCode;

use anyhow::bail;
use odhive_core::config::Edition;
use odhive_core::project::ProjectConfig;
use odhive_core::sources::{self, CloneEvent, CloneOptions};
use odhive_core::version::OdooVersion;

use super::{Ctx, say, table};

pub fn add(
    ctx: &mut Ctx,
    path: &Path,
    enterprise: Option<OdooVersion>,
) -> anyhow::Result<ExitCode> {
    let entry = match enterprise {
        Some(v) => sources::register_enterprise(&mut ctx.cfg, path, v)?,
        None => sources::register_community(&mut ctx.cfg, path)?,
    };
    ctx.save()?;
    say(format!(
        "source {} (Odoo {} {}) at {}",
        entry.id,
        entry.odoo_version,
        edition(entry.edition),
        entry.path.display()
    ));
    Ok(ExitCode::SUCCESS)
}

pub async fn clone(ctx: &mut Ctx, version: OdooVersion) -> anyhow::Result<ExitCode> {
    let mut phase = String::new();
    let entry = sources::clone_community(
        &mut ctx.cfg,
        &ctx.ws.clone(),
        version,
        &CloneOptions::default(),
        |e| match e {
            CloneEvent::Started {
                url,
                branch,
                target,
            } => {
                say(format!(
                    "cloning {url} ({branch}) into {}",
                    target.display()
                ));
            }
            CloneEvent::Progress { phase: p, .. } if p != phase => {
                say(format!("  {p}…"));
                phase = p;
            }
            CloneEvent::Progress { .. } | CloneEvent::Finished { .. } => {}
        },
    )
    .await?;
    ctx.save()?;
    say(format!("source {} at {}", entry.id, entry.path.display()));
    Ok(ExitCode::SUCCESS)
}

pub fn list(ctx: &Ctx) -> anyhow::Result<ExitCode> {
    if ctx.cfg.sources.is_empty() {
        say("no sources; add one with `odhive source add <path>` or `odhive source clone 19.0`");
        return Ok(ExitCode::SUCCESS);
    }
    let rows: Vec<Vec<String>> = ctx
        .cfg
        .sources
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

pub fn remove(ctx: &mut Ctx, id: &str) -> anyhow::Result<ExitCode> {
    let users: Vec<&str> = ctx
        .cfg
        .projects
        .iter()
        .filter(|p| {
            ProjectConfig::load(&p.path).is_ok_and(|c| {
                c.sources.community == id || c.sources.enterprise.as_deref() == Some(id)
            })
        })
        .map(|p| p.name.as_str())
        .collect();
    if !users.is_empty() {
        bail!("source {id:?} is used by {}", users.join(", "));
    }
    let entry = sources::remove_source(&mut ctx.cfg, id)?;
    ctx.save()?;
    say(format!(
        "removed source {}; files kept at {}",
        entry.id,
        entry.path.display()
    ));
    Ok(ExitCode::SUCCESS)
}

fn edition(e: Edition) -> &'static str {
    match e {
        Edition::Community => "community",
        Edition::Enterprise => "enterprise",
    }
}
