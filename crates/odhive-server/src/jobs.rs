//! Background jobs: work that takes too long for one request (cloning a
//! source, setting up a project's Python environment, creating a database).
//!
//! A request starts a job and gets its [`Job`] back at once. Progress is
//! published as [`JobEvent`]s (sent over the WebSocket) and kept on the job
//! itself: its steps, the last [`MAX_OUTPUT_LINES`] lines of tool output and,
//! once it ends, its result or error. Only one job per [`JobKind`] runs at a
//! time. Cancelling a job drops its work, which kills the tool it runs and
//! removes what it left half done.

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use odhive_core::config::SourceEntry;
use odhive_core::db::{CreateEvent, CreatedDatabase, RestoreEvent, RestoredDatabase};
use odhive_core::events::{Subscription, Topic};
use odhive_core::requirements::InstallEvent;
use odhive_core::setup::{EnvReport, SetupEvent};
use odhive_core::sources::CloneEvent;
use odhive_core::venv::VenvEvent;
use serde::Serialize;
use tokio::sync::{oneshot, watch};
use utoipa::ToSchema;

use crate::api::error::{ApiError, ErrorBody};

/// Tool output lines kept per job.
pub const MAX_OUTPUT_LINES: usize = 200;

/// Steps kept per job (the oldest go first).
pub const MAX_STEPS: usize = 200;

/// Ended jobs kept in memory (the oldest go first).
pub const MAX_FINISHED_JOBS: usize = 50;

/// Job events held per subscriber.
const JOB_EVENT_CAPACITY: usize = 1024;

/// What a job does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobKind {
    /// Shallow-clones Odoo Community and registers it.
    Clone {
        /// Odoo series, e.g. `19.0`.
        version: String,
    },
    /// Creates a project's venv and installs its requirements.
    Setup {
        /// Project name.
        project: String,
    },
    /// Creates an Odoo database for a project.
    CreateDatabase {
        /// Project name.
        project: String,
        /// Database name.
        database: String,
    },
    /// Restores an uploaded dump as a database of a project.
    RestoreDatabase {
        /// Project name.
        project: String,
        /// Database name.
        database: String,
    },
}

impl JobKind {
    /// The project whose database this job creates or restores.
    #[must_use]
    pub fn database_project(&self) -> Option<&str> {
        match self {
            Self::CreateDatabase { project, .. } | Self::RestoreDatabase { project, .. } => {
                Some(project)
            }
            _ => None,
        }
    }
}

/// Where a job stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// Still working.
    Running,
    /// Done; see `result`.
    Succeeded,
    /// Ended with an error; see `error`.
    Failed,
    /// Cancelled; what it had done is undone.
    Cancelled,
}

/// One step of a job's progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize, ToSchema)]
#[serde(tag = "job", rename_all = "snake_case")]
pub enum JobStep {
    /// Progress of a clone.
    Clone(CloneEvent),
    /// Progress of a project setup.
    Setup(SetupEvent),
    /// Progress of a database creation.
    CreateDatabase(CreateEvent),
    /// Progress of a database restore.
    RestoreDatabase(RestoreEvent),
}

/// What a successful job produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobResult {
    /// The cloned source, registered.
    Clone {
        /// The new source.
        source: SourceEntry,
    },
    /// The project's environment.
    Setup {
        /// What was set up.
        report: EnvReport,
    },
    /// The new database.
    CreateDatabase {
        /// What was created.
        database: CreatedDatabase,
    },
    /// The restored database.
    RestoreDatabase {
        /// What was restored.
        database: RestoredDatabase,
    },
}

/// A background job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize, ToSchema)]
pub struct Job {
    /// Id, unique while this service runs.
    pub id: u64,
    /// What it does.
    #[serde(flatten)]
    pub kind: JobKind,
    /// Where it stands.
    pub state: JobState,
    /// Start time, milliseconds since the Unix epoch.
    pub started_at: u64,
    /// End time, milliseconds since the Unix epoch.
    pub finished_at: Option<u64>,
    /// Progress steps, oldest first (tool output is in `output`).
    pub steps: Vec<JobStep>,
    /// The last lines of tool output.
    pub output: Vec<String>,
    /// Older output lines that are no longer kept.
    pub output_dropped: u64,
    /// Set when it succeeded.
    pub result: Option<JobResult>,
    /// Set when it failed.
    pub error: Option<ErrorBody>,
}

