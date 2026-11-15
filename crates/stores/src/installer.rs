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

use std::path::{Path, PathBuf};

use pitagames_core::{GameId, Store};

use crate::amazon;
use crate::cli::{CliCommand, CliOptions, ProgressEvent};
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

/// What a successful run did, for the Downloads row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallRun {
    /// The program that ran.
    pub program: &'static str,
    /// The directory the store filled.
    pub target: PathBuf,
    /// The raw transcript of the run, when logging was on.
    pub transcript: Option<PathBuf>,
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

/// Run one queued install to completion.
///
/// Blocks while the store CLI works: call this on a worker thread. Every
/// change in progress is handed to `on_progress` as a [`JobProgress`], which is
/// the shape [`crate::queue::InstallQueue::progress`] takes, and the returned
/// error is already display-ready (the CLI's own message, plus the transcript
/// path when one was written).
pub fn run(
    backends: &Backends,
    job: &InstallJob,
    options: &CliOptions,
    mut on_progress: impl FnMut(JobProgress),
) -> Result<InstallRun, String> {
    let command = install_command(&job.id, &job.target);
    let mut tracker = ProgressTracker::default();

    let report = match job.id.store {
        Store::Epic => backends.epic.stream(&command, options, |event| {
            tracker.observe(event, &mut on_progress)
        }),
        Store::Amazon => backends.amazon.stream(&command, options, |event| {
            tracker.observe(event, &mut on_progress)
        }),
        Store::Gog => backends.gog.stream(&command, options, |event| {
            tracker.observe(event, &mut on_progress)
        }),
        other => return Err(InstallerError::Unsupported(other).to_string()),
    }
    .map_err(|error| format!("could not run the {} install: {error}", job.id.store))?;

    if !report.succeeded() {
        return Err(failure_message(&job.id, &report));
    }

    Ok(InstallRun {
        program: program_for(job.id.store),
        target: job.target.clone(),
        transcript: report.log_path().map(|path| path.to_path_buf()),
    })
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
        let run = run(&backends, &job, &options(), |progress| seen.push(progress)).unwrap();

        assert_eq!(run.program, "legendary");
        assert_eq!(run.target, job.target);
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
        let error = run(&backends, &job, &options(), |_| {}).unwrap_err();
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
        let error = run(&backends, &job, &options(), |_| {}).unwrap_err();
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
        let error = run(&backends, &job, &options(), |_| {}).unwrap_err();
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
        let error = run(&backends, &job, &options(), |_| {}).unwrap_err();
        assert!(error.contains("no command-line install backend"), "{error}");
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
