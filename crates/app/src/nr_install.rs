//! The in-app DLSSNR install, on the shared terminal.
//!
//! The plan (which program, which argv, which working directory) comes from
//! [`pitagames_platform::nr_install`]; the PTY relay that runs it and captures
//! its printed `%command%` lines comes from
//! [`pitagames_stores::mod_install`]. This module is only the adapter between
//! them and the one terminal ([`crate::terminal`]), exactly as [`crate::auth`]
//! is for store sign-in — so the same sheet streams the installer, takes the one
//! line it asks for, and stops it when it is closed.
//!
//! The installer's own prompt policy is never re-implemented here: the relay
//! already answers an ENTER courtesy and surfaces every real question.

use std::sync::{Arc, Mutex};

use std::path::PathBuf;

use futures_channel::mpsc::UnboundedSender;
use pitagames_core::paths;
use pitagames_mods::CapturedEnv;
use pitagames_platform::nr_install::InstallPlan;
use pitagames_platform::nr_uninstall::RemovePlan;
use pitagames_stores::cli::CliOptions;
use pitagames_stores::mod_install::{InstallRun, InstallStep};

use crate::terminal::{
    self, TerminalDriver, TerminalEvent, TerminalInputs, TerminalOutcome, TerminalStep,
};

/// Everything a started install hands back to the screen.
pub struct Started {
    /// The window's handle on the run's input, for the Send/Cancel buttons.
    pub inputs: TerminalInputs,
    /// The allowlisted env captured from the installer's printed lines, filled
    /// in when the run finishes.
    pub captured: Arc<Mutex<Option<CapturedEnv>>>,
}

/// The installer, as the shared terminal's driver.
struct InstallDriver {
    run: InstallRun,
    captured: Arc<Mutex<Option<CapturedEnv>>>,
}

impl TerminalDriver for InstallDriver {
    fn next_step(&self) -> Result<Option<TerminalStep>, String> {
        self.run
            .next()
            .map(|step| {
                step.map(|step| match step {
                    // An answered ENTER courtesy is an ordinary line to the
                    // terminal: nothing has to be typed now.
                    InstallStep::Line(text) | InstallStep::Continued(text) => {
                        TerminalStep::Line(text)
                    }
                    InstallStep::Prompt { text, hint } => TerminalStep::Prompt { text, hint },
                })
            })
            .map_err(|error| error.to_string())
    }

    fn answer(&self, line: &str) -> Result<(), String> {
        self.run.answer(line).map_err(|error| error.to_string())
    }

    fn cancel(&self) {
        self.run.cancel();
    }

    fn finish(&self) -> Result<TerminalOutcome, String> {
        self.run
            .finish()
            .map(|outcome| {
                if let Ok(mut slot) = self.captured.lock() {
                    *slot = Some(outcome.captured.clone());
                }
                TerminalOutcome {
                    succeeded: outcome.success,
                    cancelled: outcome.cancelled,
                    message: outcome.failure_message(),
                    log: outcome.transcript,
                }
            })
            .map_err(|error| error.to_string())
    }
}

/// The CLI options an install runs under.
///
/// An installer waits for a person, so only the hard cap applies (an idle
/// timeout would kill a run whose user is reading a prompt), and the transcript
/// lands in the launcher's own log directory.
pub fn install_options(cwd: std::path::PathBuf) -> CliOptions {
    CliOptions {
        log_dir: paths::logs_dir(),
        cwd: Some(cwd),
        ..CliOptions::interactive()
    }
}

/// Start an NR install on the shared terminal, reporting to `events`.
///
/// The installer is spawned **inside the worker thread** (via
/// [`terminal::spawn`]), so a missing `bash` or `install.sh` is reported as a
/// failed run rather than blocking the click. The window only keeps
/// [`Started::inputs`], so a closed sheet stops the installer instead of leaving
/// it waiting for an answer nobody can type.
pub fn spawn(run: u64, plan: InstallPlan, events: UnboundedSender<TerminalEvent>) -> Started {
    spawn_command(
        run,
        "pitagames-nr-install",
        plan.program,
        plan.args,
        plan.cwd,
        events,
    )
}

/// Start upstream's `remove` route on the shared terminal, reporting to `events`.
///
/// The uninstall runs on the same PTY relay and sheet as the install: it can ask
/// for an answer and it can be cancelled, exactly like `bash install.sh … remove`
/// would in a shell.
pub fn spawn_remove(run: u64, plan: RemovePlan, events: UnboundedSender<TerminalEvent>) -> Started {
    spawn_command(
        run,
        "pitagames-nr-uninstall",
        plan.program,
        plan.args,
        plan.cwd,
        events,
    )
}

/// The shared body of [`spawn`] and [`spawn_remove`].
fn spawn_command(
    run: u64,
    thread: &str,
    program: String,
    args: Vec<String>,
    cwd: PathBuf,
    events: UnboundedSender<TerminalEvent>,
) -> Started {
    let captured = Arc::new(Mutex::new(None));
    let driver_captured = Arc::clone(&captured);
    let inputs = terminal::spawn(
        run,
        thread.to_string(),
        move || {
            let options = install_options(cwd);
            let run =
                InstallRun::start(&program, &args, &options).map_err(|error| error.to_string())?;
            let driver: Box<dyn TerminalDriver> = Box::new(InstallDriver {
                run,
                captured: driver_captured,
            });
            Ok(driver)
        },
        events,
    );
    Started { inputs, captured }
}

