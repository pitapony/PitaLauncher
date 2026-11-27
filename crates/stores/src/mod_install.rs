//! Run an upstream mod installer in a PTY and read what it prints.
//!
//! DLSSNR-AMD (and, later, HelixSR) ships an unmodified `install.sh` that must be
//! run as-is: ADR-0024 forbids reimplementing it, and its license forbids
//! editing it. So the launcher's job is only to drive it — allocate a PTY with
//! the game's executable directory as the working directory, stream its output,
//! pass on the answers a person types, and remember what it printed.
//!
//! This module is the relay, and it is the *second* caller of the shared PTY
//! driver ([`crate::cli::CliHandle`]) after [`crate::auth`] — same handle, same
//! timeouts, same process-group cancel, same per-run transcript, same two-lock
//! poll that lets one thread read while another types.
//!
//! Two rules shape the prompt policy, and both are "never guess":
//!
//! - an **acknowledgment** (`Press ENTER to continue`) carries no choice, so the
//!   relay answers it and reports it as [`InstallStep::Continued`];
//! - everything else that looks like a question — a line ending in `:` or `?` —
//!   is surfaced as [`InstallStep::Prompt`] for a typed answer. The installer's
//!   route, network, processing and model prompts are its own business; the
//!   launcher never picks one for the user.
//!
//! On top of the stream it does the one thing the plan asks of every installer:
//! every line is handed to [`pitagames_mods::capture`], so a printed
//! `WINEDLLOVERRIDES=… %command%` line becomes the profile's `captured_env` with
//! no copy-and-paste step.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pitagames_mods::capture::{self, CapturedEnv};

use crate::cli::{CliError, CliHandle, CliOptions, ProgressEvent};

/// The transcript label used in the per-run log file name.
const LABEL: &str = "install";

/// How many printed lines the relay keeps for the capture pass. Bounded so an
/// installer that decides to chatter cannot grow the launcher without limit.
const MAX_LINES: usize = 10_000;

/// Markers on a line that only asks the user to acknowledge it.
pub const CONTINUE_MARKERS: [&str; 3] = ["press enter", "press return", "press any key"];

/// The sentence shown beside the input when the installer asks a question.
pub const PROMPT_HINT: &str =
    "Type the installer's answer and press Send — PitaGames never guesses an installer's choice.";

/// How a printed line is classified by the relay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// An ordinary line; show it as it is.
    Line,
    /// An acknowledgment-only prompt ("press ENTER"); auto-answered.
    Continued,
    /// A question waiting for a typed value.
    Prompt,
}

/// Classify one printed line.
///
/// Pure, so the policy is tested without a process: a line mentioning a
/// continue marker is a courtesy, a line ending in `:` or `?` is a question,
/// and everything else is ordinary output.
pub fn classify(line: &str) -> LineKind {
    let lower = line.to_ascii_lowercase();
    if CONTINUE_MARKERS.iter().any(|marker| lower.contains(marker)) {
        return LineKind::Continued;
    }
    let trimmed = line.trim_end();
    if trimmed.ends_with(':') || trimmed.ends_with('?') {
        return LineKind::Prompt;
    }
    LineKind::Line
}

/// One step of an installer run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallStep {
    /// A line the installer printed; show it as it is.
    Line(String),
    /// An acknowledgment prompt the relay already answered; show the line, but
    /// nothing has to be typed.
    Continued(String),
    /// A question the installer asked; show it and take a typed answer.
    Prompt { text: String, hint: String },
}

/// A finished installer run, judged the way an installer has to be.
#[derive(Debug, Clone)]
pub struct InstallOutcome {
    /// The installer exited `0`, was not cancelled and did not time out.
    pub success: bool,
    /// The person stopped it (a Stop button, a closed sheet).
    pub cancelled: bool,
    /// The process exit code (`1` when it died from a signal).
    pub code: u32,
    /// The per-run raw transcript, when logging was enabled.
    pub transcript: Option<PathBuf>,
    /// Every `%command%` line's allowlisted env, read from the transcript.
    pub captured: CapturedEnv,
    /// The lines the installer printed, oldest first.
    pub lines: Vec<String>,
}

