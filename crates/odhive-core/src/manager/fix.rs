//! Dependency auto-fix for one managed instance (see [`crate::autofix`]).
//!
//! The instance's log hands every completed entry to a [`Watcher`] (from the
//! supervisor callback, so none is missed). An entry reporting a missing
//! module is queued for the instance's worker task, which decides what to do
//! and, to install, runs the installer and restarts the instance on the same
//! ports. While something is queued or being installed the instance counts
//! as busy, so a crash is not taken as the end of it.
//!
//! Each process run of an instance is acted on once: the first missing
//! module of a run is installed (or awaits confirmation, or is given up on);
//! later reports of that run are ignored, because they come from the same
//! failure and the restart replaces the process anyway.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use serde::Serialize;
use tokio::sync::{mpsc, watch};

use super::{Inner, Notice, Slot};
use crate::autofix::{
    self, Action, Attempts, AutofixMode, Candidate, GiveUp, MAX_ATTEMPTS, Trust, Trusted,
};
use crate::logs::parse::Entry;
use crate::ports::PortOwner;
use crate::requirements::{self, InstallEvent, InstallOptions};

/// Auto-fix settings of one instance.
#[derive(Debug, Clone)]
pub struct AutoFix {
    /// What to do about a missing module.
    pub mode: AutofixMode,
    /// The instance's venv interpreter and installer.
    pub install: InstallOptions,
    /// Packages that may be installed without asking.
    pub trusted: Arc<Trusted>,
}

/// What the auto-fix does about a missing module.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum FixAction {
    /// The package is installed now, then the instance restarts.
    Install,
    /// Waiting for [`super::Manager::confirm_fix`] or
    /// [`super::Manager::decline_fix`].
    Confirm,
    /// Reported only ([`AutofixMode::Off`]).
    Report,
    /// Nothing will be installed for this start.
    GiveUp {
        /// Why.
        reason: GiveUp,
    },
}

/// Auto-fix state shown with an instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct FixInfo {
    /// Mode.
    pub mode: AutofixMode,
    /// A missing module is being handled (queued, installing, restarting).
    pub active: bool,
    /// Installs since the instance was started.
    pub attempts: u32,
    /// Awaiting confirmation.
    pub pending: Option<Candidate>,
}

#[derive(Debug)]
struct Detected {
    run: u64,
    module: String,
}

#[derive(Debug, Default)]
struct State {
    attempts: Attempts,
    busy: bool,
    pending: Option<Candidate>,
    /// Newest run acted on.
    done_run: u64,
    /// Reported only (mode off): `(run, module)`.
    reported: BTreeSet<(u64, String)>,
}

/// The auto-fix of one instance start; kept across restarts and the port
/// retry, replaced by a new start.
#[derive(Debug)]
pub(super) struct FixState {
    config: AutoFix,
    queued: AtomicUsize,
    state: Mutex<State>,
    /// Bumped to cancel a running install.
    cancel: watch::Sender<u64>,
}

/// Hands log entries of an instance to its auto-fix.
#[derive(Debug)]
pub(super) struct Watcher {
    tx: mpsc::UnboundedSender<Detected>,
    fix: Arc<FixState>,
}

impl Watcher {
    /// Queues `entry` if it reports a missing module. `run` counts the
    /// instance's processes.
    pub fn observe(&self, entry: &Entry, run: u64) {
        if let Some(module) = autofix::missing_module(entry) {
            // Counted before the status that follows the entry is published.
            self.fix.queued.fetch_add(1, Ordering::SeqCst);
            if self.tx.send(Detected { run, module }).is_err() {
                self.fix.queued.fetch_sub(1, Ordering::SeqCst);
            }
        }
    }
}

enum Step {
    Nothing,
    Notify(Notice),
    Install(Candidate, u32, Notice),
}

