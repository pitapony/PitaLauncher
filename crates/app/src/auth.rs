//! The store sign-in, on the shared terminal.
//!
//! The Stores screen used to answer a signed-out store with the command to run
//! in a terminal. This is the in-app replacement: a small terminal that drives
//! the store's own CLI over the shared relay ([`pitagames_stores::auth`]) and
//! shows what it says.
//!
//! The terminal itself ([`crate::terminal`]) is generic — stream, one typed line,
//! Cancel, run-id tagged — so the M3/M3.5 mod installers can drive it too. This
//! module is only the adapter: it turns a store's [`AuthRun`] into a
//! [`TerminalDriver`], so the store sign-in is just another caller of the same
//! machinery.

use std::path::Path;

use futures_channel::mpsc::UnboundedSender;
use pitagames_core::Store;
use pitagames_stores::auth::{AuthRun, AuthStep};
use pitagames_stores::cli::CliOptions;

use crate::terminal::{
    self, TerminalDriver, TerminalEvent, TerminalInputs, TerminalOutcome, TerminalStep,
};

/// A store's sign-in as the shared terminal's driver.
///
/// The relay ([`AuthRun`]) owns the prompt policy — the ENTER courtesy, the
/// `:`-terminated prompt, the verdict — so the adapter only re-shapes its steps
/// into the generic [`TerminalStep`] and its outcome into a [`TerminalOutcome`].
struct AuthDriver {
    run: AuthRun,
}

impl TerminalDriver for AuthDriver {
    fn next_step(&self) -> Result<Option<TerminalStep>, String> {
        self.run
            .next()
            .map(|step| {
                step.map(|step| match step {
                    // A CONTINUE courtesy is already answered: to the terminal it
                    // is an ordinary line with nothing to type.
                    AuthStep::Line(text) | AuthStep::Continued(text) => TerminalStep::Line(text),
                    AuthStep::Prompt { text, hint } => TerminalStep::Prompt { text, hint },
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
            .map(|outcome| TerminalOutcome {
                succeeded: outcome.succeeded(),
                cancelled: outcome.cancelled(),
                message: outcome.failure_message(),
                log: outcome.log_path().map(Path::to_path_buf),
            })
            .map_err(|error| error.to_string())
    }
}

/// Start `store`'s sign-in on the shared terminal, reporting to `events`.
///
/// The CLI is spawned **inside the worker thread** (via [`terminal::spawn`]), so
/// a missing program is reported as a failed run rather than blocking the click.
/// The window only keeps [`TerminalInputs`], so a closed terminal cancels the
/// CLI instead of leaving it waiting for an answer nobody can type.
pub fn spawn(
    run: u64,
    store: Store,
    options: CliOptions,
    events: UnboundedSender<TerminalEvent>,
) -> TerminalInputs {
    terminal::spawn(
        run,
        format!("pitagames-auth-{}", store.as_str()),
        move || {
            let run = AuthRun::start(store, &options).map_err(|error| error.to_string())?;
            let driver: Box<dyn TerminalDriver> = Box::new(AuthDriver { run });
            Ok(driver)
        },
        events,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::{Terminal, TerminalStatus};
    use futures_channel::mpsc::TryRecvError;
    use std::fs;
    use std::thread;
    use std::time::{Duration, Instant};

    /// A fake CLI named after the real program, so the default spec resolves to
    /// it through the `PATH` override.
    fn script(dir: &Path, name: &str, body: &str) {
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&path, permissions).unwrap();
        }
    }

    fn options(dir: &Path) -> CliOptions {
        let mut env = std::collections::BTreeMap::new();
        let path = std::env::var("PATH").unwrap_or_default();
        env.insert("PATH".to_string(), format!("{}:{path}", dir.display()));
        CliOptions {
            log_dir: None,
            env,
            cwd: None,
            idle_timeout: None,
            hard_timeout: Some(Duration::from_secs(20)),
        }
    }

    #[test]
    fn a_typed_answer_reaches_the_cli_and_the_run_reports_success() {
        let dir = tempfile::tempdir().unwrap();
        script(
            dir.path(),
            "legendary",
            "#!/bin/sh\n\
             printf 'Enter the authorization code: '\n\
             read code\n\
             case \"$code\" in\n\
             123456) printf '[cli] INFO: Authentication successful.\\n' ;;\n\
             *) printf '[cli] ERROR: Invalid authorization code\\n' ;;\n\
             esac\n\
             exit 0\n",
        );

        let (events, mut receiver) = futures_channel::mpsc::unbounded();
        let inputs = spawn(11, Store::Epic, options(dir.path()), events);
        let mut terminal = Terminal::new(11, "Epic");

        let deadline = Instant::now() + Duration::from_secs(20);
        let mut answered = false;
        loop {
            assert!(Instant::now() < deadline, "the fake CLI never finished");
            match receiver.try_recv() {
                Ok(event) => {
                    if let TerminalEvent::Prompt { .. } = &event
                        && !answered
                    {
                        answered = true;
                        assert!(inputs.answer("123456"));
                    }
                    terminal.apply(&event);
                    if event.is_finished() {
                        break;
                    }
                }
                Err(TryRecvError::Empty) => thread::sleep(Duration::from_millis(10)),
                Err(TryRecvError::Closed) => {
                    panic!("the channel closed before the verdict")
                }
            }
        }

        assert!(answered, "the fake CLI asked for a code");
        assert_eq!(terminal.status(), &TerminalStatus::Succeeded);
        assert!(
            terminal
                .lines()
                .iter()
                .any(|line| line.contains("Authentication successful")),
            "{:?}",
            terminal.lines()
        );
    }

    #[test]
    fn a_missing_program_is_reported_as_a_failed_run() {
        let dir = tempfile::tempdir().unwrap();
        let (events, mut receiver) = futures_channel::mpsc::unbounded();
        let _inputs = spawn(11, Store::Epic, options(dir.path()), events);
        let mut terminal = Terminal::new(11, "Epic");

        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            assert!(Instant::now() < deadline, "the failed run never reported");
            match receiver.try_recv() {
                Ok(event) => {
                    terminal.apply(&event);
                    if event.is_finished() {
                        break;
                    }
                }
                Err(TryRecvError::Empty) => thread::sleep(Duration::from_millis(10)),
                Err(TryRecvError::Closed) => panic!("the channel closed before the verdict"),
            }
        }

        match terminal.status() {
            TerminalStatus::Failed(message) => {
                assert!(message.contains("legendary"), "{message}")
            }
            other => panic!("expected a failed run, got {other:?}"),
        }
    }
}
