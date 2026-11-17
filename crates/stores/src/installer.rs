//! Running a queued install on one of the CLI store backends.
//!
//! Three modules already own a piece of a non-Steam install: [`crate::queue`]
//! owns the ordering and the disk pre-check, [`crate::sync`] owns what each
//! backend says the account owns, and the per-store backends ([`crate::epic`],
//! [`crate::amazon`], [`crate::gog`]) own the argv. This module is the seam
//! between them: what a job *is* (the install directory the store will fill and
//! the program that fills it) and how to run it (the PTY driver, the progress it
//! reports and the verdict).
//!
//! ```text
//!  installer::plan(id, title) ──► InstallPlan { target, program }
//!          │                              │
//!          │                              └─ .job() ──► queue::InstallJob
//!          ▼
//!  installer::run(backends, job, options, on_progress) ──► Ok/Err
//!          │            │
//!          │            └─ backends.epic/amazon/gog.stream(install, …)
//!          └─ JobProgress { percent, stage } per change
//! ```
//!
//! The split is deliberate: a plan is pure and can be computed when the user
//! clicks Install (so the queue row shows its target before any process exists),
//! while a run blocks on the CLI and belongs on a worker thread.
//!
//! **Every backend's install verdict is `RunReport::succeeded()`**, which is
//! exit `0` *and* no `ERROR`/`CRITICAL`/`FATAL` line. legendary and nile both log
//! an error and exit `0` for some refusals, and gogdl exits non-zero — one rule
//! covers all three, and the CLI's own summary line is not trusted.
//!
//! **A run is cancellable from another thread.** `run` spawns the CLI itself
//! instead of calling a backend's blocking `stream`, so it can register the
//! [`CliHandle`] with an [`InstallControl`] the UI holds: Cancel calls
//! `control.cancel()`, the driver signals the process group and the run ends
//! with [`InstallRun::cancelled`] set. The handle's two-lock poll is what makes
//! that call reach a worker blocked on a silent CLI.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use pitagames_core::{GameId, Store};

use crate::amazon;
use crate::cli::{CliCommand, CliError, CliHandle, CliOptions, CliOutcome, ProgressEvent};
use crate::epic;
use crate::gog;
use crate::queue::{InstallJob, JobProgress};
use crate::report::RunReport;
use crate::sync::Backends;

/// Where one store installs a game and which program does it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPlan {
    pub id: GameId,
    pub title: String,
    /// The CLI that will run, for messages and the transcript name.
    pub program: &'static str,
    /// The directory the store will fill: `games/<store>/<safe title>`.
    pub target: PathBuf,
}

impl InstallPlan {
    /// The queue entry for this plan.
    ///
    /// The job carries the target, so a retry after a restart still installs
    /// into the directory the user saw — `run` reads the job, never the title.
    pub fn job(&self) -> InstallJob {
        InstallJob::new(self.id.clone(), self.title.clone(), self.target.clone())
    }

    /// The command a backend needs, addressed to this plan's directory.
    pub fn command(&self) -> CliCommand {
        install_command(&self.id, &self.target)
    }
}

/// Why a game cannot be queued for a store install.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InstallerError {
    /// Steam installs through its own client, and there is no local-store
    /// backend — only the three CLI stores install this way.
    #[error("{0} has no command-line install backend; only epic, amazon and gog install this way")]
    Unsupported(Store),

    /// `games_dir()` needs `XDG_DATA_HOME` or `HOME`.
    #[error("no data directory (HOME is unset), so there is nowhere to install")]
    NoDataDir,
}

/// What a finished run did, for the Downloads row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallRun {
    /// The program that ran.
    pub program: &'static str,
    /// The directory the store filled (or would have).
    pub target: PathBuf,
    /// The raw transcript of the run, when logging was on.
    pub transcript: Option<PathBuf>,
    /// The user stopped the run before it finished.
    ///
    /// A cancelled run is not a failure: the CLI was killed on request, so the
    /// queue row says `cancelled` rather than quoting the driver's exit code.
    /// Whatever the store had already written to `target` stays.
    pub cancelled: bool,
}

