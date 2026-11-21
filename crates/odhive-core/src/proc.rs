//! Small helpers shared by modules that run child processes.

use std::collections::VecDeque;
use std::io;
use std::process::{ExitStatus, Stdio};

use rustix::process::{Pid, Signal, kill_process_group};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::mpsc::UnboundedSender;

/// Longest output line passed on by [`forward_lines`]; longer lines are split.
pub(crate) const MAX_LINE_BYTES: usize = 4096;

/// Reads at most `cap + 1` bytes (so callers can tell "over the cap"), then
/// drains the rest so the child never blocks on a full pipe.
pub(crate) async fn read_capped(r: impl AsyncRead + Unpin, cap: u64) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut r = r.take(cap + 1);
    r.read_to_end(&mut buf).await?;
    tokio::io::copy(&mut r.into_inner(), &mut tokio::io::sink()).await?;
    Ok(buf)
}

/// Sends each non-empty line of `r` (split on `\n` or `\r`, trailing
/// whitespace trimmed, invalid UTF-8 replaced) to `tx`. A line longer than
/// [`MAX_LINE_BYTES`] is passed on in chunks instead of being buffered.
pub(crate) async fn forward_lines(
    r: impl AsyncRead + Unpin,
    tx: UnboundedSender<String>,
) -> io::Result<()> {
    forward_lines_with(r, tx, |line| line).await
}

/// Like [`forward_lines`], but passes each line through `wrap` first (to tag
/// it with the stream it came from, say).
pub(crate) async fn forward_lines_with<T>(
    mut r: impl AsyncRead + Unpin,
    tx: UnboundedSender<T>,
    wrap: impl Fn(String) -> T,
) -> io::Result<()> {
    let mut pending = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = r.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        pending.extend_from_slice(&buf[..n]);
        while let Some(pos) = pending.iter().position(|b| *b == b'\n' || *b == b'\r') {
            let line: Vec<u8> = pending.drain(..=pos).collect();
            send_line(&line[..pos], &tx, &wrap);
        }
        while pending.len() >= MAX_LINE_BYTES {
            let chunk: Vec<u8> = pending.drain(..MAX_LINE_BYTES).collect();
            send_line(&chunk, &tx, &wrap);
        }
    }
    send_line(&pending, &tx, &wrap);
    Ok(())
}

fn send_line<T>(raw: &[u8], tx: &UnboundedSender<T>, wrap: &impl Fn(String) -> T) {
    let line = String::from_utf8_lossy(raw).trim_end().to_owned();
    if !line.trim().is_empty() {
        // The receiver only goes away when the caller itself was dropped.
        let _ = tx.send(wrap(line));
    }
}

/// Environment variables that point a child Python at another interpreter,
/// prefix or package set. Removed before running venv / install tools so
/// OdHive's own environment cannot leak into a project venv.
pub(crate) const PYTHON_ENV_OVERRIDES: [&str; 5] = [
    "VIRTUAL_ENV",
    "PYTHONHOME",
    "PYTHONPATH",
    "UV_PYTHON",
    "CONDA_PREFIX",
];

/// Error from [`run_streaming`].
#[derive(Debug)]
pub(crate) enum RunError {
    /// The child could not be started.
    Spawn(io::Error),
    /// Reading its output or waiting for it failed.
    Io(io::Error),
}

/// How a [`run_streaming`] child ended.
#[derive(Debug)]
pub(crate) struct Finished {
    /// Exit status.
    pub(crate) status: ExitStatus,
    /// Last `tail_lines` output lines, joined with `\n`.
    pub(crate) tail: String,
}

