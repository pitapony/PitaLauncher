//! The store installs the launcher is running, and the queue behind them.
//!
//! Steam installs are handed to Steam and followed by polling manifests
//! ([`crate::downloads`]). The CLI stores are the opposite: PitaGames runs the
//! client itself, so it owns the ordering (`stores::queue`) and the process
//! (`stores::installer`). This module is the framework-free board between them
//! and the window: it holds the queue, folds the worker's events into it, and
//! hands the next job to a thread.
//!
//! ```text
//!  enqueue(plan.job()) ──► InstallQueue ──start_next──► InstallJob
//!                               ▲                          │
//!                               │                    worker thread + InstallControl
//!                        InstallEvent ◄─── progress ── installer::run
//! ```
//!
//! The queue is one job at a time by design: the three clients share a login
//! session and their `~/.config` caches, so two installs at once fight over
//! both. That is also why a cancelled run keeps its row `Running` until the
//! worker confirms the CLI is gone: the next job must not start while the
//! previous process is still being signalled.
//!
//! **Reports are keyed by [`GameId`], not by row index.** Removing a finished
//! row above a running one shifts every index after it, so an index-keyed
//! report would land on the wrong job — or on none. A game id is stable, and
//! the queue refuses two active jobs for the same game, so it identifies the
//! worker's job uniquely.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::rc::Rc;
use std::thread;

use futures_channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures_util::StreamExt as _;
use futures_util::lock::Mutex as AsyncMutex;
use pitagames_core::GameId;
use pitagames_db::StoredJob;
use pitagames_stores::cli::CliOptions;
use pitagames_stores::installer::{self, InstallControl, InstallRun};
use pitagames_stores::queue::{
    InstallJob, InstallQueue, Interrupted, JobKind, JobProgress, JobState, QueueError, StartOutcome,
};
use pitagames_stores::sync::Backends;

/// How a worker's run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallOutcome {
    /// The backend's verdict passed.
    Done,
    /// The user stopped it; the row says `cancelled` rather than quoting a
    /// signal death.
    Cancelled,
    /// The backend (or the driver) failed; the message is display-ready.
    Failed(String),
}

/// One report from a running install worker.
#[derive(Debug, Clone, PartialEq)]
pub enum InstallEvent {
    /// The CLI is up: its process-group id, reported while it is alive so the
    /// queue can persist it before a crash orphans the process.
    Started { id: GameId, pid: u32 },
    /// The backend printed something new: a percentage, a stage line, both.
    Progress { id: GameId, progress: JobProgress },
    /// The run finished; the queue row moves to its final state.
    Finished { id: GameId, outcome: InstallOutcome },
}

/// The window's end of the install event channel.
///
/// One channel belongs to the app: the grid holds the sender to queue work and
/// the Downloads board reads the queue, while a single drain future owns the
/// receiver. `use_hook` needs a `Clone` value and an `UnboundedReceiver` is not
/// one, so the receiver sits behind an async mutex — the same shape as the
/// activation requests.
#[derive(Clone)]
pub struct InstallChannel {
    sender: UnboundedSender<InstallEvent>,
    receiver: Rc<AsyncMutex<Option<UnboundedReceiver<InstallEvent>>>>,
}

impl InstallChannel {
    pub fn new() -> Self {
        let (sender, receiver) = unbounded();
        Self {
            sender,
            receiver: Rc::new(AsyncMutex::new(Some(receiver))),
        }
    }

    /// A sender for the queue buttons; [`spawn`] takes one per worker.
    pub fn sender(&self) -> UnboundedSender<InstallEvent> {
        self.sender.clone()
    }

    /// Wait for the next worker report; `None` once the channel closes.
    pub async fn next(&self) -> Option<InstallEvent> {
        let mut slot = self.receiver.lock().await;
        let receiver = slot.as_mut()?;
        receiver.next().await
    }
}

impl Default for InstallChannel {
    fn default() -> Self {
        Self::new()
    }
}

