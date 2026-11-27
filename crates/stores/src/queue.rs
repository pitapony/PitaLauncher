//! The serial install queue the non-Steam stores share.
//!
//! Epic, Amazon and GOG install through external CLIs. Two installs at once
//! fight over the same login session, the same `~/.config` caches and the same
//! link, and a cancelled one can leave both halves of a download behind — so
//! PitaGames runs them one at a time. This module is that ordering: a list of
//! [`InstallJob`]s a driver moves through `Queued → Running → Done | Failed |
//! Cancelled`, with a disk-space pre-check before a job starts.
//!
//! It is deliberately pure — no threads, no I/O, no store backend. The driver
//! (the GUI's worker or a CLI loop) spawns the backend, forwards progress, and
//! reports the outcome back with [`InstallQueue::finish`]. The disk figure is
//! injected as a closure returning the free bytes on the target's filesystem, so
//! this module never has to depend on `platform`; the real value comes from
//! `platform::disk::space`.

use std::path::{Path, PathBuf};

use pitagames_core::GameId;

/// What a queued job will do.
///
/// An update is not a second path: it is an install of an **existing** game
/// followed by a verify, so it rides the same queue and the same worker thread
/// as a fresh install. The only difference is the second command
/// [`crate::installer::run`] runs, which is why the kind travels on the job
/// rather than living in a separate code path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum JobKind {
    /// Fetch and lay down a game's files (a fresh install or a repair).
    #[default]
    Install,
    /// Install an existing game, then verify the files it just wrote.
    Update,
    /// Re-check the files an interrupted install left behind, without
    /// re-downloading: the store's own `verify` (legendary/nile) or `repair`
    /// (gogdl). This is the "Verify" a reconciled row offers, and it is the
    /// store's to run — the launcher never deletes a partial download itself.
    Verify,
}

impl JobKind {
    /// A short word for a log line or a row.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Update => "update",
            Self::Verify => "verify",
        }
    }

    /// The token persisted with the queue; stable across releases.
    pub const fn wire(self) -> &'static str {
        self.label()
    }

    /// The kind a persisted token names, or `None` for an unknown token.
    #[must_use]
    pub fn from_wire(token: &str) -> Option<Self> {
        match token {
            "install" => Some(Self::Install),
            "update" => Some(Self::Update),
            "verify" => Some(Self::Verify),
            _ => None,
        }
    }

    /// Whether the run lays files down before (or instead of) verifying.
    pub const fn installs(self) -> bool {
        matches!(self, Self::Install | Self::Update)
    }

    /// Whether the run checks the files after (or instead of) the install.
    pub const fn verifies(self) -> bool {
        matches!(self, Self::Update | Self::Verify)
    }
}

/// What a queued install needs.
#[derive(Debug, Clone, PartialEq)]
pub struct InstallJob {
    pub id: GameId,
    pub title: String,
    /// The directory the store will fill (`games/<store>/<title>` by default).
    pub target: PathBuf,
    /// The size the store reports for the game, when it reports one.
    pub size: Option<u64>,
    /// Whether this is a fresh install, an install-then-verify update, or a
    /// verify-only repair.
    pub kind: JobKind,
    pub state: JobState,
    pub progress: JobProgress,
    /// The store CLI's process-group id while the job runs.
    ///
    /// Recorded so a launcher that crashes mid-install can reap the orphaned
    /// CLI at its next boot; cleared when the job stops running.
    pub pid: Option<u32>,
}

impl InstallJob {
    /// A job for `id`/`title` landing in `target`.
    pub fn new(id: GameId, title: impl Into<String>, target: impl Into<PathBuf>) -> Self {
        Self {
            id,
            title: title.into(),
            target: target.into(),
            size: None,
            kind: JobKind::Install,
            state: JobState::Queued,
            progress: JobProgress::default(),
            pid: None,
        }
    }

    /// The same, with the store's reported download size.
    #[must_use]
    pub fn with_size(mut self, size: u64) -> Self {
        self.size = Some(size);
        self
    }