/// The other thread's end of a running install.
///
/// Created by the UI before the worker starts, cloned into it, and kept so a
/// Cancel button can reach the [`CliHandle`] the worker owns. Cancelling is
/// idempotent and safe at any time: before [`run`] spawns anything it only sets
/// the flag (so no process starts), and once the handle is registered it also
/// signals the CLI's process group.
#[derive(Clone, Default)]
pub struct InstallControl {
    inner: Arc<ControlInner>,
}

#[derive(Default)]
struct ControlInner {
    cancelled: AtomicBool,
    /// The running CLI, registered for as long as [`run`] is driving it.
    handle: Mutex<Option<Arc<CliHandle>>>,
}

impl InstallControl {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask the run to stop. Safe before it starts and after it finished.
    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::SeqCst);
        let handle = self
            .inner
            .handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(handle) = handle {
            // The driver's two-lock poll means this reaches a worker that is
            // blocked waiting for output.
            handle.cancel();
        }
    }

    /// Whether [`cancel`](Self::cancel) was called.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    /// Publish the handle so `cancel` can reach it.
    ///
    /// Returns whether a cancel already arrived while the CLI was starting, in
    /// which case the caller must cancel the handle it just registered.
    fn attach(&self, handle: &Arc<CliHandle>) -> bool {
        let mut slot = self
            .inner
            .handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *slot = Some(handle.clone());
        self.is_cancelled()
    }

    /// Forget the handle: the run is over, so a later cancel has nothing to stop.
    fn detach(&self) {
        *self
            .inner
            .handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

impl std::fmt::Debug for InstallControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstallControl")
            .field("cancelled", &self.is_cancelled())
            .finish_non_exhaustive()
    }
}

/// Plan an install for `id`/`title`.
///
/// The target comes from the store's own module (`epic::install_dir` and
/// friends), so the path is the same one `pitagames-cli epic install` uses and
/// the two cannot drift. Nothing is created here.
pub fn plan(id: &GameId, title: &str) -> Result<InstallPlan, InstallerError> {
    let (target, program) = match id.store {
        Store::Epic => (epic::install_dir(title), epic::PROGRAM),
        Store::Amazon => (amazon::install_dir(title), amazon::PROGRAM),
        Store::Gog => (gog::install_dir(title), gog::GAME_PROGRAM),
        other => return Err(InstallerError::Unsupported(other)),
    };

    Ok(InstallPlan {
        id: id.clone(),
        title: title.to_string(),
        program,
        target: target.ok_or(InstallerError::NoDataDir)?,
    })
}

/// Run one queued install to completion, or until `control` cancels it.
///
/// Blocks while the store CLI works: call this on a worker thread. Every
/// change in progress is handed to `on_progress` as a [`JobProgress`], which is
/// the shape [`crate::queue::InstallQueue::progress`] takes, and the returned
/// error is already display-ready (the CLI's own message, plus the transcript
/// path when one was written). A run `control` stopped comes back as an
/// `Ok` [`InstallRun`] with `cancelled` set — it is a user decision, not a
/// store failure.
pub fn run(
    backends: &Backends,
    job: &InstallJob,
    options: &CliOptions,
    control: &InstallControl,
    mut on_progress: impl FnMut(JobProgress),
) -> Result<InstallRun, String> {
    if !matches!(job.id.store, Store::Epic | Store::Amazon | Store::Gog) {
        return Err(InstallerError::Unsupported(job.id.store).to_string());
    }

    // Cancelled before the CLI started: no process, no directory.
    if control.is_cancelled() {
        return Ok(cancelled_run(job));
    }

    let command = install_command(&job.id, &job.target);
    let handle = Arc::new(
        spawn_backend(backends, job.id.store, &command, options)
            .map_err(|error| format!("could not run the {} install: {error}", job.id.store))?,
    );
    if control.attach(&handle) {
        handle.cancel();
    }

    let mut tracker = ProgressTracker::default();
    loop {
        match handle.next_event() {
            Ok(Some(event)) => tracker.observe(&event, &mut on_progress),
            Ok(None) => break,
            Err(error) => {
                control.detach();
                return Err(format!(
                    "could not read the {} install: {error}",
                    job.id.store
                ));
            }
        }
    }
    let outcome = handle.drain();
    control.detach();

    let outcome =
        outcome.map_err(|error| format!("could not read the {} install: {error}", job.id.store))?;
    let report = report_backend(backends, job.id.store, &command, outcome);

    if report.outcome.cancelled {
        return Ok(cancelled_run(job));
    }
    if !report.succeeded() {
        return Err(failure_message(&job.id, &report));
    }

    Ok(InstallRun {
        program: program_for(job.id.store),
        target: job.target.clone(),
        transcript: report.log_path().map(Path::to_path_buf),
        cancelled: false,
    })
}