/// The install queue plus what the Downloads board needs to render it.
#[derive(Debug, Default, Clone)]
pub struct Installs {
    queue: InstallQueue,
    /// One control per started job. Keyed by game because an index can shift
    /// when a finished row above a running one is removed.
    controls: BTreeMap<GameId, InstallControl>,
    /// Jobs whose CLI has been told to stop but has not confirmed yet.
    cancelling: BTreeSet<GameId>,
}

impl Installs {
    /// Rebuild the board from persisted jobs (a previous launcher run's queue).
    ///
    /// Controls and the cancelling set are fresh: no process from the previous
    /// run is a child of this one, so there is nothing to cancel yet.
    #[must_use]
    pub fn restore(jobs: Vec<InstallJob>) -> Self {
        Self {
            queue: InstallQueue::from_jobs(jobs),
            controls: BTreeMap::new(),
            cancelling: BTreeSet::new(),
        }
    }

    /// The persisted rows for the current queue, in order.
    #[must_use]
    pub fn stored(&self) -> Vec<StoredJob> {
        self.queue.jobs().iter().map(stored_job).collect()
    }

    /// Turn every `Running` job into `Failed` and report what the crashed run
    /// left behind, so the next boot can reap an orphaned CLI by its pid.
    ///
    /// Nothing here touches the filesystem: a partial download is the store's
    /// to repair (the Verify the reconciled row offers), never the launcher's
    /// to delete.
    pub fn reconcile(&mut self, message: &str) -> Vec<Interrupted> {
        let interrupted = self.queue.reconcile_running(message);
        for entry in &interrupted {
            // No worker of this run owns these ids; drop any stale token.
            self.controls.remove(&entry.id);
            self.cancelling.remove(&entry.id);
        }
        interrupted
    }

    /// Record the running CLI's process-group id, for crash recovery.
    pub fn started(&mut self, id: &GameId, pid: u32) -> bool {
        let Some(index) = self.index_of(id) else {
            return false;
        };
        self.queue.set_pid(index, pid)
    }

    /// Add a game unless it is already queued or running.
    pub fn enqueue(&mut self, job: InstallJob) -> Result<usize, QueueError> {
        self.queue.enqueue(job)
    }

    /// Start the first queued job, after the disk pre-check.
    pub fn start_next(&mut self) -> StartOutcome {
        self.queue.start_next(free_space)
    }

    /// The job at `index`, for the worker and for the row that flips to
    /// installed when it finishes.
    pub fn job(&self, index: usize) -> Option<&InstallJob> {
        self.queue.jobs().get(index)
    }

    /// The job for a game, for a report that names its id.
    pub fn job_by_id(&self, id: &GameId) -> Option<&InstallJob> {
        self.queue.jobs().iter().find(|job| &job.id == id)
    }

    /// The control the job's worker will report to, created on first call.
    ///
    /// The token is registered before the thread starts, so a Cancel click that
    /// arrives during the CLI's startup still reaches it.
    pub fn control(&mut self, index: usize) -> Option<InstallControl> {
        let id = self.queue.jobs().get(index)?.id.clone();
        let control = InstallControl::new();
        self.controls.insert(id, control.clone());
        Some(control)
    }

    /// Fold a worker event into the queue.
    ///
    /// A late report for a job that was cancelled or already finished is
    /// ignored by the queue itself, which is why this cannot resurrect a row.
    pub fn apply(&mut self, event: &InstallEvent) {
        match event {
            InstallEvent::Started { id, pid } => {
                self.started(id, *pid);
            }
            InstallEvent::Progress { id, progress } => {
                if let Some(index) = self.index_of(id) {
                    self.queue
                        .progress(index, progress.percent, progress.stage.clone());
                }
            }
            InstallEvent::Finished { id, outcome } => {
                self.cancelling.remove(id);
                self.controls.remove(id);
                let Some(index) = self.index_of(id) else {
                    // The row is gone (removed by the user); nothing to update.
                    return;
                };
                match outcome {
                    InstallOutcome::Done => {
                        self.queue.finish(index, Ok(()));
                    }
                    InstallOutcome::Cancelled => {
                        self.queue.cancel(index);
                    }
                    InstallOutcome::Failed(message) => {
                        self.queue.finish(index, Err(message.clone()));
                    }
                }
            }
        }
    }