    /// The same, with the CLI's process-group id (a restored or running job).
    #[must_use]
    pub fn with_pid(mut self, pid: u32) -> Self {
        self.pid = Some(pid);
        self
    }

    /// The same, with an explicit kind (a restored job).
    #[must_use]
    pub fn with_kind(mut self, kind: JobKind) -> Self {
        self.kind = kind;
        self
    }

    /// The same, with an explicit state (a restored job).
    #[must_use]
    pub fn with_state(mut self, state: JobState) -> Self {
        self.state = state;
        self
    }

    /// The same, with progress already reported (a restored job).
    #[must_use]
    pub fn with_progress(mut self, progress: JobProgress) -> Self {
        self.progress = progress;
        self
    }

    /// The same, as an update: install the existing game, then verify it.
    ///
    /// The target is unchanged — an update fills the directory the game is
    /// already installed in, never the store's default for a new install.
    #[must_use]
    pub fn as_update(mut self) -> Self {
        self.kind = JobKind::Update;
        self
    }

    /// The directory the space check runs against.
    ///
    /// The install directory does not exist yet, so the check resolves its
    /// nearest existing parent — the filesystem the game will actually land on.
    fn space_root(&self) -> PathBuf {
        let mut probe = self.target.as_path();
        while let Some(parent) = probe.parent() {
            if parent.as_os_str().is_empty() {
                break;
            }
            if parent.exists() {
                return parent.to_path_buf();
            }
            probe = parent;
        }
        self.target.clone()
    }
}

/// Where one job stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobState {
    /// Waiting for its turn.
    Queued,
    /// The backend is running it.
    Running,
    /// Finished successfully.
    Done,
    /// The pre-check refused it, or the backend reported a failure.
    Failed { message: String },
    /// The user cancelled it (or the backend did).
    Cancelled,
}

impl JobState {
    /// Neither running nor waiting; no further transition will happen on its own.
    pub const fn is_finished(&self) -> bool {
        matches!(self, Self::Done | Self::Failed { .. } | Self::Cancelled)
    }

    /// Queued or running: the store still has work (or will get it) for this id.
    pub const fn is_active(&self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }

    /// Short status word for a queue row.
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "installing",
            Self::Done => "installed",
            Self::Failed { .. } => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// The token persisted with the queue: the state *kind*, without a failure
    /// message (that travels in its own column).
    pub const fn wire(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed { .. } => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// The state a persisted token names, with the stored failure message.
    ///
    /// An unknown token is `None`; a `failed` token without a message gets a
    /// placeholder so the row still renders.
    #[must_use]
    pub fn from_wire(token: &str, message: Option<String>) -> Option<Self> {
        match token {
            "queued" => Some(Self::Queued),
            "running" => Some(Self::Running),
            "done" => Some(Self::Done),
            "failed" => Some(Self::Failed {
                message: message.unwrap_or_else(|| "the install failed".to_string()),
            }),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

/// The latest progress reported for a running job.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct JobProgress {
    /// Percent complete (`0.0..=100.0`), when the CLI reports one.
    pub percent: Option<f32>,
    /// The last line/stage the CLI printed, for the downloads row.
    pub stage: Option<String>,
}

impl JobProgress {
    /// Replace the percentage, clamping a stray value into `0..=100`.
    pub fn set_percent(&mut self, percent: f32) {
        self.percent = Some(percent.clamp(0.0, 100.0));
    }

    pub fn clear(&mut self) {
        self.percent = None;
        self.stage = None;
    }
}

/// Why a job could not be added.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum QueueError {
    /// A job for the same game is already queued or running. The GUI would
    /// otherwise start a second download of the same title.
    #[error("an install for this game is already queued or running")]
    Duplicate,
}

/// What [`InstallQueue::start_next`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartOutcome {
    /// Nothing is queued.
    Empty,
    /// A job is already running; the driver must finish it first.
    Busy,
    /// The head job is now `Running`; the driver should start the backend.
    Started { index: usize },
    /// The head job was refused by the space check and is now `Failed`.
    ///
    /// `available` and `required` let a caller explain the shortfall without
    /// re-reading the job's state.
    Skipped {
        index: usize,
        available: u64,
        required: u64,
    },
}

/// An install the launcher stopped owning when it crashed or was closed.
///
/// Reported by [`InstallQueue::reconcile_running`] so the next boot can reap the
/// store CLI the interrupted run left behind. `pid` is `None` when the run had
/// not recorded one yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interrupted {
    pub id: GameId,
    pub pid: Option<u32>,
}