/// Runs `cmd` in its own process group with stdin closed, passing each line of
/// stdout and stderr to `on_line` as it arrives.
///
/// Dropping the returned future (cancel, timeout) kills the whole process
/// group, so helpers the tool started (compilers, build backends) die too.
pub(crate) async fn run_streaming(
    mut cmd: Command,
    tail_lines: usize,
    mut on_line: impl FnMut(String),
) -> Result<Finished, RunError> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(RunError::Spawn)?;
    let mut group = GroupKill(group_of(&child));
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let out_task = tokio::spawn(forward_lines(stdout, tx.clone()));
    let err_task = tokio::spawn(forward_lines(stderr, tx));
    let mut tail = VecDeque::with_capacity(tail_lines);
    while let Some(line) = rx.recv().await {
        if tail_lines > 0 {
            if tail.len() == tail_lines {
                tail.pop_front();
            }
            tail.push_back(line.clone());
        }
        on_line(line);
    }
    for task in [out_task, err_task] {
        task.await
            .map_err(|e| RunError::Io(io::Error::other(e)))?
            .map_err(RunError::Io)?;
    }
    let status = child.wait().await.map_err(RunError::Io)?;
    // Reaped: the pid may be reused from now on, so never signal it again.
    group.0 = None;
    Ok(Finished {
        status,
        tail: Vec::from(tail).join("\n"),
    })
}

/// The process group led by `child` (spawned with `process_group(0)`).
pub(crate) fn group_of(child: &tokio::process::Child) -> Option<Pid> {
    child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .and_then(Pid::from_raw)
}

/// Sends SIGKILL to a process group on drop, unless cleared.
pub(crate) struct GroupKill(pub(crate) Option<Pid>);

impl Drop for GroupKill {
    fn drop(&mut self) {
        if let Some(pid) = self.0.take() {
            // ESRCH (group already gone) is fine.
            let _ = kill_process_group(pid, Signal::KILL);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn output_is_split_into_bounded_lines() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut input = b"one\r\ntwo\rthree\n\n   \n".to_vec();
        input.extend(std::iter::repeat_n(b'x', MAX_LINE_BYTES * 3));
        forward_lines(&input[..], tx).await.expect("forward");
        let mut lines = Vec::new();
        while let Some(l) = rx.recv().await {
            lines.push(l);
        }
        assert_eq!(&lines[..3], ["one", "two", "three"]);
        assert!(lines[3..].iter().all(|l| l.len() <= MAX_LINE_BYTES));
        assert_eq!(
            lines[3..].iter().map(String::len).sum::<usize>(),
            MAX_LINE_BYTES * 3
        );
    }

    #[tokio::test]
    async fn streams_both_pipes_and_keeps_a_tail() {
        let mut cmd = Command::new("/bin/sh");
        cmd.args([
            "-c",
            "for i in 1 2 3 4; do echo out$i; echo err$i >&2; done; exit 3",
        ]);
        let mut seen = Vec::new();
        let done = run_streaming(cmd, 2, |l| seen.push(l)).await.expect("run");
        assert_eq!(done.status.code(), Some(3));
        assert_eq!(seen.len(), 8);
        assert_eq!(done.tail.lines().count(), 2);
    }

    #[tokio::test]
    async fn spawn_failure_is_reported_as_such() {
        let cmd = Command::new("/nonexistent/odhive-tool");
        let err = run_streaming(cmd, 5, |_| {}).await.expect_err("spawn");
        assert!(matches!(err, RunError::Spawn(_)), "{err:?}");
    }

    /// Cancelling kills grandchildren too, not just the direct child.
    #[tokio::test]
    async fn cancel_kills_the_process_group() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pidfile = dir.path().join("pid");
        let mut cmd = Command::new("/bin/sh");
        // The grandchild writes its pid, then sleeps; the shell waits on it.
        cmd.arg("-c")
            .arg(r#"sh -c 'echo $$ > "$0"; exec sleep 30' "$1"; wait"#)
            .arg("sh")
            .arg(&pidfile);
        let run = run_streaming(cmd, 0, |_| {});
        let wait_pid = async {
            loop {
                if let Ok(s) = std::fs::read_to_string(&pidfile)
                    && let Ok(pid) = s.trim().parse::<i32>()
                {
                    return pid;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        let pid = tokio::select! {
            _ = run => panic!("finished early"),
            pid = wait_pid => pid,
        };
        // `run` has been dropped here.
        let pid = Pid::from_raw(pid).expect("pid");
        let gone = async {
            while rustix::process::test_kill_process(pid).is_ok() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), gone)
            .await
            .expect("grandchild still alive after cancel");
    }
}