    /// The queue itself, for tests that assert on the whole thing.
    #[cfg(test)]
    pub fn queue(&self) -> &InstallQueue {
        &self.queue
    }

    /// Jobs still queued or running, in queue order.
    pub fn active(&self) -> Vec<(usize, &InstallJob)> {
        self.queue
            .jobs()
            .iter()
            .enumerate()
            .filter(|(_, job)| job.state.is_active())
            .collect()
    }

    /// Jobs that will not change again, in queue order.
    pub fn finished(&self) -> Vec<(usize, &InstallJob)> {
        self.queue
            .jobs()
            .iter()
            .enumerate()
            .filter(|(_, job)| job.state.is_finished())
            .collect()
    }

    pub fn active_count(&self) -> usize {
        self.queue.active_count()
    }

    /// The label a grid card shows while this game's install is queued, running
    /// or being stopped; `None` once it finished (the row is installed by then).
    pub fn active_label(&self, id: &GameId) -> Option<&'static str> {
        if self.cancelling.contains(id) {
            return Some("cancelling");
        }
        let job = self.queue.get(id)?;
        job.state.is_active().then(|| job.state.label())
    }

    /// Whether the job's CLI has been asked to stop but has not said so yet.
    pub fn is_cancelling(&self, id: &GameId) -> bool {
        self.cancelling.contains(id)
    }

    /// Drop every finished job.
    pub fn clear_finished(&mut self) -> usize {
        self.queue.clear_finished()
    }

    /// Put a failed or cancelled job back in the queue.
    pub fn retry(&mut self, index: usize) -> bool {
        self.queue.retry(index)
    }

    /// Turn a finished job into a verify-only repair and queue it.
    ///
    /// This is the "Verify" a crashed install's row offers: the store's own
    /// check over the files the interruption left, with no second download.
    pub fn verify(&mut self, index: usize) -> bool {
        self.queue.verify(index)
    }

    /// Cancel the job at `index`.
    ///
    /// A queued job is cancelled outright. A running one keeps its `Running`
    /// state — and therefore its slot in the serial queue — until its worker
    /// reports back, because the CLI is still shutting down: starting the next
    /// install now would let two clients fight over the same session.
    pub fn cancel(&mut self, index: usize) -> bool {
        let Some(job) = self.queue.jobs().get(index) else {
            return false;
        };
        let id = job.id.clone();
        match job.state {
            JobState::Queued => self.queue.cancel(index),
            JobState::Running => {
                let Some(control) = self.controls.get(&id) else {
                    return false;
                };
                control.cancel();
                self.cancelling.insert(id);
                true
            }
            JobState::Done | JobState::Failed { .. } | JobState::Cancelled => false,
        }
    }

    /// Drop a finished job.
    pub fn remove(&mut self, index: usize) -> bool {
        self.queue.remove(index)
    }

    fn index_of(&self, id: &GameId) -> Option<usize> {
        self.queue.jobs().iter().position(|job| &job.id == id)
    }
}

/// The free bytes on the filesystem that would hold `path`.
///
/// An unreadable path is an unknown answer, and the queue deliberately skips
/// the check rather than guessing.
fn free_space(path: &Path) -> Option<u64> {
    pitagames_platform::disk::space(path)
        .ok()
        .map(|space| space.available)
}

/// The persisted row for a job.
///
/// The kind and state travel as their stable wire tokens; the failure message,
/// the size and the recorded pid sit in their own columns. Progress is stored
/// too, so a restored row shows the bar it had reached before the crash.
#[must_use]
pub fn stored_job(job: &InstallJob) -> StoredJob {
    let (state, message) = match &job.state {
        JobState::Failed { message } => ("failed", Some(message.clone())),
        other => (other.wire(), None),
    };
    StoredJob {
        id: job.id.clone(),
        title: job.title.clone(),
        target: job.target.clone(),
        size: job.size,
        kind: job.kind.wire().to_string(),
        state: state.to_string(),
        message,
        pid: job.pid,
        percent: job.progress.percent,
        stage: job.progress.stage.clone(),
    }
}

