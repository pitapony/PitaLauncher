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
//!                               │                    worker thread
//!                        InstallEvent ◄─── progress ── installer::run
//! ```
//!
//! The queue is one job at a time by design: the three clients share a login
//! session and their `~/.config` caches, so two installs at once fight over
//! both.

use std::path::Path;
use std::rc::Rc;
use std::thread;

use futures_channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures_util::StreamExt as _;
use futures_util::lock::Mutex as AsyncMutex;
use pitagames_stores::cli::CliOptions;
use pitagames_stores::installer;
use pitagames_stores::queue::{
    InstallJob, InstallQueue, JobProgress, JobState, QueueError, StartOutcome,
};
use pitagames_stores::sync::Backends;

/// One report from a running install worker.
#[derive(Debug, Clone, PartialEq)]
pub enum InstallEvent {
    /// The backend printed something new: a percentage, a stage line, both.
    Progress { index: usize, progress: JobProgress },
    /// The run finished; `Ok` means the backend's verdict passed.
    Finished {
        index: usize,
        result: Result<(), String>,
    },
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
}

impl Installs {
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

    /// Fold a worker event into the queue.
    ///
    /// A late report for a job that was cancelled or already finished is
    /// ignored by the queue itself, which is why this cannot resurrect a row.
    pub fn apply(&mut self, event: &InstallEvent) {
        match event {
            InstallEvent::Progress { index, progress } => {
                self.queue
                    .progress(*index, progress.percent, progress.stage.clone());
            }
            InstallEvent::Finished { index, result } => {
                self.queue.finish(*index, result.clone());
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

    /// The label a grid card shows while this game's install is queued or
    /// running; `None` once it finished (the row is installed by then).
    pub fn active_label(&self, id: &pitagames_core::GameId) -> Option<&'static str> {
        let job = self.queue.get(id)?;
        job.state.is_active().then(|| job.state.label())
    }

    /// Drop every finished job.
    pub fn clear_finished(&mut self) -> usize {
        self.queue.clear_finished()
    }

    /// Put a failed or cancelled job back in the queue.
    pub fn retry(&mut self, index: usize) -> bool {
        self.queue.retry(index)
    }

    /// Stop a queued job. A running one is left to its worker: the store CLI
    /// owns the process once it started, and the driver has no cancel channel
    /// from the UI thread yet.
    pub fn cancel(&mut self, index: usize) -> bool {
        match self.job(index).map(|job| job.state.clone()) {
            Some(JobState::Queued) => self.queue.cancel(index),
            _ => false,
        }
    }

    /// Drop a finished job.
    pub fn remove(&mut self, index: usize) -> bool {
        self.queue.remove(index)
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

/// Run one job on its own thread, reporting progress to `sender`.
///
/// The thread owns nothing but the job: the queue stays on the UI side, so a
/// window that closes mid-install cannot corrupt it.
pub fn spawn(job: InstallJob, index: usize, sender: UnboundedSender<InstallEvent>) {
    let thread_name = format!("pitagames-install-{}", job.id.store_id);
    let spawned = thread::Builder::new().name(thread_name).spawn(move || {
        let options = CliOptions {
            log_dir: pitagames_core::paths::logs_dir(),
            ..CliOptions::default()
        };
        let backends = Backends::new();

        let result = installer::run(&backends, &job, &options, |progress| {
            let _ = sender.unbounded_send(InstallEvent::Progress { index, progress });
        })
        .map(|_| ());

        let _ = sender.unbounded_send(InstallEvent::Finished { index, result });
    });

    if let Err(error) = spawned {
        eprintln!("[pitagames] install worker could not start: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pitagames_core::{GameId, Store};

    fn job(id: &str) -> InstallJob {
        InstallJob::new(
            GameId::new(Store::Epic, id),
            id,
            std::path::PathBuf::from("/games/epic").join(id),
        )
    }

    #[test]
    fn a_job_moves_from_queued_through_running_to_done() {
        let mut installs = Installs::default();
        assert_eq!(installs.enqueue(job("Gizmo")).unwrap(), 0);
        assert_eq!(installs.active_count(), 1);
        assert_eq!(installs.start_next(), StartOutcome::Started { index: 0 });
        assert_eq!(installs.active_count(), 1, "running still counts as active");

        installs.apply(&InstallEvent::Finished {
            index: 0,
            result: Ok(()),
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
            index: 0,
            result: Ok(()),
        });
        assert_eq!(installs.enqueue(job("Gizmo")).unwrap(), 1);
    }

    #[test]
    fn progress_lands_on_the_running_row_and_stops_after_it_finishes() {
        let mut installs = Installs::default();
        installs.enqueue(job("Gizmo")).unwrap();
        installs.start_next();

        installs.apply(&InstallEvent::Progress {
            index: 0,
            progress: JobProgress {
                percent: Some(45.0),
                stage: Some("downloading".into()),
            },
        });
        let progress = &installs.job(0).unwrap().progress;
        assert_eq!(progress.percent, Some(45.0));
        assert_eq!(progress.stage.as_deref(), Some("downloading"));

        installs.apply(&InstallEvent::Finished {
            index: 0,
            result: Err("legendary exited 1".into()),
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
            index: 0,
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
            index: 0,
            result: Err("boom".into()),
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
            index,
            result: Err("boom".into()),
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
            index: 0,
            result: Ok(()),
        });
        assert_eq!(
            installs.active_label(&id),
            None,
            "a finished job leaves the card to the installed row"
        );
        assert_eq!(installs.active_label(&GameId::new(Store::Gog, "1")), None);
    }
}
