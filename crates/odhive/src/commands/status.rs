//! `odhive status`

use std::process::ExitCode;

use odhive_core::launch::{self, Target};
use odhive_core::pidfile::RecordState;

use super::{Ctx, say, table};

pub fn status(ctx: &Ctx, target: Option<&Target>) -> anyhow::Result<ExitCode> {
    let targets: Vec<Target> = match target {
        Some(t) => vec![t.clone()],
        None => ctx
            .cfg
            .projects
            .iter()
            .map(|p| Target::project(&p.name))
            .collect(),
    };
    if targets.is_empty() {
        say("no projects; create one with `odhive project add <name> --source <id|path>`");
        return Ok(ExitCode::SUCCESS);
    }
    let run = ctx.run_dir();
    let mut rows = Vec::new();
    let mut detached = false;
    for t in &targets {
        let project = match launch::load_project(&ctx.cfg, &t.project) {
            Ok(p) => p,
            Err(e) if target.is_some() => return Err(e.into()),
            Err(e) => {
                rows.push(vec![t.to_string(), format!("error: {e}"), String::new()]);
                continue;
            }
        };
        for s in launch::states(&run, &project, t)? {
            let (state, pid, url) = match (&s.run, &s.record) {
                (Some(RecordState::Running { pid, supervisor }), Some(r)) => {
                    detached |= supervisor.is_none();
                    (
                        if supervisor.is_some() {
                            "running"
                        } else {
                            "running, detached"
                        },
                        pid.to_string(),
                        format!("http://localhost:{}", r.http_port),
                    )
                }
                (Some(RecordState::Stale { .. }), _) => {
                    ("stopped (stale record)", String::new(), String::new())
                }
                _ => ("stopped", String::new(), String::new()),
            };
            rows.push(vec![s.owner.to_string(), state.into(), pid, url, s.db]);
        }
    }
    table(&["INSTANCE", "STATE", "PID", "URL", "DATABASE"], &rows);
    if detached {
        say(
            "detached: started by an OdHive that has exited; its output is not captured. `odhive restart` takes it over.",
        );
    }
    Ok(ExitCode::SUCCESS)
}