/// A job rebuilt from its stored row.
///
/// `None` when the row names a kind or state this build does not know (a
/// downgrade after an upgrade persisted a newer token): an unknown row is
/// skipped rather than guessed, so a stale database cannot resurrect a job the
/// current binary cannot run.
#[must_use]
pub fn install_job(row: &StoredJob) -> Option<InstallJob> {
    let kind = JobKind::from_wire(&row.kind)?;
    let state = JobState::from_wire(&row.state, row.message.clone())?;
    let mut job = InstallJob::new(row.id.clone(), row.title.clone(), row.target.clone());
    job.size = row.size;
    job.kind = kind;
    job.state = state;
    job.progress = JobProgress {
        percent: row.percent,
        stage: row.stage.clone(),
    };
    job.pid = row.pid;
    Some(job)
}

/// Reap the install CLIs a crashed launcher left behind.
///
/// The decision (signal only a pid that is still the store's CLI) lives in
/// `stores::recovery`; here we hand it the real `/proc` table and the same
/// `SIGTERM`→`SIGKILL` escalation a Stop button uses. A pid that is gone, or
/// that now belongs to another program, is left alone.
pub fn reap_orphans(orphans: &[Interrupted]) {
    let list = pitagames_stores::recovery::Orphan::from_interrupted(orphans);
    for outcome in pitagames_stores::recovery::reap_all(
        &list,
        pitagames_stores::recovery::system_cmdline,
        |pid| {
            pitagames_platform::launch::terminate(pid, pitagames_platform::launch::DEFAULT_GRACE);
        },
    ) {
        match outcome {
            pitagames_stores::recovery::OrphanOutcome::Signalled { pid, program } => {
                eprintln!("[pitagames] reaped an orphaned {program} install (pid {pid})");
            }
            pitagames_stores::recovery::OrphanOutcome::Foreign { pid, command } => {
                eprintln!("[pitagames] left pid {pid} ({command}) alone; it is not a store CLI");
            }
            pitagames_stores::recovery::OrphanOutcome::Gone { .. } => {}
        }
    }
}