/// A change to a job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize, ToSchema)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum JobEvent {
    /// A job was started.
    Started {
        /// The job.
        job: Job,
    },
    /// A job made progress.
    Step {
        /// Job id.
        id: u64,
        /// The step.
        step: JobStep,
    },
    /// A job's tool printed a line.
    Output {
        /// Job id.
        id: u64,
        /// The line.
        line: String,
    },
    /// A job ended (any state but `running`).
    Finished {
        /// The job as it ended.
        job: Job,
    },
}

/// The work of a job.
pub(crate) type Work = Pin<Box<dyn Future<Output = Result<JobResult, ApiError>> + Send>>;

/// The running and recently ended jobs. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Jobs {
    inner: Arc<Mutex<State>>,
    topic: Topic<JobEvent>,
}

#[derive(Debug, Default)]
struct State {
    next_id: u64,
    jobs: BTreeMap<u64, Slot>,
    /// Ended jobs, oldest first.
    finished: VecDeque<u64>,
}

#[derive(Debug)]
struct Slot {
    job: Job,
    output: VecDeque<String>,
    cancel: Option<oneshot::Sender<()>>,
    /// Turns `true` when the job ended.
    done: watch::Receiver<bool>,
}

impl Slot {
    fn view(&self) -> Job {
        Job {
            output: self.output.iter().cloned().collect(),
            ..self.job.clone()
        }
    }
}

impl Default for Jobs {
    fn default() -> Self {
        Self::new()
    }
}

impl Jobs {
    /// No jobs yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(State::default())),
            topic: Topic::new(JOB_EVENT_CAPACITY),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // A panic while holding the lock leaves consistent data behind.
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Events of every job from now on.
    #[must_use]
    pub fn subscribe(&self) -> Subscription<JobEvent> {
        self.topic.subscribe()
    }

    /// Every job, oldest first.
    #[must_use]
    pub fn list(&self) -> Vec<Job> {
        self.state().jobs.values().map(Slot::view).collect()
    }

    /// Job `id`.
    #[must_use]
    pub fn get(&self, id: u64) -> Option<Job> {
        self.state().jobs.get(&id).map(Slot::view)
    }

    /// The running job of this kind.
    #[must_use]
    pub fn running(&self, kind: &JobKind) -> Option<Job> {
        self.state()
            .jobs
            .values()
            .find(|s| s.job.state == JobState::Running && &s.job.kind == kind)
            .map(Slot::view)
    }

    /// Starts a job of `kind`; `work` gets a [`Reporter`] for its progress.
    /// Must be called within a tokio runtime.
    ///
    /// # Errors
    /// `409 job_running` when a job of the same kind runs.
    pub(crate) fn spawn(
        &self,
        kind: JobKind,
        work: impl FnOnce(Reporter) -> Work,
    ) -> Result<Job, ApiError> {
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let (done_tx, done_rx) = watch::channel(false);
        let job = {
            let mut st = self.state();
            if let Some(other) = st
                .jobs
                .values()
                .find(|s| s.job.state == JobState::Running && s.job.kind == kind)
            {
                return Err(ApiError::conflict(
                    "job_running",
                    format!("job {} does this already", other.job.id),
                ));
            }
            st.next_id += 1;
            let id = st.next_id;
            let job = Job {
                id,
                kind,
                state: JobState::Running,
                started_at: now_ms(),
                finished_at: None,
                steps: Vec::new(),
                output: Vec::new(),
                output_dropped: 0,
                result: None,
                error: None,
            };
            st.jobs.insert(
                id,
                Slot {
                    job: job.clone(),
                    output: VecDeque::new(),
                    cancel: Some(cancel_tx),
                    done: done_rx,
                },
            );
            self.topic.publish(JobEvent::Started { job: job.clone() });
            job
        };
        let reporter = Reporter {
            jobs: self.clone(),
            id: job.id,
        };
        let fut = work(reporter);
        let jobs = self.clone();
        let id = job.id;
        tokio::spawn(async move {
            let outcome = tokio::select! {
                r = fut => Some(r),
                Ok(()) = cancel_rx => None,
            };
            // The work is dropped here: its cleanup ran.
            jobs.finish(id, outcome);
            done_tx.send_replace(true);
        });
        Ok(job)
    }

