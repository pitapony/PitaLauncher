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
//!  installer::run(backends, job, options, control, on_progress) ──► Ok/Err
//!          │            │
//!          │            ├─ install phase ──► backends.epic/amazon/gog
//!          │            └─ verify phase (JobKind::Update only) ──► the same
//!          └─ JobProgress { percent, stage } per change
//! ```
//!
//! **An update is an install of an existing id plus a verify.** It is not a
//! second path: [`update_plan`] produces the same [`InstallPlan`]/[`InstallJob`]
//! a fresh install does (with the directory the game already occupies), only
//! [`crate::queue::JobKind::Update`], and [`run`] then follows the install with
//! the backend's own verify. The Updates chip, the store-status list and the
//! grid's Install button therefore all drive one queue.
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

use pitagames_core::{Game, GameId, Store};

use crate::amazon;
use crate::cli::{CliCommand, CliError, CliHandle, CliOptions, CliOutcome, ProgressEvent};
use crate::epic;
use crate::gog;
use crate::queue::{InstallJob, JobKind, JobProgress};
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
    /// Whether this fills a fresh directory or updates an existing install
    /// (install, then verify).
    pub kind: JobKind,
}

impl InstallPlan {
    /// The queue entry for this plan.
    ///
    /// The job carries the target, so a retry after a restart still installs
    /// into the directory the user saw — `run` reads the job, never the title.
    pub fn job(&self) -> InstallJob {
        let job = InstallJob::new(self.id.clone(), self.title.clone(), self.target.clone());
        match self.kind {
            JobKind::Install => job,
            JobKind::Update => job.as_update(),
        }
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
    /// The verify phase of an update run, when one ran (never for a plain
    /// install, and never for an install that was cancelled before it started).
    pub verify: Option<VerifyRun>,
}

/// What an update's verify phase reported.
///
/// Each backend judges its own verify differently — legendary always exits `0`
/// and its verdict is a summary line, nile's repair is judged by its error lines
/// and gogdl's by its exit code — so `verified` is the backend's own answer and
/// `message` is that backend's own reason when it is not `true`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct VerifyRun {
    /// The program that ran the verify (`legendary`, `nile`, `gogdl`).
    pub program: &'static str,
    /// The raw transcript of the verify, when logging was on.
    pub transcript: Option<PathBuf>,
    /// The user stopped the verify before it finished.
    pub cancelled: bool,
    /// The backend's own verdict.
    pub verified: bool,
    /// Why it did not verify, in the backend's words.
    pub message: Option<String>,
}

impl VerifyRun {
    /// A verify the user stopped, or that never ran because the install was
    /// cancelled first. Neither counts against the backend.
    fn cancelled(store: Store) -> Self {
        Self {
            program: program_for(store),
            transcript: None,
            cancelled: true,
            verified: false,
            message: None,
        }
    }
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

/// Whether this store has an installer backend.
///
/// The three CLI stores do; Steam installs through its own client, and a
/// `Local`/imported row has no installer at all. This is the one predicate the
/// GUI asks before offering Install to a row whose files are missing, so a row
/// that [`plan`] would reject never shows a button that could only error.
#[must_use]
pub const fn is_installable(store: Store) -> bool {
    matches!(store, Store::Epic | Store::Amazon | Store::Gog)
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
        kind: JobKind::Install,
    })
}

/// Plan an **update** for an installed game: install its existing id, then
/// verify the files that land.
///
/// An update fills the directory the game already occupies. The row usually
/// knows it (`Game::install_dir`, written by the library sync or a previous
/// install), and that path wins — re-deriving it from the title would move an
/// update to the store's default directory and leave the real install behind.
/// When the row does not know it, [`plan`]'s store directory is the fallback.
///
/// The plan's [`JobKind::Update`] makes [`run`] follow the install with the
/// backend's own verify, so the update rides the queue exactly like an install.
pub fn update_plan(game: &Game) -> Result<InstallPlan, InstallerError> {
    let program = match game.id.store {
        Store::Epic => epic::PROGRAM,
        Store::Amazon => amazon::PROGRAM,
        Store::Gog => gog::GAME_PROGRAM,
        other => return Err(InstallerError::Unsupported(other)),
    };
    let target = match &game.install_dir {
        Some(dir) => dir.clone(),
        // The row does not know where the game is; fall back to the store's
        // default, which is what a fresh install would have used.
        None => plan(&game.id, &game.title)?.target,
    };
    Ok(InstallPlan {
        id: game.id.clone(),
        title: game.title.clone(),
        program,
        target,
        kind: JobKind::Update,
    })
}