/// The queued installs, in the order they were added.
#[derive(Debug, Default, Clone)]
pub struct InstallQueue {
    jobs: Vec<InstallJob>,
}

impl InstallQueue {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuild a queue from persisted jobs, keeping their order.
    ///
    /// No validation: the rows were written from a live queue, so the
    /// no-duplicate-active-id invariant already held. Callers that want to be
    /// sure run [`reconcile_running`](Self::reconcile_running) first.
    #[must_use]
    pub fn from_jobs(jobs: Vec<InstallJob>) -> Self {
        Self { jobs }
    }

    /// A copy of every job, for persistence.
    #[must_use]
    pub fn snapshot(&self) -> Vec<InstallJob> {
        self.jobs.clone()
    }

    /// Turn every `Running` job into `Failed { message }` and report what it
    /// left behind.
    ///
    /// A `Running` job's process does not belong to this launcher any more (the
    /// one that spawned it is gone), so it can never complete: it surfaces as a
    /// failure with Retry and Verify offered. A `Queued` job never started, so
    /// it stays queued — a restart must not lose it. A finished job is
    /// untouched.
    ///
    /// Nothing here touches the filesystem. Whatever the interrupted store had
    /// already written is left for the store's own repair (the Verify job), not
    /// deleted: a partial download is the store's to reconcile.
    pub fn reconcile_running(&mut self, message: &str) -> Vec<Interrupted> {
        let mut interrupted = Vec::new();
        for job in &mut self.jobs {
            if job.state != JobState::Running {
                continue;
            }
            job.state = JobState::Failed {
                message: message.to_string(),
            };
            job.progress.clear();
            interrupted.push(Interrupted {
                id: job.id.clone(),
                pid: job.pid.take(),
            });
        }
        interrupted
    }

    /// Add a job, unless the same game is already queued or running.
    ///
    /// Returns the job's index. A finished job for the same game does **not**
    /// block a re-install; the driver removes it (or the queue is cleared).
    pub fn enqueue(&mut self, job: InstallJob) -> Result<usize, QueueError> {
        if self
            .jobs
            .iter()
            .any(|existing| existing.id == job.id && existing.state.is_active())
        {
            return Err(QueueError::Duplicate);
        }
        self.jobs.push(job);
        Ok(self.jobs.len() - 1)
    }

    pub fn jobs(&self) -> &[InstallJob] {
        &self.jobs
    }

    pub fn len(&self) -> usize {
        self.jobs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }

    pub fn get(&self, id: &GameId) -> Option<&InstallJob> {
        self.jobs.iter().find(|job| &job.id == id)
    }

    /// The index of the job the backend is running, if any.
    #[must_use]
    pub fn running(&self) -> Option<usize> {
        self.jobs
            .iter()
            .position(|job| job.state == JobState::Running)
    }

    /// The index of the first job still waiting.
    #[must_use]
    pub fn next_queued(&self) -> Option<usize> {
        self.jobs
            .iter()
            .position(|job| job.state == JobState::Queued)
    }

    pub fn active_count(&self) -> usize {
        self.jobs.iter().filter(|job| job.state.is_active()).count()
    }