impl InstallOutcome {
    /// Whether anything was captured from a printed `%command%` line.
    pub fn captured_anything(&self) -> bool {
        !self.captured.is_empty()
    }

    /// One line explaining a failure, for the terminal's status.
    pub fn failure_message(&self) -> String {
        if self.cancelled {
            return "cancelled by user".to_string();
        }
        self.lines
            .iter()
            .rev()
            .find(|line| !line.trim().is_empty())
            .map(|line| line.chars().take(200).collect())
            .unwrap_or_else(|| format!("the installer exited with code {}", self.code))
    }
}

/// A mod installer in progress.
///
/// Cheap to share through an [`Arc`] behind the handle: the terminal's reader
/// thread drives [`next`](Self::next) while its input thread calls
/// [`answer`](Self::answer), the same two-lock arrangement the auth relay uses.
pub struct InstallRun {
    handle: Arc<CliHandle>,
    lines: Mutex<Vec<String>>,
}

impl std::fmt::Debug for InstallRun {
    /// The handle is a live process, so only the transcript is printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstallRun")
            .field("transcript", &self.handle.log_path())
            .finish_non_exhaustive()
    }
}

impl InstallRun {
    /// Start `program` with `args` on the shared PTY driver.
    ///
    /// `options` should be [`CliOptions::interactive`]: an installer waits for a
    /// person, so an idle timeout would kill a run whose user is reading a
    /// prompt. The caller sets `cwd` to the game's executable directory.
    pub fn start(program: &str, args: &[String], options: &CliOptions) -> Result<Self, CliError> {
        let handle = CliHandle::spawn_command(program, args, LABEL, options)?;
        Ok(Self {
            handle: Arc::new(handle),
            lines: Mutex::new(Vec::new()),
        })
    }

    /// The per-run raw transcript, when logging is enabled.
    pub fn transcript(&self) -> Option<PathBuf> {
        self.handle.log_path()
    }

    /// The next thing the installer wants, or `None` when its stream ended.
    ///
    /// A caller that gets `None` must still call [`finish`](Self::finish): the
    /// verdict is the outcome, not the end of the stream.
    pub fn next(&self) -> Result<Option<InstallStep>, CliError> {
        loop {
            let Some(event) = self.handle.next_event()? else {
                return Ok(None);
            };
            let text = match event {
                ProgressEvent::Line(text)
                | ProgressEvent::Stage(text)
                | ProgressEvent::Percent(_, text) => text,
                // The terminal event is the verdict's business, not a step.
                ProgressEvent::Done | ProgressEvent::Failed { .. } => continue,
            };
            self.record(&text);
            match classify(&text) {
                LineKind::Continued => {
                    // Answering here is the whole reason the courtesy is
                    // special: it is not a question.
                    self.handle.write_stdin(b"\n")?;
                    return Ok(Some(InstallStep::Continued(text)));
                }
                LineKind::Prompt => {
                    return Ok(Some(InstallStep::Prompt {
                        text,
                        hint: PROMPT_HINT.to_string(),
                    }));
                }
                LineKind::Line => return Ok(Some(InstallStep::Line(text))),
            }
        }
    }

    /// Answer the question [`InstallStep::Prompt`] asked for.
    ///
    /// The newline is added here: an installer reads a line, and a caller that
    /// forgot it would look like a hang.
    pub fn answer(&self, text: &str) -> Result<(), CliError> {
        let mut line = text.trim_end_matches(['\n', '\r']).to_string();
        line.push('\n');
        self.handle.write_stdin(line.as_bytes())
    }

