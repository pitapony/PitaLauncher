//! The three stores' browser sign-ins, as one relay.
//!
//! Epic (`legendary auth --disable-webview`), Amazon (`nile auth --login`) and
//! GOG (`lgogdownloader --browser-login`) all sign in the same way from a
//! launcher's point of view: the CLI opens a page (or prints its address), asks
//! for one pasted line — an authorization code, or the URL the browser was
//! redirected to — and stores the session itself. The differences are only in
//! the copy and in Amazon's extra "press ENTER to open the browser" step, which
//! is the sort of detail that ends up copied into three call sites and then
//! drifts.
//!
//! This module is the one relay. It drives a [`CliHandle`] started on
//! [`CliCommand::Auth`], classifies each line into a [`AuthStep`], auto-answers
//! the ENTER-only prompt, and judges the finished run with the backend's own
//! log format ([`RunReport::succeeded`] — every one of these CLIs can log an
//! error and still exit `0`).
//!
//! ```text
//!  AuthRun::start(store) ──► CliHandle ──next──► AuthStep::{Line, Prompt, Continued}
//!        │                        │                    │
//!        │                        └──answer──────► pasted code / URL
//!        └──finish()──► AuthOutcome { report, cancelled }
//! ```
//!
//! The CLI drives it from `stdin`; the GUI drives it from the in-app terminal.
//! Neither owns the prompt policy, and neither has to know which line means
//! "type something".

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pitagames_core::Store;

use crate::cli::{CliCommand, CliError, CliHandle, CliOptions, CliSpec, ProgressEvent};
use crate::report::{LogFormat, RunReport};

/// The command label used in the transcript name and the verdict.
const COMMAND: &str = "auth";

/// nile pauses for ENTER before it opens the login page; the relay answers it so
/// the only thing a person types is the value the store asked for.
const CONTINUE_MARKER: &str = "Press ENTER";

/// Whether this store signs in through a CLI at all.
///
/// Steam has no sign-in of its own to drive (it starts in its own client), and
/// a local/imported row has no account.
pub const fn has_sign_in(store: Store) -> bool {
    matches!(store, Store::Epic | Store::Amazon | Store::Gog)
}

/// The CLI preset that signs `store` in, with its default program.
///
/// `None` for a store [`has_sign_in`] rejects. A caller with its own program
/// override (`pitagames-cli --program`) spawns the handle itself and hands it to
/// [`AuthRun::attach`].
pub fn spec_for(store: Store) -> Option<CliSpec> {
    match store {
        Store::Epic => Some(crate::epic::spec()),
        Store::Amazon => Some(crate::amazon::spec()),
        Store::Gog => Some(crate::gog::spec()),
        _ => None,
    }
}

/// The log layout the store's sign-in CLI writes.
///
/// `legendary` and `lgogdownloader` share one bracketed layout, so both are
/// [`LogFormat::Legendary`]; `gogdl` — the CLI `LogFormat::Gogdl` names — is not
/// the program that signs anyone in. The per-store value is what keeps a
/// transcript labelled honestly.
pub const fn log_format(store: Store) -> LogFormat {
    match store {
        Store::Amazon => LogFormat::Nile,
        _ => LogFormat::Legendary,
    }
}

/// What the person should paste when `store`'s CLI asks.
///
/// The CLI's own prompt line (a legend, with the address it opened and the
/// field it wants) is printed above this in both callers; this is the sentence
/// that says what to do with it.
pub const fn hint(store: Store) -> &'static str {
    match store {
        Store::Epic => {
            "Sign in on the page that was opened, then copy the authorizationCode from the JSON \
             response and paste it here."
        }
        Store::Amazon => {
            "Sign in in the browser that was opened, then copy the amazon.com address you were \
             redirected to and paste it here."
        }
        Store::Gog => {
            "Sign in in the browser that was opened, then copy the full URL you were redirected \
             to and paste it here."
        }
        _ => "Paste the value the sign-in page showed you.",
    }
}

/// One step of a sign-in run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthStep {
    /// A line the CLI printed; show it as it is.
    Line(String),
    /// The CLI is waiting for one typed line (a code or a redirected URL).
    Prompt { text: String, hint: String },
    /// The CLI's ENTER-only prompt, already answered by the relay; show the
    /// line, but nothing has to be typed.
    Continued(String),
}

/// Why a sign-in could not start.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("{0} has no command-line sign-in")]
    Unsupported(Store),

    #[error("{0}")]
    Cli(#[from] CliError),
}