    fn finish(&self, id: u64, outcome: Option<Result<JobResult, ApiError>>) {
        let mut st = self.state();
        let Some(slot) = st.jobs.get_mut(&id) else {
            return;
        };
        slot.cancel = None;
        slot.job.finished_at = Some(now_ms());
        match outcome {
            Some(Ok(result)) => {
                slot.job.state = JobState::Succeeded;
                slot.job.result = Some(result);
            }
            Some(Err(e)) => {
                tracing::warn!(job = id, code = e.code, "job failed: {}", e.message);
                slot.job.state = JobState::Failed;
                slot.job.error = Some(e.body());
            }
            None => slot.job.state = JobState::Cancelled,
        }
        let job = slot.view();
        st.finished.push_back(id);
        while st.finished.len() > MAX_FINISHED_JOBS {
            if let Some(old) = st.finished.pop_front() {
                st.jobs.remove(&old);
            }
        }
        self.topic.publish(JobEvent::Finished { job });
    }

    /// Cancels job `id` and waits up to `wait` for it to end; returns the
    /// job as it is then. An ended job is returned as is.
    ///
    /// # Errors
    /// `404 unknown_job`.
    pub async fn cancel(&self, id: u64, wait: Duration) -> Result<Job, ApiError> {
        let mut done = {
            let mut st = self.state();
            let slot = st
                .jobs
                .get_mut(&id)
                .ok_or_else(|| ApiError::not_found("unknown_job", format!("no job {id}")))?;
            if let Some(tx) = slot.cancel.take() {
                let _ = tx.send(());
            }
            slot.done.clone()
        };
        let _ = tokio::time::timeout(wait, done.wait_for(|d| *d)).await;
        self.get(id)
            .ok_or_else(|| ApiError::not_found("unknown_job", format!("no job {id}")))
    }

    /// Cancels every running job and waits up to `wait` for them to end.
    pub async fn cancel_all(&self, wait: Duration) {
        let running: Vec<u64> = self
            .state()
            .jobs
            .values()
            .filter(|s| s.job.state == JobState::Running)
            .map(|s| s.job.id)
            .collect();
        let cancels = running.into_iter().map(|id| self.cancel(id, wait));
        for c in cancels {
            let _ = c.await;
        }
    }

    fn report(&self, id: u64, step: JobStep) {
        let mut st = self.state();
        let Some(slot) = st.jobs.get_mut(&id) else {
            return;
        };
        if slot.job.state != JobState::Running {
            return;
        }
        if let Some(line) = output_line(&step) {
            let line = line.to_owned();
            if slot.output.len() >= MAX_OUTPUT_LINES {
                slot.output.pop_front();
                slot.job.output_dropped += 1;
            }
            slot.output.push_back(line.clone());
            self.topic.publish(JobEvent::Output { id, line });
            return;
        }
        let steps = &mut slot.job.steps;
        // git prints progress many times per phase, Odoo once per module:
        // keep the latest only.
        let replaces = matches!(
            (steps.last(), &step),
            (
                Some(JobStep::Clone(CloneEvent::Progress { phase: a, .. })),
                JobStep::Clone(CloneEvent::Progress { phase: b, .. }),
            ) if a == b
        ) || matches!(
            (steps.last(), &step),
            (
                Some(JobStep::CreateDatabase(CreateEvent::Module { .. })),
                JobStep::CreateDatabase(CreateEvent::Module { .. }),
            )
        );
        if replaces {
            steps.pop();
        } else if steps.len() >= MAX_STEPS {
            steps.remove(0);
        }
        steps.push(step.clone());
        self.topic.publish(JobEvent::Step { id, step });
    }
}