    /// Ask the installer to stop. Idempotent; the stream drains to its terminal
    /// event and [`finish`](Self::finish) reports it as cancelled.
    pub fn cancel(&self) {
        self.handle.cancel();
    }

    /// Drain the run, capture every `%command%` line and report the verdict.
    ///
    /// Safe after [`next`](Self::next) returned `None`, and safe from a
    /// different thread than the one reading: it only reaps and reports.
    pub fn finish(&self) -> Result<InstallOutcome, CliError> {
        let outcome = self.handle.drain()?;
        let lines = match self.lines.lock() {
            Ok(lines) => lines.clone(),
            Err(_) => Vec::new(),
        };
        let captured = capture::capture(&lines.join("\n"));
        Ok(InstallOutcome {
            success: outcome.success,
            cancelled: outcome.cancelled,
            code: outcome.code,
            transcript: outcome.log_path,
            captured,
            lines,
        })
    }

    fn record(&self, text: &str) {
        if let Ok(mut lines) = self.lines.lock()
            && lines.len() < MAX_LINES
        {
            lines.push(text.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;
    use std::time::Duration;

    /// A fake `install.sh` in `dir`, executable, using POSIX `sh` only.
    fn installer(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("install.sh");
        fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&path, permissions).unwrap();
        }
        path
    }

    /// Options whose `PATH` finds `bash`, with a transcript under `dir`.
    fn options(dir: &Path) -> CliOptions {
        let mut env = BTreeMap::new();
        let path = std::env::var("PATH").unwrap_or_default();
        env.insert("PATH".to_string(), path);
        CliOptions {
            log_dir: Some(dir.join("logs")),
            env,
            cwd: Some(dir.to_path_buf()),
            idle_timeout: None,
            hard_timeout: Some(Duration::from_secs(20)),
        }
    }

    /// Drain a run to the end of its stream, collecting the steps; `react` may
    /// answer each `Prompt`.
    fn drain(run: &InstallRun, mut react: impl FnMut(&InstallStep)) -> Vec<InstallStep> {
        let mut steps = Vec::new();
        while let Some(step) = run.next().unwrap() {
            react(&step);
            steps.push(step);
        }
        steps
    }

    fn lines(steps: &[InstallStep]) -> Vec<&str> {
        steps
            .iter()
            .filter_map(|step| match step {
                InstallStep::Line(text) | InstallStep::Continued(text) => Some(text.as_str()),
                InstallStep::Prompt { .. } => None,
            })
            .collect()
    }

    #[test]
    fn classify_separates_courtesies_questions_and_output() {
        assert_eq!(classify("Press ENTER to continue"), LineKind::Continued);
        assert_eq!(classify("press return when done"), LineKind::Continued);
        assert_eq!(classify("Select the network:"), LineKind::Prompt);
        assert_eq!(classify("  Which model?  "), LineKind::Prompt);
        assert_eq!(classify("Installing to /games/Game"), LineKind::Line);
        assert_eq!(classify("done."), LineKind::Line);
    }

    #[test]
    fn a_fixture_installer_runs_to_completion_and_the_transcript_is_captured() {
        let dir = tempfile::tempdir().unwrap();
        let script = installer(
            dir.path(),
            "echo 'Installing DLSSNR-AMD'\n\
             echo 'Set these Steam launch options:'\n\
             echo 'WINEDLLOVERRIDES=\"dxgi=n,b\" %command%'\n\
             echo 'done'\n",
        );
        let args = vec![script.to_string_lossy().into_owned()];
        let run = InstallRun::start("bash", &args, &options(dir.path())).unwrap();

        let steps = drain(&run, |_| {});
        assert!(lines(&steps).contains(&"Installing DLSSNR-AMD"));
        assert!(lines(&steps).contains(&"done"));

        let outcome = run.finish().unwrap();
        assert!(outcome.success, "{outcome:?}");
        assert!(!outcome.cancelled);
        assert_eq!(
            outcome
                .captured
                .env
                .get("WINEDLLOVERRIDES")
                .map(String::as_str),
            Some("dxgi=n,b"),
            "the printed %command% line is captured"
        );

        let transcript = outcome.transcript.expect("a transcript path");
        let text = fs::read_to_string(&transcript).unwrap();
        assert!(text.contains("Installing DLSSNR-AMD"), "{text}");
        assert!(
            text.contains("%command%"),
            "the raw transcript keeps the line"
        );
    }

    #[test]
    fn an_unknown_prompt_is_surfaced_and_the_typed_answer_passed_through() {
        let dir = tempfile::tempdir().unwrap();
        let script = installer(
            dir.path(),
            "printf 'Enter the starting network: '\n\
             read network\n\
             echo \"you chose $network\"\n",
        );
        let args = vec![script.to_string_lossy().into_owned()];
        let run = InstallRun::start("bash", &args, &options(dir.path())).unwrap();

        let mut answered = false;
        let steps = drain(&run, |step| {
            if let InstallStep::Prompt { hint, .. } = step {
                assert!(hint.contains("never guesses"));
                run.answer("cc_small_fp8").unwrap();
                answered = true;
            }
        });

        assert!(answered, "the question was surfaced: {steps:?}");
        assert!(
            lines(&steps)
                .iter()
                .any(|line| line.contains("you chose cc_small_fp8")),
            "the typed answer reached the installer: {steps:?}"
        );
        assert!(run.finish().unwrap().success);
    }

    #[test]
    fn an_enter_courtesy_is_answered_without_a_person() {
        let dir = tempfile::tempdir().unwrap();
        let script = installer(
            dir.path(),
            "printf 'Press ENTER to continue'\n\
             read _continue\n\
             echo 'continuing'\n",
        );
        let args = vec![script.to_string_lossy().into_owned()];
        let run = InstallRun::start("bash", &args, &options(dir.path())).unwrap();

        let steps = drain(&run, |_| {});
        assert!(
            steps
                .iter()
                .any(|step| matches!(step, InstallStep::Continued(_))),
            "the courtesy is reported as Continued, not a question: {steps:?}"
        );
        assert!(
            !steps
                .iter()
                .any(|step| matches!(step, InstallStep::Prompt { .. })),
            "an acknowledgment is not surfaced as a question"
        );
        assert!(lines(&steps).contains(&"continuing"));
        assert!(run.finish().unwrap().success);
    }

    #[test]
    fn the_installer_runs_in_the_game_directory() {
        let dir = tempfile::tempdir().unwrap();
        let game = dir.path().join("Game");
        fs::create_dir_all(&game).unwrap();
        let script = installer(dir.path(), "printf 'cwd='; pwd\n");
        let args = vec![script.to_string_lossy().into_owned()];

        let mut opts = options(dir.path());
        opts.cwd = Some(game.clone());
        let run = InstallRun::start("bash", &args, &opts).unwrap();

        let steps = drain(&run, |_| {});
        let canonical = game.canonicalize().unwrap();
        assert!(
            lines(&steps)
                .iter()
                .any(|line| line.contains(&*canonical.to_string_lossy())),
            "the installer ran in {canonical:?}: {steps:?}"
        );
    }

    #[test]
    fn cancelling_the_installer_reports_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let script = installer(
            dir.path(),
            "echo 'working'\n\
             sleep 30\n",
        );
        let args = vec![script.to_string_lossy().into_owned()];
        let run = InstallRun::start("bash", &args, &options(dir.path())).unwrap();

        // Read the first line, then stop the run.
        let first = run.next().unwrap().expect("a line");
        assert!(matches!(first, InstallStep::Line(_)));
        run.cancel();

        // Drain to the end; the verdict is cancelled, not a success.
        while run.next().unwrap().is_some() {}
        let outcome = run.finish().unwrap();
        assert!(outcome.cancelled, "{outcome:?}");
        assert!(!outcome.success);
    }
}