/// A finished sign-in, as the store's own CLI judged it.
#[derive(Debug, Clone)]
pub struct AuthOutcome {
    /// The finished run: the exit status, the CLI's log levels and the raw text.
    pub report: RunReport,
}

impl AuthOutcome {
    /// A session was stored: the CLI exited `0`, logged no error, and the run
    /// was not cancelled. This is [`RunReport::succeeded`], because every one of
    /// these CLIs can refuse and still exit `0`.
    pub fn succeeded(&self) -> bool {
        self.report.succeeded()
    }

    /// The user cancelled the flow (a closed terminal, a Stop button).
    pub fn cancelled(&self) -> bool {
        self.report.outcome.cancelled
    }

    /// Why the sign-in did not finish, in one line.
    pub fn failure_message(&self) -> String {
        self.report
            .failure_message()
            .unwrap_or_else(|| "no session was stored".to_string())
    }

    /// The per-run raw transcript, when logging was enabled.
    pub fn log_path(&self) -> Option<&Path> {
        self.report.log_path()
    }
}

/// A sign-in in progress.
///
/// Cheap to clone through an [`Arc`]: the CLI's reader thread holds one while
/// the window's input thread holds another, which is why the handle is shared
/// rather than owned. Every method takes `&self`.
pub struct AuthRun {
    store: Store,
    handle: Arc<CliHandle>,
}

impl std::fmt::Debug for AuthRun {
    /// The handle is a live process, so only the store is printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthRun")
            .field("store", &self.store)
            .finish_non_exhaustive()
    }
}

impl AuthRun {
    /// Start `store`'s sign-in with its default program.
    ///
    /// `options` should be [`CliOptions::interactive`]: an auth flow waits for a
    /// person, so an idle timeout would kill a run whose user is reading a
    /// browser tab.
    pub fn start(store: Store, options: &CliOptions) -> Result<Self, AuthError> {
        let spec = spec_for(store).ok_or(AuthError::Unsupported(store))?;
        Ok(Self::attach(
            store,
            CliHandle::spawn(&spec, &CliCommand::Auth, options)?,
        ))
    }

    /// Wrap a handle the caller spawned itself (a program override, a test).
    pub fn attach(store: Store, handle: CliHandle) -> Self {
        Self {
            store,
            handle: Arc::new(handle),
        }
    }

    pub const fn store(&self) -> Store {
        self.store
    }

    /// The per-run raw transcript, when logging was enabled.
    ///
    /// A PTY echoes what is typed, so this file can contain the pasted code or
    /// URL: it is for local diagnostics only, never parsed back.
    pub fn log_path(&self) -> Option<PathBuf> {
        self.handle.log_path()
    }

    /// The next thing the flow wants, or `None` when the CLI's stream ended.
    ///
    /// A caller that gets `None` must still call [`finish`](Self::finish): the
    /// verdict is the report, not the end of the stream.
    pub fn next(&self) -> Result<Option<AuthStep>, CliError> {
        let Some(event) = self.handle.next_event()? else {
            return Ok(None);
        };

        let step = match event {
            ProgressEvent::Line(text)
            | ProgressEvent::Stage(text)
            | ProgressEvent::Percent(_, text) => {
                if text.contains(CONTINUE_MARKER) {
                    // Answering here is the whole reason this module exists: the
                    // prompt is a courtesy, not a question.
                    self.handle.write_stdin(b"\n")?;
                    AuthStep::Continued(text)
                } else if text.trim_end().ends_with(':') {
                    AuthStep::Prompt {
                        text,
                        hint: hint(self.store).to_string(),
                    }
                } else {
                    AuthStep::Line(text)
                }
            }
            // The terminal event is the report's business, not a step.
            ProgressEvent::Done | ProgressEvent::Failed { .. } => return self.next(),
        };

        Ok(Some(step))
    }

    /// Answer the prompt [`AuthStep::Prompt`] asked for.
    ///
    /// The newline is added here: every one of these CLIs reads a line, and a
    /// caller that forgot it would look like a hang.
    pub fn answer(&self, text: &str) -> Result<(), CliError> {
        let mut line = text.trim_end_matches(['\n', '\r']).to_string();
        line.push('\n');
        self.handle.write_stdin(line.as_bytes())
    }

    /// Ask the flow to stop. Idempotent; the stream drains to its terminal
    /// event and [`finish`](Self::finish) reports it as cancelled.
    pub fn cancel(&self) {
        self.handle.cancel();
    }