fn output_line(step: &JobStep) -> Option<&str> {
    match step {
        JobStep::Setup(
            SetupEvent::Venv(VenvEvent::Output { line })
            | SetupEvent::Install(InstallEvent::Output { line }),
        )
        | JobStep::CreateDatabase(CreateEvent::Output { line })
        | JobStep::RestoreDatabase(RestoreEvent::Output { line }) => Some(line),
        _ => None,
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Reports the progress of one job.
#[derive(Debug, Clone)]
pub(crate) struct Reporter {
    jobs: Jobs,
    id: u64,
}

impl Reporter {
    /// Records `step` and publishes it.
    pub(crate) fn step(&self, step: JobStep) {
        self.jobs.report(self.id, step);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use odhive_core::events::Delivery;

    fn setup_kind(p: &str) -> JobKind {
        JobKind::Setup { project: p.into() }
    }

    fn line(l: &str) -> JobStep {
        JobStep::Setup(SetupEvent::Install(InstallEvent::Output { line: l.into() }))
    }

    fn progress(phase: &str, percent: u8) -> JobStep {
        JobStep::Clone(CloneEvent::Progress {
            phase: phase.into(),
            percent: Some(percent),
        })
    }

    async fn wait_done(jobs: &Jobs, id: u64) -> Job {
        for _ in 0..200 {
            let job = jobs.get(id).expect("job");
            if job.state != JobState::Running {
                return job;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("job {id} did not end");
    }

    #[tokio::test]
    async fn a_job_reports_steps_and_output_then_its_result() {
        let jobs = Jobs::new();
        let mut sub = jobs.subscribe();
        let (go_tx, go_rx) = oneshot::channel::<()>();
        let job = jobs
            .spawn(setup_kind("shop"), |r| {
                Box::pin(async move {
                    for i in 0..(MAX_OUTPUT_LINES + 5) {
                        r.step(line(&format!("line {i}")));
                    }
                    r.step(progress("Receiving objects", 10));
                    r.step(progress("Receiving objects", 90));
                    r.step(progress("Resolving deltas", 50));
                    let _ = go_rx.await;
                    Ok(JobResult::Setup {
                        report: EnvReport {
                            base: None,
                            python: "/p/.venv/bin/python".into(),
                            installed: 0,
                        },
                    })
                })
            })
            .expect("spawn");
        assert_eq!(job.state, JobState::Running);

        // One job per kind; another kind is fine.
        let busy = jobs
            .spawn(setup_kind("shop"), |_| Box::pin(async { unreachable!() }))
            .expect_err("busy");
        assert_eq!(busy.code, "job_running");
        assert_eq!(
            jobs.running(&setup_kind("shop")).map(|j| j.id),
            Some(job.id)
        );

        tokio::time::sleep(Duration::from_millis(50)).await;
        let now = jobs.get(job.id).expect("job");
        assert_eq!(now.output.len(), MAX_OUTPUT_LINES);
        assert_eq!(now.output_dropped, 5);
        assert_eq!(now.output.first().map(String::as_str), Some("line 5"));
        assert_eq!(
            now.steps,
            vec![
                progress("Receiving objects", 90),
                progress("Resolving deltas", 50)
            ],
            "repeated progress of a phase is kept once"
        );

        go_tx.send(()).expect("go");
        let done = wait_done(&jobs, job.id).await;
        assert_eq!(done.state, JobState::Succeeded);
        assert!(done.result.is_some() && done.finished_at.is_some());

        let mut kinds = Vec::new();
        while let Ok(Delivery::Event(e)) = sub.try_recv() {
            kinds.push(match e {
                JobEvent::Started { .. } => "started",
                JobEvent::Step { .. } => "step",
                JobEvent::Output { .. } => "output",
                JobEvent::Finished { .. } => "finished",
            });
        }
        assert_eq!(kinds.first(), Some(&"started"));
        assert_eq!(kinds.last(), Some(&"finished"));
        assert_eq!(kinds.iter().filter(|k| **k == "step").count(), 3);
    }

    #[tokio::test]
    async fn database_creation_keeps_the_latest_module_step() {
        let jobs = Jobs::new();
        let kind = JobKind::CreateDatabase {
            project: "shop".into(),
            database: "shop_main".into(),
        };
        let (go_tx, go_rx) = oneshot::channel::<()>();
        let job = jobs
            .spawn(kind, |r| {
                Box::pin(async move {
                    let step = |s| JobStep::CreateDatabase(s);
                    r.step(step(CreateEvent::Step {
                        step: odhive_core::db::CreateStep::InstallModules,
                    }));
                    for index in 1..=3 {
                        r.step(step(CreateEvent::Module {
                            name: format!("m{index}"),
                            index,
                            total: 3,
                        }));
                        r.step(step(CreateEvent::Output {
                            line: format!("loading m{index}"),
                        }));
                    }
                    let _ = go_rx.await;
                    Err(ApiError::internal("stop"))
                })
            })
            .expect("spawn");
        tokio::time::sleep(Duration::from_millis(50)).await;
        let now = jobs.get(job.id).expect("job");
        assert_eq!(now.steps.len(), 2, "{:?}", now.steps);
        assert!(matches!(
            now.steps.last(),
            Some(JobStep::CreateDatabase(CreateEvent::Module {
                index: 3,
                ..
            }))
        ));
        assert_eq!(now.output.len(), 3);
        let _ = go_tx.send(());
        wait_done(&jobs, job.id).await;
    }

    struct Dropped(Arc<Mutex<bool>>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            *self.0.lock().expect("lock") = true;
        }
    }

    #[tokio::test]
    async fn cancel_drops_the_work_and_failures_keep_their_error() {
        let jobs = Jobs::new();
        let dropped = Arc::new(Mutex::new(false));
        let flag = Dropped(dropped.clone());
        let job = jobs
            .spawn(setup_kind("shop"), move |_| {
                Box::pin(async move {
                    let _flag = flag;
                    std::future::pending::<()>().await;
                    unreachable!()
                })
            })
            .expect("spawn");
        let cancelled = jobs
            .cancel(job.id, Duration::from_secs(5))
            .await
            .expect("cancel");
        assert_eq!(cancelled.state, JobState::Cancelled);
        assert!(*dropped.lock().expect("lock"), "cleanup ran");
        // Cancelling again returns the ended job.
        let again = jobs.cancel(job.id, Duration::from_secs(1)).await;
        assert_eq!(again.map(|j| j.state), Ok(JobState::Cancelled));
        assert_eq!(
            jobs.cancel(999, Duration::ZERO).await.map_err(|e| e.code),
            Err("unknown_job")
        );

        let failed = jobs
            .spawn(setup_kind("shop"), |_| {
                Box::pin(async { Err(ApiError::invalid("python", "no Python")) })
            })
            .expect("spawn after cancel");
        let failed = wait_done(&jobs, failed.id).await;
        assert_eq!(failed.state, JobState::Failed);
        assert_eq!(failed.error.map(|e| e.error), Some("python".to_owned()));
    }

    #[tokio::test]
    async fn only_the_newest_ended_jobs_are_kept() {
        let jobs = Jobs::new();
        let mut last = 0;
        for i in 0..(MAX_FINISHED_JOBS + 3) {
            let job = jobs
                .spawn(setup_kind(&format!("p{i}")), |_| {
                    Box::pin(async { Err(ApiError::internal("x")) })
                })
                .expect("spawn");
            wait_done(&jobs, job.id).await;
            last = job.id;
        }
        let list = jobs.list();
        assert_eq!(list.len(), MAX_FINISHED_JOBS);
        assert_eq!(list.last().map(|j| j.id), Some(last));
        assert!(jobs.get(1).is_none());
    }
}
