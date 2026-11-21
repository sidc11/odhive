//! `odhive logs`

use std::process::ExitCode;
use std::time::Duration;

use odhive_core::launch::{self, Target};
use odhive_core::logs::{self, Follower};

use super::{Ctx, say};

const POLL: Duration = Duration::from_millis(250);

pub async fn logs(
    ctx: &Ctx,
    target: &Target,
    follow: bool,
    lines: usize,
) -> anyhow::Result<ExitCode> {
    let project = launch::load_project(&ctx.cfg, &target.project)?;
    let instances = project.select(target)?;
    let prefix = instances.len() > 1;
    let dir = project.logs_dir();
    let mut followers = Vec::new();
    let mut any = false;
    for i in instances {
        let path = logs::log_path(&dir, &i.id)?;
        any |= path.exists();
        for line in logs::read_tail(&path, lines)? {
            print_line(prefix, &i.id, &line);
        }
        followers.push((i.id.clone(), Follower::from_end(&path)));
    }
    if !follow {
        if !any {
            say(format!(
                "{target}: no log yet; it is written while `odhive start {target}` runs"
            ));
        }
        return Ok(ExitCode::SUCCESS);
    }
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return Ok(ExitCode::SUCCESS),
            () = tokio::time::sleep(POLL) => {
                for (id, f) in &mut followers {
                    for line in f.poll()? {
                        print_line(prefix, id, &line);
                    }
                }
            }
        }
    }
}

fn print_line(prefix: bool, instance: &str, line: &str) {
    if prefix {
        say(format!("[{instance}] {line}"));
    } else {
        say(line);
    }
}