    /// Start the first queued job, after a disk-space pre-check.
    ///
    /// `free_space` is asked for the free bytes on the filesystem that would
    /// hold the job, given the job's nearest existing parent directory. It
    /// returns `None` when the answer is unknown (no store size, an unsupported
    /// filesystem), in which case the check is skipped rather than guessed —
    /// a wrong refusal is worse than a late failure. A job with a known size and
    /// too little room is marked `Failed` and reported as [`StartOutcome::Skipped`]
    /// so the caller can move on to the next one.
    pub fn start_next<F>(&mut self, free_space: F) -> StartOutcome
    where
        F: FnOnce(&Path) -> Option<u64>,
    {
        if self.running().is_some() {
            return StartOutcome::Busy;
        }
        let Some(index) = self.next_queued() else {
            return StartOutcome::Empty;
        };

        if let Some(required) = self.jobs[index].size
            && let Some(available) = free_space(&self.jobs[index].space_root())
            && available < required
        {
            self.jobs[index].state = JobState::Failed {
                message: format!(
                    "not enough free space: need {required} bytes, {available} available"
                ),
            };
            return StartOutcome::Skipped {
                index,
                available,
                required,
            };
        }

        self.jobs[index].state = JobState::Running;
        self.jobs[index].progress.clear();
        // A fresh run has no pid yet; the worker records its own once the CLI
        // is up (`set_pid`).
        self.jobs[index].pid = None;
        StartOutcome::Started { index }
    }

    /// Record the CLI's process-group id for a running job.
    ///
    /// Returns `false` for a job that is not running, so a late report from a
    /// worker whose job already finished cannot attach a stale pid.
    pub fn set_pid(&mut self, index: usize, pid: u32) -> bool {
        let Some(job) = self.jobs.get_mut(index) else {
            return false;
        };
        if job.state != JobState::Running {
            return false;
        }
        job.pid = Some(pid);
        true
    }

    /// Record the outcome of the running job at `index`.
    ///
    /// Returns `false` when the job is not running (already finished, or a stale
    /// index), so a late report from a cancelled backend cannot resurrect a row.
    pub fn finish(&mut self, index: usize, result: Result<(), String>) -> bool {
        let Some(job) = self.jobs.get_mut(index) else {
            return false;
        };
        if job.state != JobState::Running {
            return false;
        }
        job.progress.clear();
        job.pid = None;
        job.state = match result {
            Ok(()) => JobState::Done,
            Err(message) => JobState::Failed { message },
        };
        true
    }

    /// Record progress for the running job at `index` (ignored otherwise).
    pub fn progress(&mut self, index: usize, percent: Option<f32>, stage: Option<String>) -> bool {
        let Some(job) = self.jobs.get_mut(index) else {
            return false;
        };
        if job.state != JobState::Running {
            return false;
        }
        if let Some(percent) = percent {
            job.progress.set_percent(percent);
        }
        if stage.is_some() {
            job.progress.stage = stage;
        }
        true
    }

    /// Cancel the job at `index`. A `Running` job is marked `Cancelled`; the
    /// caller is responsible for stopping the backend it started.
    pub fn cancel(&mut self, index: usize) -> bool {
        let Some(job) = self.jobs.get_mut(index) else {
            return false;
        };
        if job.state.is_finished() {
            return false;
        }
        job.progress.clear();
        // The caller stops the backend it started; the pid is no longer ours.
        job.pid = None;
        job.state = JobState::Cancelled;
        true
    }

    /// Put a finished (failed or cancelled) job back in the queue.
    pub fn retry(&mut self, index: usize) -> bool {
        let Some(job) = self.jobs.get_mut(index) else {
            return false;
        };
        if !job.state.is_finished() {
            return false;
        }
        job.state = JobState::Queued;
        job.progress.clear();
        job.pid = None;
        true
    }