/// The default label for an install's terminal sheet.
pub fn title(game: &str) -> String {
    format!("DLSSNR install — {game}")
}

/// The default label for an uninstall's terminal sheet.
pub fn uninstall_title(game: &str) -> String {
    format!("DLSSNR uninstall — {game}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::{Terminal, TerminalStatus};
    use futures_channel::mpsc::TryRecvError;
    use pitagames_core::profile::{ModelSource, NrBackend};
    use pitagames_platform::nr_install::{InstallForm, plan};
    use std::fs;
    use std::thread;
    use std::time::{Duration, Instant};

    /// A fake `install.sh` that prints a launch-option line and asks one
    /// question, using POSIX `sh` only.
    fn fixture(dir: &std::path::Path) -> InstallPlan {
        let package = dir.join("pkg");
        fs::create_dir_all(&package).unwrap();
        fs::write(
            package.join("install.sh"),
            "#!/bin/sh\n\
             printf 'Enter the starting network: '\n\
             read network\n\
             echo \"network=$network\"\n\
             echo 'WINEDLLOVERRIDES=\"dxgi=n,b\" %command%'\n",
        )
        .unwrap();
        let exe = dir.join("Game/Game.exe");
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        fs::write(&exe, b"MZ").unwrap();
        let form = InstallForm::new(NrBackend::Rdna4Upstream, 100, ModelSource::CachedInPackage);
        plan(&form, &package, &exe).unwrap()
    }

    #[test]
    fn a_plan_runs_on_the_shared_terminal_and_reports_the_capture() {
        let dir = tempfile::tempdir().unwrap();
        let plan = fixture(dir.path());

        let (events, mut receiver) = futures_channel::mpsc::unbounded();
        let run = terminal::next_run();
        let started = spawn(run, plan, events);

        let mut terminal = Terminal::new(run, "DLSSNR install — Test");
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            assert!(Instant::now() < deadline, "the installer never finished");
            match receiver.try_recv() {
                Ok(event) => {
                    let finished = event.is_finished();
                    if let TerminalEvent::Prompt { .. } = &event {
                        assert!(started.inputs.answer("cc_small_fp8"));
                    }
                    terminal.apply(&event);
                    if finished {
                        break;
                    }
                }
                Err(TryRecvError::Empty) => thread::sleep(Duration::from_millis(5)),
                Err(TryRecvError::Closed) => panic!("closed before the verdict"),
            }
        }

        assert_eq!(terminal.status(), &TerminalStatus::Succeeded);
        let captured = started
            .captured
            .lock()
            .unwrap()
            .clone()
            .expect("the capture was recorded");
        assert_eq!(
            captured.env.get("WINEDLLOVERRIDES").map(String::as_str),
            Some("dxgi=n,b"),
            "the installer's %command% line fed mods::capture"
        );
    }

    #[test]
    fn an_upstream_remove_runs_on_the_shared_terminal() {
        let dir = tempfile::tempdir().unwrap();
        // A fake upstream `install.sh` whose `remove` route deletes the ledger
        // `$1` is the game directory, as the real one's argv has it.
        let package = dir.path().join("pkg");
        fs::create_dir_all(&package).unwrap();
        fs::write(
            package.join("install.sh"),
            "#!/bin/sh\nrm -f \"$1/dlssnr-amd-install.txt\"\necho \"Uninstalled from $1.\"\n",
        )
        .unwrap();
        let game = dir.path().join("Game");
        fs::create_dir_all(&game).unwrap();
        fs::write(game.join("dlssnr-amd-install.txt"), b"dxgi.dll\n").unwrap();

        let plan = pitagames_platform::nr_uninstall::remove_plan(&package, &game).unwrap();
        let (events, mut receiver) = futures_channel::mpsc::unbounded();
        let run = terminal::next_run();
        let _started = spawn_remove(run, plan, events);

        let mut terminal = Terminal::new(run, "DLSSNR uninstall — Test");
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            assert!(Instant::now() < deadline, "the remove never finished");
            match receiver.try_recv() {
                Ok(event) => {
                    let finished = event.is_finished();
                    terminal.apply(&event);
                    if finished {
                        break;
                    }
                }
                Err(TryRecvError::Empty) => thread::sleep(Duration::from_millis(5)),
                Err(TryRecvError::Closed) => panic!("closed before the verdict"),
            }
        }

        assert_eq!(terminal.status(), &TerminalStatus::Succeeded);
        assert!(
            !game.join("dlssnr-amd-install.txt").exists(),
            "upstream's own remove deleted by its ledger"
        );
        assert!(
            terminal
                .lines()
                .iter()
                .any(|line| line.contains("Uninstalled")),
            "the installer's own line streamed through: {:?}",
            terminal.lines()
        );
    }
}