impl FixState {
    pub fn new(config: AutoFix) -> Arc<Self> {
        Arc::new(Self {
            config,
            queued: AtomicUsize::new(0),
            state: Mutex::new(State::default()),
            cancel: watch::channel(0).0,
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // Every update leaves the state usable, so it is valid after a panic.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A watcher feeding a new worker task. The previous watcher's worker
    /// ends once its watcher is dropped and its queue is drained.
    pub fn watch(self: &Arc<Self>, inner: Weak<Inner>, owner: PortOwner) -> Watcher {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(worker(inner, owner, self.clone(), rx));
        Watcher {
            tx,
            fix: self.clone(),
        }
    }

    /// Something is queued or being installed.
    pub fn active(&self) -> bool {
        self.queued.load(Ordering::SeqCst) > 0 || self.lock().busy
    }

    pub fn info(&self) -> FixInfo {
        let s = self.lock();
        FixInfo {
            mode: self.config.mode,
            active: s.busy || self.queued.load(Ordering::SeqCst) > 0,
            attempts: s.attempts.used(),
            pending: s.pending.clone(),
        }
    }

    /// Stops a running install.
    pub fn cancel(&self) {
        self.cancel.send_modify(|n| *n = n.wrapping_add(1));
    }

    /// A restart asked for by the user counts as a new start.
    pub fn reset(&self) {
        let mut s = self.lock();
        s.attempts = Attempts::new();
        s.pending = None;
    }

    /// Takes the pending candidate for installing.
    pub fn take_pending(&self) -> Option<Result<(Candidate, u32), GiveUp>> {
        let mut s = self.lock();
        if s.busy {
            return None;
        }
        let candidate = s.pending.take()?;
        Some(match s.attempts.begin(&candidate.package) {
            Ok(attempt) => {
                s.busy = true;
                Ok((candidate, attempt))
            }
            Err(g) => Err(g),
        })
    }

    pub fn decline(&self) -> Option<Candidate> {
        self.lock().pending.take()
    }

    fn step(&self, d: Detected) -> Step {
        let mut s = self.lock();
        if d.run <= s.done_run || s.busy {
            return Step::Nothing;
        }
        let missing =
            |package: Option<String>, trust: Option<Trust>, action| Notice::DependencyMissing {
                module: d.module.clone(),
                package,
                trust,
                action,
            };
        let candidate = match autofix::resolve(&d.module, &self.config.trusted) {
            Ok(c) => c,
            Err(refusal) => {
                s.done_run = d.run;
                return Step::Notify(missing(
                    None,
                    None,
                    FixAction::GiveUp {
                        reason: GiveUp::Refused { refusal },
                    },
                ));
            }
        };
        let notice = |action| {
            missing(
                Some(candidate.package.clone()),
                candidate.trust.clone(),
                action,
            )
        };
        let action = autofix::decide(self.config.mode, &candidate);
        if action != Action::Report
            && let Err(reason) = s.attempts.check(&candidate.package)
        {
            s.done_run = d.run;
            return Step::Notify(notice(FixAction::GiveUp { reason }));
        }
        match action {
            Action::Report => {
                if s.reported.insert((d.run, d.module.clone())) {
                    Step::Notify(notice(FixAction::Report))
                } else {
                    Step::Nothing
                }
            }
            Action::Confirm => {
                s.done_run = d.run;
                let n = notice(FixAction::Confirm);
                s.pending = Some(candidate);
                Step::Notify(n)
            }
            Action::Install => {
                s.done_run = d.run;
                match s.attempts.begin(&candidate.package) {
                    Ok(attempt) => {
                        s.busy = true;
                        let n = notice(FixAction::Install);
                        Step::Install(candidate, attempt, n)
                    }
                    Err(reason) => Step::Notify(notice(FixAction::GiveUp { reason })),
                }
            }
        }
    }
}

async fn worker(
    inner: Weak<Inner>,
    owner: PortOwner,
    fix: Arc<FixState>,
    mut rx: mpsc::UnboundedReceiver<Detected>,
) {
    while let Some(d) = rx.recv().await {
        match fix.step(d) {
            Step::Nothing => {}
            Step::Notify(n) => notify(&inner, &owner, n),
            Step::Install(candidate, attempt, n) => {
                notify(&inner, &owner, n);
                apply(&inner, &owner, &fix, &candidate, attempt).await;
            }
        }
        fix.queued.fetch_sub(1, Ordering::SeqCst);
    }
}

fn notify(inner: &Weak<Inner>, owner: &PortOwner, notice: Notice) {
    if let Some(me) = inner.upgrade() {
        me.notify(owner, notice);
    }
}

/// Installs `candidate` (marked busy by the caller), then restarts the
/// instance if it is still the one `fix` belongs to. Clears busy.
pub(super) async fn apply(
    inner: &Weak<Inner>,
    owner: &PortOwner,
    fix: &Arc<FixState>,
    candidate: &Candidate,
    attempt: u32,
) {
    let mut cancelled = fix.cancel.subscribe();
    cancelled.borrow_and_update();
    let log = inner.upgrade().and_then(|me| {
        me.lock()
            .get(owner)
            .filter(|e| e.fix.as_ref().is_some_and(|f| Arc::ptr_eq(f, fix)))
            .and_then(|e| e.log.clone())
    });
    let marker = |text: &str| {
        if let Some(log) = &log {
            // A file error was reported when the log was opened or first written.
            let _ = log.marker(&format!("odhive: {text}"));
        }
    };
    notify(
        inner,
        owner,
        Notice::DependencyInstalling {
            module: candidate.module.clone(),
            package: candidate.package.clone(),
            requirements: candidate.requirements.clone(),
            attempt,
            max: MAX_ATTEMPTS,
        },
    );
    marker(&format!(
        "installing {} for missing module {:?} (attempt {attempt}/{MAX_ATTEMPTS})",
        candidate.package, candidate.module
    ));
    let plan = candidate.plan();
    let install = requirements::install(&plan, &fix.config.install, |e| {
        if let InstallEvent::Output { line } = e {
            marker(&line);
        }
    });
    let result = tokio::select! {
        r = install => Some(r),
        _ = cancelled.changed() => None,
    };
    let restart = match result {
        None => {
            marker("install cancelled");
            None
        }
        Some(Err(e)) => {
            let error = e.to_string();
            let hint = match e {
                requirements::RequirementsError::Failed { hint, .. } => hint.map(|h| *h),
                _ => None,
            };
            marker(&format!("install of {} failed", candidate.package));
            notify(
                inner,
                owner,
                Notice::DependencyFailed {
                    package: candidate.package.clone(),
                    error,
                    hint,
                },
            );
            None
        }
        Some(Ok(())) => {
            marker(&format!("installed {}; restarting", candidate.package));
            notify(
                inner,
                owner,
                Notice::DependencyInstalled {
                    package: candidate.package.clone(),
                },
            );
            inner.upgrade().and_then(|me| {
                me.lock()
                    .get(owner)
                    .filter(|e| e.fix.as_ref().is_some_and(|f| Arc::ptr_eq(f, fix)))
                    .and_then(|e| match &e.slot {
                        Slot::Supervised(h) => Some(h.clone()),
                        Slot::Attached(_) | Slot::Failed => None,
                    })
            })
        }
    };
    if let Some(handle) = restart {
        // A failed spawn is reported by the supervisor as `failed_to_start`.
        if let Err(e) = handle.restart().await {
            tracing::warn!(instance = %owner, error = %e, "restart after install failed");
        }
        drop(handle);
    }
    // Cleared after the restart: the new status is `starting` by then.
    fix.lock().busy = false;
}