    /// Turn a finished job into a **verify-only** repair and queue it.
    ///
    /// This is the "Verify" a reconciled row offers after a crash: the store's
    /// own check over whatever the interrupted install left, without fetching
    /// anything again. The target is kept, so the check runs against the
    /// directory the user saw — and a partial download is never deleted, only
    /// repaired by the store. Only a finished job can be re-queued.
    pub fn verify(&mut self, index: usize) -> bool {
        let Some(job) = self.jobs.get_mut(index) else {
            return false;
        };
        if !job.state.is_finished() {
            return false;
        }
        job.kind = JobKind::Verify;
        job.state = JobState::Queued;
        job.progress.clear();
        job.pid = None;
        true
    }

    /// Drop a job. A running job must be [`cancel`](Self::cancel)led first.
    pub fn remove(&mut self, index: usize) -> bool {
        match self.jobs.get(index) {
            Some(job) if job.state != JobState::Running => {
                self.jobs.remove(index);
                true
            }
            _ => false,
        }
    }

    /// Drop every finished job, returning how many were removed.
    pub fn clear_finished(&mut self) -> usize {
        let before = self.jobs.len();
        self.jobs.retain(|job| !job.state.is_finished());
        before - self.jobs.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pitagames_core::Store;

    const GIB: u64 = 1024 * 1024 * 1024;

    fn job(id: &str, title: &str) -> InstallJob {
        InstallJob::new(
            GameId::new(Store::Epic, id),
            title,
            PathBuf::from("/games/epic").join(title),
        )
    }

    /// A probe that reports a fixed amount of free space.
    fn free(bytes: u64) -> impl FnOnce(&Path) -> Option<u64> {
        move |_| Some(bytes)
    }

    #[test]
    fn a_fresh_job_starts_queued() {
        let job = job("Gizmo", "Gizmo");
        assert_eq!(job.state, JobState::Queued);
        assert!(job.state.is_active() && !job.state.is_finished());
        assert_eq!(job.state.label(), "queued");
    }

    #[test]
    fn a_new_job_is_a_plain_install() {
        let job = job("Gizmo", "Gizmo");
        assert_eq!(job.kind, JobKind::Install);
        assert!(!job.kind.verifies());
        assert_eq!(job.kind.label(), "install");
    }

    #[test]
    fn an_update_job_keeps_its_target_and_verifies() {
        let job = job("Gizmo", "Gizmo").as_update();
        assert_eq!(job.kind, JobKind::Update);
        assert!(job.kind.verifies());
        assert_eq!(job.kind.label(), "update");
        // The target is the existing install, not the store's default.
        assert_eq!(job.target, PathBuf::from("/games/epic/Gizmo"));
        assert_eq!(job.state, JobState::Queued);
    }

    #[test]
    fn enqueue_refuses_a_duplicate_active_id() {
        let mut queue = InstallQueue::new();
        assert_eq!(queue.enqueue(job("Gizmo", "Gizmo")).unwrap(), 0);
        assert_eq!(
            queue.enqueue(job("Gizmo", "Gizmo")),
            Err(QueueError::Duplicate)
        );
        assert_eq!(queue.len(), 1);

        // A different game is fine.
        assert_eq!(queue.enqueue(job("Other", "Other")).unwrap(), 1);
    }

    #[test]
    fn a_finished_job_does_not_block_a_reinstall() {
        let mut queue = InstallQueue::new();
        queue.enqueue(job("Gizmo", "Gizmo")).unwrap();
        queue.start_next(free(10 * GIB));
        assert!(queue.finish(0, Ok(())));
        assert_eq!(queue.jobs()[0].state, JobState::Done);

        assert_eq!(queue.enqueue(job("Gizmo", "Gizmo")).unwrap(), 1);
    }

    #[test]
    fn start_next_runs_one_job_at_a_time() {
        let mut queue = InstallQueue::new();
        queue.enqueue(job("A", "A")).unwrap();
        queue.enqueue(job("B", "B")).unwrap();

        assert_eq!(
            queue.start_next(free(10 * GIB)),
            StartOutcome::Started { index: 0 }
        );
        assert_eq!(queue.running(), Some(0));
        // While 0 runs, the queue is busy.
        assert_eq!(queue.start_next(free(10 * GIB)), StartOutcome::Busy);

        assert!(queue.finish(0, Ok(())));
        assert_eq!(
            queue.start_next(free(10 * GIB)),
            StartOutcome::Started { index: 1 }
        );
        assert_eq!(queue.active_count(), 1);
    }

    #[test]
    fn an_empty_queue_starts_nothing() {
        let mut queue = InstallQueue::new();
        assert_eq!(queue.start_next(free(0)), StartOutcome::Empty);
    }

    #[test]
    fn a_short_on_space_job_is_skipped_and_fails() {
        let mut queue = InstallQueue::new();
        queue
            .enqueue(job("Big", "Big").with_size(50 * GIB))
            .unwrap();
        queue.enqueue(job("Small", "Small").with_size(GIB)).unwrap();

        assert_eq!(
            queue.start_next(free(10 * GIB)),
            StartOutcome::Skipped {
                index: 0,
                available: 10 * GIB,
                required: 50 * GIB,
            }
        );
        assert!(matches!(queue.jobs()[0].state, JobState::Failed { .. }));
        assert_eq!(queue.jobs()[0].state.label(), "failed");

        // The driver can move on to the next job.
        assert_eq!(
            queue.start_next(free(10 * GIB)),
            StartOutcome::Started { index: 1 }
        );
    }

    #[test]
    fn an_unknown_free_space_skips_the_check() {
        let mut queue = InstallQueue::new();
        queue
            .enqueue(job("Big", "Big").with_size(50 * GIB))
            .unwrap();

        assert_eq!(
            queue.start_next(|_| None),
            StartOutcome::Started { index: 0 },
            "an unknown answer must not refuse the install"
        );
    }

    #[test]
    fn a_job_without_a_size_is_never_refused() {
        let mut queue = InstallQueue::new();
        queue.enqueue(job("Mystery", "Mystery")).unwrap();
        assert_eq!(
            queue.start_next(free(0)),
            StartOutcome::Started { index: 0 }
        );
    }

    #[test]
    fn the_space_check_targets_the_nearest_existing_parent() {
        let root = tempfile::tempdir().unwrap();
        let deep = root.path().join("games/epic/Gizmo");
        let mut queue = InstallQueue::new();
        queue
            .enqueue(InstallJob::new(GameId::new(Store::Epic, "Gizmo"), "Gizmo", deep).with_size(1))
            .unwrap();

        let mut probed: Option<PathBuf> = None;
        queue.start_next(|path| {
            probed = Some(path.to_path_buf());
            Some(u64::MAX)
        });

        assert_eq!(
            probed.as_deref(),
            Some(root.path()),
            "the target does not exist yet"
        );
    }

    #[test]
    fn finish_only_applies_to_a_running_job() {
        let mut queue = InstallQueue::new();
        queue.enqueue(job("A", "A")).unwrap();

        // A queued job is not running yet.
        assert!(!queue.finish(0, Ok(())));
        queue.start_next(free(GIB));
        assert!(queue.finish(0, Ok(())));
        // A late second report is ignored.
        assert!(!queue.finish(0, Err("late".into())));
        assert_eq!(queue.jobs()[0].state, JobState::Done);

        // Out-of-range index is false, not a panic.
        assert!(!queue.finish(9, Ok(())));
    }

    #[test]
    fn a_failure_keeps_its_message() {
        let mut queue = InstallQueue::new();
        queue.enqueue(job("A", "A")).unwrap();
        queue.start_next(free(GIB));
        assert!(queue.finish(0, Err("legendary exited 1".into())));
        assert_eq!(
            queue.jobs()[0].state,
            JobState::Failed {
                message: "legendary exited 1".into()
            }
        );
    }

    #[test]
    fn progress_is_recorded_only_while_running() {
        let mut queue = InstallQueue::new();
        queue.enqueue(job("A", "A")).unwrap();
        assert!(!queue.progress(0, Some(10.0), None), "still queued");

        queue.start_next(free(GIB));
        assert!(queue.progress(0, Some(42.5), Some("downloading".into())));
        assert_eq!(queue.jobs()[0].progress.percent, Some(42.5));
        assert_eq!(
            queue.jobs()[0].progress.stage.as_deref(),
            Some("downloading")
        );

        // Percent is clamped, and the stage survives when a later event has none.
        assert!(queue.progress(0, Some(150.0), None));
        assert_eq!(queue.jobs()[0].progress.percent, Some(100.0));
        assert_eq!(
            queue.jobs()[0].progress.stage.as_deref(),
            Some("downloading")
        );

        queue.finish(0, Ok(()));
        assert_eq!(queue.jobs()[0].progress, JobProgress::default());
        assert!(!queue.progress(0, Some(50.0), None), "finished");
    }

    #[test]
    fn cancel_and_retry_round_trip() {
        let mut queue = InstallQueue::new();
        queue.enqueue(job("A", "A")).unwrap();
        queue.enqueue(job("B", "B")).unwrap();

        assert!(queue.cancel(0));
        assert_eq!(queue.jobs()[0].state, JobState::Cancelled);
        assert!(
            !queue.cancel(0),
            "a cancelled job cannot be cancelled again"
        );
        assert!(queue.jobs()[0].state.is_finished());

        assert!(queue.retry(0));
        assert_eq!(queue.jobs()[0].state, JobState::Queued);
        assert!(!queue.retry(1), "a queued job is not retryable");

        // Cancelling the running job stops it being active.
        queue.cancel(1);
        assert_eq!(queue.active_count(), 1);
    }

    #[test]
    fn remove_refuses_a_running_job() {
        let mut queue = InstallQueue::new();
        queue.enqueue(job("A", "A")).unwrap();
        queue.enqueue(job("B", "B")).unwrap();
        queue.start_next(free(GIB));

        assert!(!queue.remove(0), "a running job must be cancelled first");
        assert!(queue.remove(1), "a queued job can be dropped");
        assert_eq!(queue.len(), 1);
        assert!(!queue.remove(5));
    }

    // -- crash recovery: a restored queue -------------------------------------

    #[test]
    fn a_verify_job_checks_without_installing() {
        let job = job("Gizmo", "Gizmo").with_kind(JobKind::Verify);
        assert_eq!(job.kind.label(), "verify");
        assert!(!job.kind.installs());
        assert!(job.kind.verifies(), "a verify job still verifies");

        // The other two keep their install/verify split.
        assert!(JobKind::Install.installs() && !JobKind::Install.verifies());
        assert!(JobKind::Update.installs() && JobKind::Update.verifies());
    }

    #[test]
    fn the_kind_and_state_wire_tokens_round_trip() {
        for kind in [JobKind::Install, JobKind::Update, JobKind::Verify] {
            assert_eq!(JobKind::from_wire(kind.wire()), Some(kind));
        }
        assert_eq!(JobKind::from_wire("bogus"), None);

        for state in [
            JobState::Queued,
            JobState::Running,
            JobState::Done,
            JobState::Cancelled,
        ] {
            assert_eq!(JobState::from_wire(state.wire(), None), Some(state));
        }
        assert_eq!(
            JobState::from_wire("failed", Some("boom".into())),
            Some(JobState::Failed {
                message: "boom".into()
            })
        );
        assert!(matches!(
            JobState::from_wire("failed", None),
            Some(JobState::Failed { .. })
        ));
        assert_eq!(JobState::from_wire("bogus", None), None);
    }

    #[test]
    fn a_restored_queue_keeps_its_order_and_shape() {
        let persisted = vec![
            job("A", "A").with_state(JobState::Queued),
            job("B", "B").with_state(JobState::Done),
            job("C", "C").with_state(JobState::Queued),
        ];
        let queue = InstallQueue::from_jobs(persisted.clone());
        assert_eq!(queue.snapshot(), persisted);
        assert_eq!(queue.len(), 3);
    }

    #[test]
    fn reconcile_turns_a_running_job_into_a_failed_one_and_reports_its_pid() {
        let mut queue = InstallQueue::from_jobs(vec![
            job("Gizmo", "Gizmo")
                .with_state(JobState::Running)
                .with_pid(4242),
            job("Later", "Later").with_state(JobState::Queued),
            job("Gone", "Gone").with_state(JobState::Done),
        ]);

        let interrupted = queue.reconcile_running("interrupted by a launcher restart");

        assert_eq!(
            interrupted,
            vec![Interrupted {
                id: GameId::new(Store::Epic, "Gizmo"),
                pid: Some(4242),
            }]
        );
        assert_eq!(
            queue.jobs()[0].state,
            JobState::Failed {
                message: "interrupted by a launcher restart".into()
            }
        );
        assert_eq!(queue.jobs()[0].pid, None, "the orphan pid is handed back");
        // A queued job survives a restart; a finished one is untouched.
        assert_eq!(queue.jobs()[1].state, JobState::Queued);
        assert_eq!(queue.jobs()[2].state, JobState::Done);
    }

    #[test]
    fn reconcile_reports_a_running_job_that_had_no_pid() {
        let mut queue =
            InstallQueue::from_jobs(vec![job("Gizmo", "Gizmo").with_state(JobState::Running)]);
        let interrupted = queue.reconcile_running("interrupted");
        assert_eq!(
            interrupted,
            vec![Interrupted {
                id: GameId::new(Store::Epic, "Gizmo"),
                pid: None,
            }]
        );
    }

    #[test]
    fn set_pid_attaches_only_to_a_running_job() {
        let mut queue = InstallQueue::new();
        queue.enqueue(job("A", "A")).unwrap();

        assert!(!queue.set_pid(0, 11), "a queued job has no running CLI");
        queue.start_next(free(GIB));
        assert!(queue.set_pid(0, 11));
        assert_eq!(queue.jobs()[0].pid, Some(11));

        queue.finish(0, Ok(()));
        assert!(!queue.set_pid(0, 22), "a finished job has no running CLI");
    }

    #[test]
    fn starting_a_job_clears_a_stale_pid() {
        let mut queue = InstallQueue::from_jobs(vec![
            job("A", "A").with_state(JobState::Queued).with_pid(999),
        ]);
        queue.start_next(free(GIB));
        assert_eq!(queue.jobs()[0].pid, None);
    }

    #[test]
    fn verify_requeues_a_finished_job_as_a_verify_only_repair() {
        let mut queue = InstallQueue::new();
        queue.enqueue(job("Gizmo", "Gizmo")).unwrap();
        queue.start_next(free(GIB));
        queue.finish(0, Err("interrupted by a launcher restart".into()));

        assert!(queue.verify(0));
        let job = &queue.jobs()[0];
        assert_eq!(job.kind, JobKind::Verify, "the re-queued job only verifies");
        assert_eq!(job.state, JobState::Queued);
        assert!(job.state.is_active());
        assert_eq!(
            job.target,
            PathBuf::from("/games/epic/Gizmo"),
            "a verify runs against the directory the user saw"
        );
        assert_eq!(job.pid, None, "no orphan pid is carried into the verify");

        // Only a finished job can be re-queued as a verify.
        queue.start_next(free(GIB));
        assert!(
            !queue.verify(0),
            "a running job must finish before it can be verified"
        );
    }

    #[test]
    fn clear_finished_keeps_active_jobs() {
        let mut queue = InstallQueue::new();
        queue.enqueue(job("A", "A")).unwrap();
        queue.enqueue(job("B", "B")).unwrap();
        queue.enqueue(job("C", "C")).unwrap();

        queue.start_next(free(GIB));
        queue.finish(0, Ok(()));
        queue.cancel(1);

        assert_eq!(queue.clear_finished(), 2);
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.jobs()[0].id.store_id, "C");
        assert_eq!(queue.active_count(), 1);
    }
}