/// Run one queued job to completion, or until `control` cancels it.
///
/// Blocks while the store CLI works: call this on a worker thread. Every
/// change in progress is handed to `on_progress` as a [`JobProgress`], which is
/// the shape [`crate::queue::InstallQueue::progress`] takes, and the returned
/// error is already display-ready (the CLI's own message, plus the transcript
/// path when one was written). A run `control` stopped comes back as an
/// `Ok` [`InstallRun`] with `cancelled` set — it is a user decision, not a
/// store failure.
///
/// The job's [`JobKind`] decides the phases: `Install` runs the backend's
/// install once; `Update` runs the install **and then** the backend's verify, so
/// an update is the same queue entry and the same worker as an install with one
/// extra command. A verify that does not pass turns an update into an error, the
/// same way a failed install does.
pub fn run(
    backends: &Backends,
    job: &InstallJob,
    options: &CliOptions,
    control: &InstallControl,
    mut on_progress: impl FnMut(JobProgress),
) -> Result<InstallRun, String> {
    if !is_installable(job.id.store) {
        return Err(InstallerError::Unsupported(job.id.store).to_string());
    }

    let command = install_command(&job.id, &job.target);
    let report = match run_phase(
        backends,
        job.id.store,
        &command,
        options,
        control,
        &mut on_progress,
    )? {
        Phase::Cancelled => return Ok(cancelled_run(job)),
        Phase::Finished(report) => report,
    };
    if !report.succeeded() {
        return Err(failure_message(&job.id, "install", &report));
    }
    let mut install = InstallRun {
        program: program_for(job.id.store),
        target: job.target.clone(),
        transcript: report.log_path().map(Path::to_path_buf),
        cancelled: false,
        verify: None,
    };

    // A plain install is done; only an update carries the extra verify.
    if !job.kind.verifies() {
        return Ok(install);
    }

    let verify_command = verify_command(&job.id, &job.target);
    install.verify = Some(
        match run_phase(
            backends,
            job.id.store,
            &verify_command,
            options,
            control,
            &mut on_progress,
        )? {
            Phase::Cancelled => VerifyRun::cancelled(job.id.store),
            Phase::Finished(report) => verify_run(job.id.store, &report),
        },
    );

    if let Some(verify) = &install.verify
        && !verify.cancelled
        && !verify.verified
    {
        return Err(verify_failure_message(&job.id, verify));
    }
    Ok(install)
}

/// The end of one command a run drove.
enum Phase {
    /// The user stopped it (or it was already stopped before it started).
    Cancelled,
    /// It ran to completion; the report carries the backend's verdict.
    Finished(RunReport),
}

/// Spawn one backend command, stream it to `on_progress`, and judge it.
///
/// `run` calls this once for an install and, for an update, a second time for
/// the verify. The handle is registered with `control` for as long as the CLI
/// runs, so a Cancel reaches a worker blocked on a silent process.
fn run_phase(
    backends: &Backends,
    store: Store,
    command: &CliCommand,
    options: &CliOptions,
    control: &InstallControl,
    on_progress: &mut impl FnMut(JobProgress),
) -> Result<Phase, String> {
    // Cancelled before the CLI started: no process, no directory.
    if control.is_cancelled() {
        return Ok(Phase::Cancelled);
    }

    let label = command.as_str();
    let handle = Arc::new(
        spawn_backend(backends, store, command, options)
            .map_err(|error| format!("could not run the {store} {label}: {error}"))?,
    );
    if control.attach(&handle) {
        handle.cancel();
    }

    let mut tracker = ProgressTracker::default();
    loop {
        match handle.next_event() {
            Ok(Some(event)) => tracker.observe(&event, on_progress),
            Ok(None) => break,
            Err(error) => {
                control.detach();
                return Err(format!("could not read the {store} {label}: {error}"));
            }
        }
    }
    let outcome = handle.drain();
    control.detach();

    let outcome =
        outcome.map_err(|error| format!("could not read the {store} {label}: {error}"))?;
    let report = report_backend(backends, store, command, outcome);
    if report.outcome.cancelled {
        Ok(Phase::Cancelled)
    } else {
        Ok(Phase::Finished(report))
    }
}

/// The report for a run that ended before doing any work.
fn cancelled_run(job: &InstallJob) -> InstallRun {
    InstallRun {
        program: program_for(job.id.store),
        target: job.target.clone(),
        transcript: None,
        cancelled: true,
        verify: None,
    }
}