    /// Drain the run and judge it with the store's own log format.
    ///
    /// Safe after [`next`](Self::next) returned `None`, and safe from a
    /// different thread than the one reading: it only reaps and reports.
    pub fn finish(&self) -> Result<AuthOutcome, CliError> {
        let outcome = self.handle.drain()?;
        Ok(AuthOutcome {
            report: RunReport::new(COMMAND, outcome, log_format(self.store)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;

    /// A fake CLI in `dir`, executable, named after the real program so
    /// [`spec_for`]'s default name resolves to it through `PATH`.
    fn fake(dir: &Path, name: &str, body: &str) {
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

    /// Options whose `PATH` finds the fakes in `dir`, and whose run cannot hang:
    /// the hard cap is the only timeout an auth flow has.
    fn options(dir: &Path) -> CliOptions {
        let mut env = std::collections::BTreeMap::new();
        let path = std::env::var("PATH").unwrap_or_default();
        env.insert("PATH".to_string(), format!("{}:{path}", dir.display()));
        CliOptions {
            env,
            idle_timeout: None,
            hard_timeout: Some(Duration::from_secs(20)),
            ..CliOptions::default()
        }
    }

    /// Drain the run to the end of its stream, collecting the steps.
    fn drain(run: &AuthRun) -> Vec<AuthStep> {
        let mut steps = Vec::new();
        while let Some(step) = run.next().unwrap() {
            steps.push(step);
        }
        steps
    }

    #[test]
    fn a_store_without_a_sign_in_is_rejected_by_name() {
        assert!(!has_sign_in(Store::Steam));
        assert!(matches!(
            AuthRun::start(Store::Steam, &CliOptions::interactive()),
            Err(AuthError::Unsupported(Store::Steam))
        ));
        assert!(matches!(
            AuthRun::start(Store::Local, &CliOptions::interactive()),
            Err(AuthError::Unsupported(Store::Local))
        ));
        for store in [Store::Epic, Store::Amazon, Store::Gog] {
            assert!(has_sign_in(store), "{store} signs in through a CLI");
            assert!(spec_for(store).is_some());
        }
    }

    #[test]
    fn a_missing_program_is_a_cli_error() {
        let dir = tempfile::tempdir().unwrap();
        let error = AuthRun::start(Store::Epic, &options(dir.path())).unwrap_err();
        assert!(matches!(error, AuthError::Cli(CliError::NotFound { .. })));
        assert!(error.to_string().contains("legendary"), "{error}");
    }

    #[test]
    fn a_code_paste_prompt_asks_for_a_line_and_the_run_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        fake(
            dir.path(),
            "legendary",
            "#!/bin/sh\n\
             printf 'Please open https://epic.example/login in a browser\\n'\n\
             printf 'Enter the authorization code: '\n\
             read code\n\
             case \"$code\" in\n\
             123456) printf '[cli] INFO: Authentication successful.\\n' ;;\n\
             *) printf '[cli] ERROR: Invalid authorization code\\n' ;;\n\
             esac\n\
             exit 0\n",
        );

        let run = AuthRun::start(Store::Epic, &options(dir.path())).unwrap();
        assert_eq!(run.store(), Store::Epic);

        let first = run.next().unwrap().unwrap();
        assert!(
            matches!(&first, AuthStep::Line(text) if text.contains("epic.example/login")),
            "{first:?}"
        );

        match run.next().unwrap().unwrap() {
            AuthStep::Prompt { text, hint } => {
                assert_eq!(text, "Enter the authorization code:");
                assert!(hint.contains("authorizationCode"), "{hint}");
            }
            other => panic!("expected a prompt, got {other:?}"),
        }

        run.answer("123456\n").unwrap();
        let rest = drain(&run);
        assert!(
            rest.iter().any(|step| {
                matches!(step, AuthStep::Line(text) if text.contains("Authentication successful"))
            }),
            "{rest:?}"
        );

        let outcome = run.finish().unwrap();
        assert!(outcome.succeeded(), "{}", outcome.failure_message());
        assert!(!outcome.cancelled());
    }

    #[test]
    fn a_refusal_that_exits_zero_is_not_a_success() {
        let dir = tempfile::tempdir().unwrap();
        fake(
            dir.path(),
            "legendary",
            "#!/bin/sh\n\
             printf 'Enter the authorization code: '\n\
             read code\n\
             printf '[cli] ERROR: Invalid authorization code\\n'\n\
             exit 0\n",
        );

        let run = AuthRun::start(Store::Epic, &options(dir.path())).unwrap();
        assert!(matches!(
            run.next().unwrap().unwrap(),
            AuthStep::Prompt { .. }
        ));
        run.answer("000000").unwrap();

        // The PTY echoes what was typed, so the pasted line comes back as
        // output — which is why an auth transcript is local diagnostics only.
        let rest = drain(&run);
        assert!(
            rest.iter()
                .any(|step| matches!(step, AuthStep::Line(text) if text == "000000")),
            "{rest:?}"
        );
        assert!(
            rest.iter().any(|step| {
                matches!(step, AuthStep::Line(text) if text.contains("Invalid authorization code"))
            }),
            "{rest:?}"
        );

        let outcome = run.finish().unwrap();
        assert!(!outcome.succeeded(), "{:?}", outcome.report.text);
        assert!(
            outcome
                .failure_message()
                .contains("Invalid authorization code"),
            "{}",
            outcome.failure_message()
        );
    }

    #[test]
    fn a_press_enter_notice_is_answered_and_does_not_ask_for_a_line() {
        let dir = tempfile::tempdir().unwrap();
        fake(
            dir.path(),
            "nile",
            "#!/bin/sh\n\
             printf 'Please open this URL: https://www.amazon.com/ap/signin\\n'\n\
             printf 'Press ENTER to continue...'\n\
             read enter\n\
             printf 'INFO [CLI]:\\t Enter the URL you were redirected to: '\n\
             read url\n\
             printf 'INFO [CLI]:\\t Signed in as player\\n'\n\
             exit 0\n",
        );

        let run = AuthRun::start(Store::Amazon, &options(dir.path())).unwrap();

        let first = run.next().unwrap().unwrap();
        assert!(
            matches!(&first, AuthStep::Line(text) if text.contains("amazon.com/ap/signin")),
            "{first:?}"
        );
        let second = run.next().unwrap().unwrap();
        assert!(
            matches!(&second, AuthStep::Continued(text) if text.contains("Press ENTER")),
            "{second:?}"
        );
        match run.next().unwrap().unwrap() {
            AuthStep::Prompt { text, hint } => {
                assert!(
                    text.contains("Enter the URL you were redirected to"),
                    "{text}"
                );
                assert!(hint.contains("amazon.com address"), "{hint}");
            }
            other => panic!("expected the URL prompt, got {other:?}"),
        }

        run.answer("https://www.amazon.com/ap/redirect?code=abc")
            .unwrap();
        let rest = drain(&run);
        assert!(
            rest.iter().any(
                |step| matches!(step, AuthStep::Line(text) if text.contains("Signed in as player"))
            ),
            "{rest:?}"
        );
        assert!(run.finish().unwrap().succeeded());
    }

    #[test]
    fn cancel_ends_the_run_and_is_reported_as_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        fake(
            dir.path(),
            "lgogdownloader",
            "#!/bin/sh\n\
             printf 'Please log in in the browser\\n'\n\
             read url\n\
             exit 0\n",
        );

        let run = AuthRun::start(Store::Gog, &options(dir.path())).unwrap();
        assert!(matches!(run.next().unwrap().unwrap(), AuthStep::Line(_)));
        run.cancel();
        let _ = drain(&run);

        let outcome = run.finish().unwrap();
        assert!(outcome.cancelled());
        assert!(!outcome.succeeded());
        assert_eq!(outcome.failure_message(), "cancelled by user");
    }

    #[test]
    fn every_store_gets_its_own_prompt_copy() {
        assert!(hint(Store::Epic).contains("authorizationCode"));
        assert!(hint(Store::Amazon).contains("amazon.com"));
        assert!(hint(Store::Gog).contains("URL"));
        for (left, right) in [
            (Store::Epic, Store::Amazon),
            (Store::Epic, Store::Gog),
            (Store::Amazon, Store::Gog),
        ] {
            assert_ne!(hint(left), hint(right), "{left} and {right} share copy");
        }
        assert_eq!(log_format(Store::Amazon), LogFormat::Nile);
        assert_eq!(
            log_format(Store::Gog),
            LogFormat::Legendary,
            "the sign-in CLI is lgogdownloader, not gogdl"
        );
        assert_eq!(log_format(Store::Epic), LogFormat::Legendary);
    }
}