/// Run one job on its own thread, reporting progress to `sender`.
///
/// The thread owns nothing but the job and the control: the queue stays on the
/// UI side, so a window that closes mid-install cannot corrupt it. Cancelling
/// the control reaches the CLI through the handle `installer::run` registers,
/// and the thread always ends with a `Finished` event so the row cannot stick.
pub fn spawn(job: InstallJob, sender: UnboundedSender<InstallEvent>, control: InstallControl) {
    let id = job.id.clone();
    let thread_name = format!("pitagames-install-{}", job.id.store_id);
    let spawned = thread::Builder::new().name(thread_name).spawn(move || {
        let options = CliOptions {
            log_dir: pitagames_core::paths::logs_dir(),
            ..CliOptions::default()
        };
        let backends = Backends::new();

        let report_id = id.clone();
        let started_id = id.clone();
        let result = installer::run(
            &backends,
            &job,
            &options,
            &control,
            |progress| {
                let _ = sender.unbounded_send(InstallEvent::Progress {
                    id: report_id.clone(),
                    progress,
                });
            },
            |pid| {
                // The pid reaches the UI (and SQLite) while the CLI is alive, so
                // a launcher crash leaves the orphan recoverable.
                let _ = sender.unbounded_send(InstallEvent::Started {
                    id: started_id.clone(),
                    pid,
                });
            },
        );

        let outcome = match result {
            Ok(InstallRun {
                cancelled: true, ..
            }) => InstallOutcome::Cancelled,
            Ok(_) => InstallOutcome::Done,
            Err(message) => InstallOutcome::Failed(message),
        };
        let _ = sender.unbounded_send(InstallEvent::Finished { id, outcome });
    });

    if let Err(error) = spawned {
        eprintln!("[pitagames] install worker could not start: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pitagames_core::Store;

    fn job(id: &str) -> InstallJob {
        InstallJob::new(
            GameId::new(Store::Epic, id),
            id,
            std::path::PathBuf::from("/games/epic").join(id),
        )
    }

    fn game(id: &str) -> GameId {
        GameId::new(Store::Epic, id)
    }

    #[test]
    fn a_job_moves_from_queued_through_running_to_done() {
        let mut installs = Installs::default();
        assert_eq!(installs.enqueue(job("Gizmo")).unwrap(), 0);
        assert_eq!(installs.active_count(), 1);
        assert_eq!(installs.start_next(), StartOutcome::Started { index: 0 });
        assert_eq!(installs.active_count(), 1, "running still counts as active");

        installs.apply(&InstallEvent::Finished {
            id: game("Gizmo"),
            outcome: InstallOutcome::Done,
        });
        assert_eq!(installs.active_count(), 0);
        assert_eq!(installs.finished().len(), 1);
        assert_eq!(installs.finished()[0].1.state, JobState::Done);
    }

    #[test]
    fn a_duplicate_is_refused_while_the_first_is_active() {
        let mut installs = Installs::default();
        installs.enqueue(job("Gizmo")).unwrap();
        assert_eq!(installs.enqueue(job("Gizmo")), Err(QueueError::Duplicate));

        // Once it finishes the id can be queued again.
        installs.start_next();
        installs.apply(&InstallEvent::Finished {
            id: game("Gizmo"),
            outcome: InstallOutcome::Done,
        });
        assert_eq!(installs.enqueue(job("Gizmo")).unwrap(), 1);
    }

    #[test]
    fn progress_lands_on_the_running_row_and_stops_after_it_finishes() {
        let mut installs = Installs::default();
        installs.enqueue(job("Gizmo")).unwrap();
        installs.start_next();

        installs.apply(&InstallEvent::Progress {
            id: game("Gizmo"),
            progress: JobProgress {
                percent: Some(45.0),
                stage: Some("downloading".into()),
            },
        });
        let progress = &installs.job(0).unwrap().progress;
        assert_eq!(progress.percent, Some(45.0));
        assert_eq!(progress.stage.as_deref(), Some("downloading"));

        installs.apply(&InstallEvent::Finished {
            id: game("Gizmo"),
            outcome: InstallOutcome::Failed("legendary exited 1".into()),
        });
        assert_eq!(
            installs.job(0).unwrap().state,
            JobState::Failed {
                message: "legendary exited 1".into()
            }
        );
        assert_eq!(
            installs.job(0).unwrap().progress,
            JobProgress::default(),
            "the state word replaces the bar"
        );

        // A late progress report from the worker is ignored.
        installs.apply(&InstallEvent::Progress {
            id: game("Gizmo"),
            progress: JobProgress {
                percent: Some(99.0),
                stage: None,
            },
        });
        assert_eq!(installs.job(0).unwrap().progress.percent, None);
    }

    #[test]
    fn a_failed_job_can_be_retried_and_a_finished_one_removed() {
        let mut installs = Installs::default();
        installs.enqueue(job("Gizmo")).unwrap();
        installs.start_next();
        installs.apply(&InstallEvent::Finished {
            id: game("Gizmo"),
            outcome: InstallOutcome::Failed("boom".into()),
        });

        assert!(installs.retry(0));
        assert_eq!(installs.job(0).unwrap().state, JobState::Queued);
        assert_eq!(installs.active_count(), 1);

        assert!(installs.cancel(0), "a queued job can be cancelled");
        assert_eq!(installs.job(0).unwrap().state, JobState::Cancelled);
        assert!(installs.remove(0));
        assert!(installs.queue().is_empty());
    }

    #[test]
    fn a_short_on_space_job_is_skipped_without_starting() {
        let mut installs = Installs::default();
        installs.enqueue(job("Gizmo").with_size(u64::MAX)).unwrap();
        assert!(matches!(
            installs.start_next(),
            StartOutcome::Skipped { .. }
        ));
        assert!(installs.active_count() == 0);
        assert_eq!(installs.job(0).unwrap().state.label(), "failed");
    }

    #[test]
    fn a_retried_job_keeps_the_target_the_user_saw() {
        let mut installs = Installs::default();
        let index = installs.enqueue(job("Gizmo")).unwrap();
        installs.start_next();
        installs.apply(&InstallEvent::Finished {
            id: game("Gizmo"),
            outcome: InstallOutcome::Failed("boom".into()),
        });

        assert!(installs.retry(index));
        assert_eq!(
            installs.job(index).unwrap().target,
            std::path::PathBuf::from("/games/epic/Gizmo"),
            "a retry fills the same directory, never a title-derived one"
        );
    }

    #[test]
    fn the_grid_label_tracks_only_an_active_job() {
        let mut installs = Installs::default();
        let id = GameId::new(Store::Epic, "Gizmo");
        assert_eq!(installs.active_label(&id), None, "nothing queued yet");

        installs.enqueue(job("Gizmo")).unwrap();
        assert_eq!(installs.active_label(&id), Some("queued"));

        installs.start_next();
        assert_eq!(installs.active_label(&id), Some("installing"));

        installs.apply(&InstallEvent::Finished {
            id: id.clone(),
            outcome: InstallOutcome::Done,
        });
        assert_eq!(
            installs.active_label(&id),
            None,
            "a finished job leaves the card to the installed row"
        );
        assert_eq!(installs.active_label(&GameId::new(Store::Gog, "1")), None);
    }

    #[test]
    fn cancelling_a_running_job_keeps_the_slot_until_the_worker_confirms() {
        let mut installs = Installs::default();
        installs.enqueue(job("Gizmo")).unwrap();
        installs.enqueue(job("Other")).unwrap();
        installs.start_next();
        let control = installs.control(0).expect("a started job has a control");

        assert!(installs.cancel(0));
        assert!(
            control.is_cancelled(),
            "the worker's token was told to stop"
        );
        assert!(installs.is_cancelling(&game("Gizmo")));
        assert_eq!(installs.active_label(&game("Gizmo")), Some("cancelling"));
        assert_eq!(
            installs.job(0).unwrap().state,
            JobState::Running,
            "the row stays running so the serial queue cannot start another job"
        );
        assert_eq!(
            installs.start_next(),
            StartOutcome::Busy,
            "a running slot blocks the next install while the CLI shuts down"
        );

        installs.apply(&InstallEvent::Finished {
            id: game("Gizmo"),
            outcome: InstallOutcome::Cancelled,
        });
        assert_eq!(installs.job(0).unwrap().state, JobState::Cancelled);
        assert!(!installs.is_cancelling(&game("Gizmo")));
        assert_eq!(installs.start_next(), StartOutcome::Started { index: 1 });
    }

    #[test]
    fn a_running_job_without_a_control_is_not_cancellable() {
        let mut installs = Installs::default();
        installs.enqueue(job("Gizmo")).unwrap();
        installs.start_next();

        // No worker token was registered (the thread never started).
        assert!(!installs.cancel(0));
        assert_eq!(installs.job(0).unwrap().state, JobState::Running);
    }

    #[test]
    fn reports_are_keyed_by_game_not_by_row() {
        let mut installs = Installs::default();
        installs.enqueue(job("Gizmo")).unwrap();
        installs.start_next();
        installs.enqueue(job("Other")).unwrap();

        // A finished/cancelled row above the running one disappears: every
        // index after it shifts. An id-keyed report still lands.
        installs.apply(&InstallEvent::Finished {
            id: game("Gizmo"),
            outcome: InstallOutcome::Failed("boom".into()),
        });
        installs.remove(0);
        assert_eq!(installs.job(0).unwrap().id, game("Other"));
        assert_eq!(installs.start_next(), StartOutcome::Started { index: 0 });

        installs.apply(&InstallEvent::Progress {
            id: game("Other"),
            progress: JobProgress {
                percent: Some(50.0),
                stage: None,
            },
        });
        assert_eq!(installs.job(0).unwrap().progress.percent, Some(50.0));

        // A report for a row the user removed is ignored, not misapplied.
        installs.apply(&InstallEvent::Progress {
            id: game("Gizmo"),
            progress: JobProgress {
                percent: Some(99.0),
                stage: None,
            },
        });
        assert_eq!(installs.job(0).unwrap().progress.percent, Some(50.0));
    }

    // -- crash recovery: persistence, reconciliation and Verify --------------

    #[test]
    fn a_job_round_trips_through_its_persisted_row() {
        let original = job("Gizmo").as_update().with_size(42).with_pid(9);
        let row = stored_job(&original);
        assert_eq!(row.kind, "update");
        assert_eq!(row.state, "queued");
        assert_eq!(row.size, Some(42));
        assert_eq!(row.pid, Some(9));
        assert_eq!(
            install_job(&row).unwrap(),
            original,
            "a persisted job rebuilds field-for-field"
        );
    }

    #[test]
    fn a_failed_job_keeps_its_message_across_a_round_trip() {
        let failed = job("Gizmo").with_state(JobState::Failed {
            message: "legendary exited 1".into(),
        });
        let row = stored_job(&failed);
        assert_eq!(row.state, "failed");
        assert_eq!(row.message.as_deref(), Some("legendary exited 1"));
        assert_eq!(install_job(&row).unwrap(), failed);
    }

    #[test]
    fn an_unknown_persisted_token_is_skipped_not_guessed() {
        let mut row = stored_job(&job("Gizmo"));
        row.kind = "teleport".into();
        assert!(install_job(&row).is_none());
    }

    #[test]
    fn a_restored_running_job_surfaces_as_failed_and_reports_its_pid() {
        let mut installs = Installs::restore(vec![
            job("Gizmo").with_state(JobState::Running).with_pid(4242),
            job("Later").with_state(JobState::Queued),
        ]);

        let interrupted = installs.reconcile("an earlier launcher run was interrupted");

        assert_eq!(interrupted.len(), 1);
        assert_eq!(interrupted[0].pid, Some(4242));
        assert_eq!(
            installs.job(0).unwrap().state,
            JobState::Failed {
                message: "an earlier launcher run was interrupted".into()
            }
        );
        assert_eq!(installs.job(0).unwrap().pid, None);
        assert_eq!(
            installs.job(1).unwrap().state,
            JobState::Queued,
            "a job that never started survives the restart"
        );

        // The reconciled failure is what the next boot persists and renders.
        let stored = installs.stored();
        assert_eq!(stored[0].state, "failed");
        assert_eq!(stored[0].pid, None);
    }

    #[test]
    fn verify_requeues_a_failed_row_as_a_verify_only_job() {
        let mut installs = Installs::restore(vec![job("Gizmo").with_state(JobState::Failed {
            message: "interrupted".into(),
        })]);

        assert!(installs.verify(0));
        assert_eq!(installs.job(0).unwrap().kind, JobKind::Verify);
        assert_eq!(installs.job(0).unwrap().state, JobState::Queued);
        assert_eq!(
            installs.job(0).unwrap().target,
            std::path::PathBuf::from("/games/epic/Gizmo"),
            "the verify checks the directory the user saw"
        );
    }

    #[test]
    fn a_started_event_attaches_the_pid_to_the_running_job() {
        let mut installs = Installs::default();
        installs.enqueue(job("Gizmo")).unwrap();
        installs.start_next();

        installs.apply(&InstallEvent::Started {
            id: game("Gizmo"),
            pid: 77,
        });
        assert_eq!(installs.job(0).unwrap().pid, Some(77));

        // A stale pid for a job that already finished is not attached.
        installs.apply(&InstallEvent::Finished {
            id: game("Gizmo"),
            outcome: InstallOutcome::Done,
        });
        installs.apply(&InstallEvent::Started {
            id: game("Gizmo"),
            pid: 88,
        });
        assert_eq!(installs.job(0).unwrap().pid, None);
    }
}