/// Judge an update's verify phase with the backend's own rule.
///
/// legendary always exits `0` (its verdict is a summary line), nile exits `0`
/// for a refusal too (its verdict is its error lines), and gogdl exits non-zero
/// on a failed repair — so each backend answers for itself rather than sharing
/// one exit-code rule.
fn verify_run(store: Store, report: &RunReport) -> VerifyRun {
    let (verified, message) = match store {
        Store::Epic => {
            let verdict = epic::verify_report(&report.text);
            let message = (!verdict.verified).then(|| {
                format!(
                    "{} file(s) corrupted, {} file(s) missing",
                    verdict.corrupted, verdict.missing
                )
            });
            (verdict.verified, message)
        }
        Store::Amazon => {
            let verdict = amazon::verify_report(report);
            let message = (!verdict.verified).then(|| verdict.errors.join("; "));
            (verdict.verified, message)
        }
        // gogdl's exit status is the verdict; `_` also covers the unsupported
        // store `run` refuses before ever getting here.
        _ => (report.succeeded(), report.failure_message()),
    };
    VerifyRun {
        program: program_for(store),
        transcript: report.log_path().map(Path::to_path_buf),
        cancelled: false,
        verified,
        message,
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

/// The verify command for an update of `id` in `target`.
///
/// The directory is always supplied: legendary and nile ignore it, and `gogdl`
/// needs it (`repair {id} --path {dir}`). A spec whose verify template does not
/// use `{dir}` simply ignores the extra value.
fn verify_command(id: &GameId, target: &Path) -> CliCommand {
    CliCommand::Verify {
        id: id.store_id.clone(),
        install_dir: Some(target.to_path_buf()),
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

/// A failed phase as one line: what the CLI said, and where the transcript is.
fn failure_message(id: &GameId, phase: &str, report: &RunReport) -> String {
    let mut message = format!(
        "{} {phase} failed: {}",
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

/// An update whose verify did not pass, as one line.
fn verify_failure_message(id: &GameId, verify: &VerifyRun) -> String {
    let mut message = format!(
        "{} update verify failed: {}",
        id.store,
        verify
            .message
            .as_deref()
            .unwrap_or("the client did not confirm the files")
    );
    if let Some(path) = &verify.transcript {
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
    fn only_the_three_cli_stores_are_installable() {
        // The one predicate the GUI gates its Install button on: a store with no
        // backend is not installable, so no button is offered for it.
        assert!(is_installable(Store::Epic));
        assert!(is_installable(Store::Amazon));
        assert!(is_installable(Store::Gog));
        assert!(!is_installable(Store::Steam), "Steam installs itself");
        assert!(
            !is_installable(Store::Local),
            "an imported row has no installer"
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

    // -- updates: an install of an existing id plus a verify ----------------

    #[test]
    fn an_update_plan_reuses_the_directory_the_game_is_installed_in() {
        let installed = tempfile::tempdir().unwrap();
        let mut game = Game::new(game(Store::Epic, "Gizmo"), "Gizmo");
        game.installed = true;
        game.install_dir = Some(installed.path().join("Gizmo"));

        let plan = update_plan(&game).unwrap();
        assert_eq!(plan.kind, JobKind::Update);
        assert_eq!(plan.program, "legendary");
        assert_eq!(plan.target, installed.path().join("Gizmo"));
        // The queue entry an update produces is an update, so `run` will verify.
        assert!(plan.job().kind.verifies());
        assert_eq!(plan.job().target, installed.path().join("Gizmo"));
    }

    #[test]
    fn an_update_plan_falls_back_to_the_stores_directory() {
        let _guard = env_lock();
        let data = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_DATA_HOME", data.path()) };

        let mut game = Game::new(game(Store::Epic, "Gizmo"), "Gizmo Deluxe");
        game.installed = true;
        let plan = update_plan(&game).unwrap();
        assert_eq!(plan.kind, JobKind::Update);
        assert_eq!(
            plan.target,
            data.path().join("pitagames/games/epic/Gizmo Deluxe")
        );

        unsafe { std::env::remove_var("XDG_DATA_HOME") };
    }

    #[test]
    fn update_plan_refuses_a_store_with_no_installer() {
        let mut game = Game::new(game(Store::Steam, "620"), "Portal 2");
        game.install_dir = Some(PathBuf::from("/games/Portal 2"));
        assert_eq!(
            update_plan(&game),
            Err(InstallerError::Unsupported(Store::Steam))
        );
    }

    #[test]
    fn an_update_installs_and_then_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let install_marker = dir.path().join("installed");
        let verify_marker = dir.path().join("verified");
        let program = script(
            dir.path(),
            "legendary",
            &format!(
                "#!/bin/sh\n\
                 case \"$1\" in\n\
                 install) : > '{installed}'; \
                 echo '[cli] INFO: = Progress: 100.00% (1000/1000)'; exit 0 ;;\n\
                 verify) : > '{verified}'; \
                 echo 'Verification finished successfully.'; exit 0 ;;\n\
                 *) echo \"unexpected: $*\" >&2; exit 2 ;;\n\
                 esac\n",
                installed = install_marker.display(),
                verified = verify_marker.display(),
            ),
        );
        let backends = backends(program.to_str().unwrap(), "/nonexistent/nile", "gogdl");
        let job = InstallJob::new(
            game(Store::Epic, "Gizmo"),
            "Gizmo",
            dir.path().join("Gizmo"),
        )
        .as_update();

        let run = run(&backends, &job, &options(), &InstallControl::new(), |_| {}).unwrap();

        assert!(install_marker.exists(), "the install phase ran");
        assert!(verify_marker.exists(), "the verify ran after the install");
        let verify = run.verify.expect("an update carries its verify");
        assert!(verify.verified);
        assert_eq!(verify.program, "legendary");
        assert!(!verify.cancelled);
        assert!(verify.transcript.is_none(), "logging was off");
    }

    #[test]
    fn a_plain_install_never_runs_a_verify() {
        let dir = tempfile::tempdir().unwrap();
        // The fake refuses anything but an install, so a stray verify would
        // fail the run instead of going unnoticed.
        let program = script(
            dir.path(),
            "legendary",
            "#!/bin/sh\n\
             if [ \"$1\" = install ]; then exit 0; fi\n\
             echo \"unexpected: $*\" >&2\n\
             exit 2\n",
        );
        let backends = backends(program.to_str().unwrap(), "/nonexistent/nile", "gogdl");
        let job = InstallJob::new(
            game(Store::Epic, "Gizmo"),
            "Gizmo",
            dir.path().join("Gizmo"),
        );

        let run = run(&backends, &job, &options(), &InstallControl::new(), |_| {}).unwrap();
        assert!(run.verify.is_none());
    }

    #[test]
    fn an_update_whose_verify_does_not_pass_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        // legendary's verify exits `0` even for corrupt files; the summary line
        // is the verdict.
        let program = script(
            dir.path(),
            "legendary",
            "#!/bin/sh\n\
             case \"$1\" in\n\
             install) exit 0 ;;\n\
             verify) echo 'Verification failed, 3 file(s) corrupted, \
             1 file(s) are missing.'; exit 0 ;;\n\
             *) exit 2 ;;\n\
             esac\n",
        );
        let backends = backends(program.to_str().unwrap(), "/nonexistent/nile", "gogdl");
        let job = InstallJob::new(
            game(Store::Epic, "Gizmo"),
            "Gizmo",
            dir.path().join("Gizmo"),
        )
        .as_update();

        let error = run(&backends, &job, &options(), &InstallControl::new(), |_| {}).unwrap_err();
        assert!(error.contains("epic update verify failed"), "{error}");
        assert!(error.contains("3 file(s) corrupted"), "{error}");
    }

    #[test]
    fn a_gog_update_judges_the_verify_by_gogdls_exit_status() {
        let dir = tempfile::tempdir().unwrap();
        // gogdl's argv is `--auth-config-path <file> <command> …`, so match the
        // command anywhere in the line rather than at a fixed position.
        let program = script(
            dir.path(),
            "gogdl",
            "#!/bin/sh\n\
             case \" $* \" in\n\
             *' download '*) exit 0 ;;\n\
             *' repair '*) echo '[gogdl] ERROR: repair failed' >&2; exit 1 ;;\n\
             *) exit 2 ;;\n\
             esac\n",
        );
        let backends = backends(
            "/nonexistent/legendary",
            "/nonexistent/nile",
            program.to_str().unwrap(),
        );
        let job = InstallJob::new(
            game(Store::Gog, "1207658930"),
            "The Witcher 3",
            dir.path().join("The Witcher 3"),
        )
        .as_update();

        let error = run(&backends, &job, &options(), &InstallControl::new(), |_| {}).unwrap_err();
        assert!(error.contains("gog update verify failed"), "{error}");
    }

    #[test]
    fn an_update_cancelled_before_it_starts_never_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        let program = script(
            dir.path(),
            "legendary",
            &format!("#!/bin/sh\n: > '{m}'\nexit 0\n", m = marker.display()),
        );
        let backends = backends(program.to_str().unwrap(), "/nonexistent/nile", "gogdl");
        let job = InstallJob::new(
            game(Store::Epic, "Gizmo"),
            "Gizmo",
            dir.path().join("Gizmo"),
        )
        .as_update();

        let control = InstallControl::new();
        control.cancel();

        let run = run(&backends, &job, &options(), &control, |_| {}).unwrap();
        assert!(run.cancelled);
        assert!(run.verify.is_none(), "nothing ran, so nothing verified");
        assert!(!marker.exists());
    }

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }
}