/// The report for a run that ended before doing any work.
fn cancelled_run(job: &InstallJob) -> InstallRun {
    InstallRun {
        program: program_for(job.id.store),
        target: job.target.clone(),
        transcript: None,
        cancelled: true,
    }
}

/// The backend's own `spawn`, so the handle stays with the caller.
fn spawn_backend(
    backends: &Backends,
    store: Store,
    command: &CliCommand,
    options: &CliOptions,
) -> Result<CliHandle, CliError> {
    match store {
        Store::Epic => backends.epic.spawn(command, options),
        Store::Amazon => backends.amazon.spawn(command, options),
        Store::Gog => backends.gog.spawn(command, options),
        // `run` refuses the other stores before this point; Epic's layout is
        // the fallback so the function stays total.
        _ => Err(CliError::Unsupported {
            command: command.as_str(),
        }),
    }
}

/// The backend's own finish-line classification (each store reads its layout).
fn report_backend(
    backends: &Backends,
    store: Store,
    command: &CliCommand,
    outcome: CliOutcome,
) -> RunReport {
    match store {
        Store::Amazon => backends.amazon.report(command, outcome),
        Store::Gog => backends.gog.report(command, outcome),
        // Epic's verdict, and the layout the unsupported case never reaches.
        _ => backends.epic.report(command, outcome),
    }
}

/// The install command for `id` landing in `target`.
fn install_command(id: &GameId, target: &Path) -> CliCommand {
    CliCommand::Install {
        id: id.store_id.clone(),
        install_dir: target.to_path_buf(),
    }
}

const fn program_for(store: Store) -> &'static str {
    match store {
        Store::Epic => epic::PROGRAM,
        Store::Amazon => amazon::PROGRAM,
        Store::Gog => gog::GAME_PROGRAM,
        _ => "",
    }
}

/// A failed run as one line: what the CLI said, and where the transcript is.
fn failure_message(id: &GameId, report: &RunReport) -> String {
    let mut message = format!(
        "{} install failed: {}",
        id.store,
        report
            .failure_message()
            .unwrap_or_else(|| "the client exited without a status line".to_string())
    );
    if let Some(path) = report.log_path() {
        message.push_str(&format!(" (transcript: {})", path.display()));
    }
    message
}

/// What the last emitted progress was, so a chatty CLI does not flood the UI.
#[derive(Debug, Default)]
struct ProgressTracker {
    last: JobProgress,
}

impl ProgressTracker {
    /// Fold one event in, emitting only when the bar or the stage line changed.
    fn observe(&mut self, event: &ProgressEvent, emit: &mut impl FnMut(JobProgress)) {
        let changed = match event {
            ProgressEvent::Percent(percent, text) => {
                let changed = self.last.percent != Some(*percent);
                self.last.set_percent(*percent);
                self.stage_from(text) || changed
            }
            ProgressEvent::Stage(text) | ProgressEvent::Line(text) => self.stage_from(text),
            // The verdict is `RunReport`, not these: `Done` only means exit 0.
            ProgressEvent::Done | ProgressEvent::Failed { .. } => return,
        };

        // A PTY interleaves blank lines and repeats; neither is news.
        if changed {
            emit(self.last.clone());
        }
    }

