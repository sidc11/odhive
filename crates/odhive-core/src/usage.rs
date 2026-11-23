//! CPU and memory use of running instances.
//!
//! An instance is its `odoo-bin` process and every process below it (Odoo's
//! workers, its gevent process). CPU use is measured between two samples, so
//! the first sample of a process reports 0 %.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// Samples closer together than this reuse the previous one: CPU use over a
/// very short window says little, and many clients may ask at once.
pub const MIN_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

/// Resource use of one process tree.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Usage {
    /// CPU use since the previous sample, in percent of one core (can exceed
    /// 100 on several cores).
    pub cpu_percent: f32,
    /// Resident memory, in bytes.
    pub memory_bytes: u64,
    /// Processes in the tree.
    pub processes: u32,
    /// When the root process started, in seconds since the Unix epoch.
    pub started_at: u64,
}

/// Keeps the process table between samples, which CPU use needs.
#[derive(Debug)]
pub struct UsageMonitor {
    system: Option<System>,
    sampled: Option<Instant>,
}

impl Default for UsageMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl UsageMonitor {
    /// A monitor with no samples yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            system: None,
            sampled: None,
        }
    }

    /// Use of the process trees rooted at `roots`, keyed by root pid. A root
    /// that does not run is left out. Refreshes the process table unless the
    /// last refresh is less than [`MIN_SAMPLE_INTERVAL`] old. Returns an
    /// empty map when the process table cannot be read.
    pub fn sample(&mut self, roots: &[u32]) -> HashMap<u32, Usage> {
        if roots.is_empty() {
            return HashMap::new();
        }
        let fresh = self
            .sampled
            .is_some_and(|at| at.elapsed() < MIN_SAMPLE_INTERVAL);
        if self.system.is_none() {
            self.system = System::new().ok();
        }
        let Some(system) = self.system.as_mut() else {
            return HashMap::new();
        };
        if !fresh {
            system.refresh_processes_specifics(
                ProcessesToUpdate::All,
                true,
                ProcessRefreshKind::nothing().with_cpu().with_memory(),
            );
            self.sampled = Some(Instant::now());
        }
        trees(system, roots)
    }
}

fn trees(system: &System, roots: &[u32]) -> HashMap<u32, Usage> {
    // Threads are listed as processes on Linux; they share their process's
    // memory and CPU figures, so counting them would count twice.
    let mut children: HashMap<Pid, Vec<Pid>> = HashMap::new();
    for (pid, process) in system.processes() {
        if process.thread_kind().is_some() {
            continue;
        }
        if let Some(parent) = process.parent() {
            children.entry(parent).or_default().push(*pid);
        }
    }
    let mut out = HashMap::with_capacity(roots.len());
    for &root in roots {
        let root_pid = Pid::from_u32(root);
        let Some(process) = system.process(root_pid) else {
            continue;
        };
        let mut usage = Usage {
            cpu_percent: 0.0,
            memory_bytes: 0,
            processes: 0,
            started_at: process.start_time(),
        };
        let mut seen = HashSet::new();
        let mut stack = vec![root_pid];
        while let Some(pid) = stack.pop() {
            if !seen.insert(pid) {
                continue;
            }
            if let Some(p) = system.process(pid) {
                usage.cpu_percent += p.cpu_usage();
                usage.memory_bytes += p.memory();
                usage.processes += 1;
            }
            if let Some(kids) = children.get(&pid) {
                stack.extend(kids.iter().copied());
            }
        }
        out.insert(root, usage);
    }
    out
}

#[cfg(test)]
mod tests {
    use std::process::{Command, Stdio};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn no_roots_needs_no_sample() {
        let mut monitor = UsageMonitor::new();
        assert!(monitor.sample(&[]).is_empty());
        assert!(monitor.sampled.is_none());
    }

    #[test]
    fn own_process_is_measured() {
        let mut monitor = UsageMonitor::new();
        let me = std::process::id();
        let usage = monitor.sample(&[me])[&me];
        assert!(usage.processes >= 1);
        assert!(usage.memory_bytes > 0);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(usage.started_at <= now && usage.started_at > now - 3600);
    }

    #[test]
    fn children_are_counted_and_missing_roots_left_out() {
        let mut child = Command::new("sleep")
            .arg("30")
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let mut monitor = UsageMonitor::new();
        let me = std::process::id();
        let alone = {
            let mut solo = UsageMonitor::new();
            solo.sample(&[child.id()])[&child.id()]
        };
        assert_eq!(alone.processes, 1);
        let out = monitor.sample(&[me, u32::MAX - 1]);
        assert!(out[&me].processes >= 2, "{:?}", out[&me]);
        assert!(!out.contains_key(&(u32::MAX - 1)));
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn samples_close_together_reuse_the_table() {
        let mut monitor = UsageMonitor::new();
        let me = std::process::id();
        monitor.sample(&[me]);
        let first = monitor.sampled.unwrap();
        monitor.sample(&[me]);
        assert_eq!(monitor.sampled.unwrap(), first);
    }
}