    /// Record a stage line, ignoring repeats and capping its length.
    ///
    /// Returns whether the stage actually changed.
    fn stage_from(&mut self, text: &str) -> bool {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return false;
        }
        let mut capped: String = trimmed.chars().take(STAGE_LIMIT).collect();
        if capped.len() < trimmed.len() {
            capped.push('…');
        }
        if self.last.stage.as_deref() == Some(capped.as_str()) {
            return false;
        }
        self.last.stage = Some(capped);
        true
    }
}

/// Longest stage line kept for the Downloads row.
const STAGE_LIMIT: usize = 200;

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn game(store: Store, id: &str) -> GameId {
        GameId::new(store, id)
    }

    fn options() -> CliOptions {
        CliOptions {
            log_dir: None,
            ..CliOptions::default()
        }
    }

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&path, permissions).unwrap();
        }
        path
    }

    fn backends(epic: &str, amazon: &str, gog: &str) -> Backends {
        Backends {
            epic: epic::Epic::with_program(epic),
            amazon: amazon::Amazon::with_program(amazon),
            gog: gog::Gog::with_programs(gog, gog),
        }
    }

    #[test]
    fn a_plan_names_the_stores_own_install_directory() {
        let _guard = env_lock();
        let data = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_DATA_HOME", data.path()) };

        let epic = plan(&game(Store::Epic, "Gizmo"), "Gizmo Deluxe").unwrap();
        assert_eq!(
            epic.target,
            data.path().join("pitagames/games/epic/Gizmo Deluxe")
        );
        assert_eq!(epic.program, "legendary");

        let amazon = plan(&game(Store::Amazon, "amzn1.adg.product.123"), "Sugar").unwrap();
        assert_eq!(
            amazon.target,
            data.path().join("pitagames/games/amazon/Sugar")
        );
        assert_eq!(amazon.program, "nile");

        let gog = plan(&game(Store::Gog, "1207658930"), "A/B").unwrap();
        assert_eq!(
            gog.target,
            data.path().join("pitagames/games/gog/A-B"),
            "a hostile title cannot escape the store's directory"
        );
        assert_eq!(gog.program, "gogdl");

        unsafe { std::env::remove_var("XDG_DATA_HOME") };
    }

    #[test]
    fn a_plan_becomes_the_queue_entry_and_its_command() {
        let _guard = env_lock();
        let data = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_DATA_HOME", data.path()) };

        let plan = plan(&game(Store::Epic, "Gizmo"), "Gizmo").unwrap();
        let job = plan.job();
        assert_eq!(job.id, game(Store::Epic, "Gizmo"));
        assert_eq!(job.title, "Gizmo");
        assert_eq!(job.target, plan.target);
        assert!(job.size.is_none(), "no store reports a size yet");

        match plan.command() {
            CliCommand::Install { id, install_dir } => {
                assert_eq!(id, "Gizmo");
                assert_eq!(install_dir, plan.target);
            }
            other => panic!("expected an install, got {other:?}"),
        }

        unsafe { std::env::remove_var("XDG_DATA_HOME") };
    }

    #[test]
    fn steam_and_local_games_have_no_cli_install() {
        assert_eq!(
            plan(&game(Store::Steam, "620"), "Portal 2"),
            Err(InstallerError::Unsupported(Store::Steam))
        );
        assert_eq!(
            plan(&game(Store::Local, "shortcut"), "Shortcut"),
            Err(InstallerError::Unsupported(Store::Local))
        );
    }

    #[test]
    fn a_missing_data_directory_is_named_not_guessed() {
        let _guard = env_lock();
        unsafe { std::env::remove_var("XDG_DATA_HOME") };
        unsafe { std::env::set_var("HOME", "") };
        assert_eq!(
            plan(&game(Store::Epic, "Gizmo"), "Gizmo"),
            Err(InstallerError::NoDataDir)
        );
        unsafe { std::env::remove_var("HOME") };
    }

    #[test]
    fn a_dry_install_reports_progress_and_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let program = script(
            dir.path(),
            "legendary",
            "#!/bin/sh\n\
             echo '[DLManager] INFO: = Progress: 42.50% (425/1000), Running for 00:00:01'\n\
             echo '[DLManager] INFO: = Progress: 100.00% (1000/1000), Running for 00:00:02'\n\
             echo 'Finished installation'\n\
             exit 0\n",
        );
        let backends = backends(program.to_str().unwrap(), "/nonexistent/nile", "gogdl");

        let job = InstallJob::new(
            game(Store::Epic, "Gizmo"),
            "Gizmo",
            dir.path().join("Gizmo"),
        );
        let mut seen: Vec<JobProgress> = Vec::new();
        let run = run(
            &backends,
            &job,
            &options(),
            &InstallControl::new(),
            |progress| seen.push(progress),
        )
        .unwrap();

        assert_eq!(run.program, "legendary");
        assert_eq!(run.target, job.target);
        assert!(!run.cancelled);
        let last = seen.last().expect("progress was reported");
        assert_eq!(last.percent, Some(100.0));
        let percents: Vec<Option<f32>> = seen.iter().map(|progress| progress.percent).collect();
        assert!(percents.contains(&Some(42.5)), "{seen:?}");
    }

    #[test]
    fn an_install_that_logs_an_error_and_exits_zero_fails() {
        let dir = tempfile::tempdir().unwrap();
        let program = script(
            dir.path(),
            "legendary",
            "#!/bin/sh\n\
             echo '[cli] ERROR: Game is managed by another launcher' >&2\n\
             exit 0\n",
        );
        let backends = backends(program.to_str().unwrap(), "/nonexistent/nile", "gogdl");

        let job = InstallJob::new(
            game(Store::Epic, "Gizmo"),
            "Gizmo",
            dir.path().join("Gizmo"),
        );
        let error = run(&backends, &job, &options(), &InstallControl::new(), |_| {}).unwrap_err();
        assert!(error.contains("another launcher"), "{error}");
        assert!(error.starts_with("epic install failed"), "{error}");
    }

    #[test]
    fn a_backend_that_exits_non_zero_fails() {
        let dir = tempfile::tempdir().unwrap();
        let program = script(
            dir.path(),
            "gogdl",
            "#!/bin/sh\n\
             echo '[gogdl] ERROR: no auth available' >&2\n\
             exit 1\n",
        );
        let backends = backends(
            "/nonexistent/legendary",
            "/nonexistent/nile",
            program.to_str().unwrap(),
        );

        let job = InstallJob::new(
            game(Store::Gog, "1207658930"),
            "Game",
            dir.path().join("Game"),
        );
        let error = run(&backends, &job, &options(), &InstallControl::new(), |_| {}).unwrap_err();
        assert!(error.contains("gog install failed"), "{error}");
    }

    #[test]
    fn a_missing_program_is_reported_with_the_store_name() {
        let dir = tempfile::tempdir().unwrap();
        let backends = backends(
            "/nonexistent/legendary",
            "/nonexistent/nile",
            "/nonexistent/gogdl",
        );

        let job = InstallJob::new(
            game(Store::Amazon, "amzn1"),
            "Game",
            dir.path().join("Game"),
        );
        let error = run(&backends, &job, &options(), &InstallControl::new(), |_| {}).unwrap_err();
        assert!(error.contains("amazon"), "{error}");
        assert!(error.contains("nile"), "{error}");
    }

    #[test]
    fn a_steam_job_is_refused_by_the_runner_too() {
        let dir = tempfile::tempdir().unwrap();
        let backends = backends("/nonexistent/legendary", "/nonexistent/nile", "gogdl");
        let job = InstallJob::new(
            game(Store::Steam, "620"),
            "Portal 2",
            dir.path().join("Portal 2"),
        );
        let error = run(&backends, &job, &options(), &InstallControl::new(), |_| {}).unwrap_err();
        assert!(error.contains("no command-line install backend"), "{error}");
    }

    #[test]
    fn cancelling_a_running_install_stops_the_cli() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("started");
        let program = script(
            dir.path(),
            "legendary",
            &format!(
                "#!/bin/sh\n\
                 : > '{}'\n\
                 while :; do sleep 1; done\n",
                marker.display()
            ),
        );
        let backends = backends(program.to_str().unwrap(), "/nonexistent/nile", "gogdl");
        let job = InstallJob::new(
            game(Store::Epic, "Gizmo"),
            "Gizmo",
            dir.path().join("Gizmo"),
        );
        let options = options();
        let control = InstallControl::new();

        let outcome = std::thread::scope(|scope| {
            let worker = scope.spawn(|| run(&backends, &job, &options, &control, |_| {}));

            // Wait for the fake CLI to be alive before cancelling: the signal
            // has to reach a running process, not a flag set before the spawn.
            for _ in 0..500 {
                if marker.exists() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            assert!(marker.exists(), "the fake CLI never started");
            assert!(!control.is_cancelled());

            control.cancel();
            worker.join().unwrap()
        });

        let run = outcome.unwrap();
        assert!(run.cancelled, "a cancel is a user decision, not a failure");
        assert_eq!(run.program, "legendary");
        assert_eq!(run.target, job.target);
    }

    #[test]
    fn cancelling_before_the_run_starts_spawns_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("started");
        let program = script(
            dir.path(),
            "legendary",
            &format!("#!/bin/sh\n: > '{}'\nexit 0\n", marker.display()),
        );
        let backends = backends(program.to_str().unwrap(), "/nonexistent/nile", "gogdl");
        let job = InstallJob::new(
            game(Store::Epic, "Gizmo"),
            "Gizmo",
            dir.path().join("Gizmo"),
        );

        let control = InstallControl::new();
        control.cancel();

        let run = run(&backends, &job, &options(), &control, |_| {}).unwrap();
        assert!(run.cancelled);
        assert!(run.transcript.is_none(), "no process means no transcript");
        assert!(!marker.exists(), "the CLI must not have been started");
    }

    #[test]
    fn a_finished_run_forgets_its_handle_and_late_cancels_are_harmless() {
        let dir = tempfile::tempdir().unwrap();
        let program = script(dir.path(), "legendary", "#!/bin/sh\nexit 0\n");
        let backends = backends(program.to_str().unwrap(), "/nonexistent/nile", "gogdl");
        let job = InstallJob::new(
            game(Store::Epic, "Gizmo"),
            "Gizmo",
            dir.path().join("Gizmo"),
        );

        let control = InstallControl::new();
        let run = run(&backends, &job, &options(), &control, |_| {}).unwrap();
        assert!(!run.cancelled);

        // The worker is gone; cancel now only records the flag.
        control.clone().cancel();
        assert!(control.is_cancelled());
        assert!(format!("{control:?}").contains("cancelled: true"));
    }

    #[test]
    fn the_tracker_ignores_blanks_and_repeats() {
        let mut tracker = ProgressTracker::default();
        let mut seen = Vec::new();
        let mut emit = |progress: JobProgress| seen.push(progress);

        tracker.observe(&ProgressEvent::Line("   ".into()), &mut emit);
        tracker.observe(&ProgressEvent::Line("downloading".into()), &mut emit);
        tracker.observe(&ProgressEvent::Line("downloading".into()), &mut emit);
        tracker.observe(
            &ProgressEvent::Percent(10.0, "= Progress: 10%".into()),
            &mut emit,
        );
        tracker.observe(&ProgressEvent::Done, &mut emit);

        assert_eq!(seen.len(), 2, "{seen:?}");
        assert_eq!(seen[0].stage.as_deref(), Some("downloading"));
        assert_eq!(seen[1].percent, Some(10.0));
    }

    #[test]
    fn a_long_stage_line_is_capped() {
        let mut tracker = ProgressTracker::default();
        let mut seen = Vec::new();
        let long = "x".repeat(STAGE_LIMIT * 2);
        tracker.observe(&ProgressEvent::Line(long), &mut |progress| {
            seen.push(progress)
        });

        let stage = seen[0].stage.as_deref().unwrap();
        assert!(stage.ends_with('…'));
        assert_eq!(stage.chars().count(), STAGE_LIMIT + 1);
    }

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }
}
