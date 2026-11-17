//! The in-app sign-in terminal.
//!
//! The Stores screen used to answer a signed-out store with the command to run
//! in a terminal. This is the in-app replacement: a small terminal that drives
//! the store's own CLI over the shared relay ([`pitagames_stores::auth`]) and
//! shows what it says.
//!
//! ```text
//!  Sign in ──► spawn(run, store, events) ──┬─ reader thread ──► AuthEvent::Line  ──┐
//!                                          │                    AuthEvent::Prompt ┤
//!                                          └─ input thread  ◄── AuthInputs ───────┘
//!                                                                 AuthEvent::Finished
//! ```
//!
//! Two threads, because the CLI blocks on its own prompt while the person types:
//! the reader drains events, and a writer thread applies what the window sent.
//! Both hold the same [`AuthRun`] (through an `Arc`), which is what the driver's
//! lock release between polls exists for. The module itself is framework-free so
//! it can be tested without a display; [`AuthTerminal`] is the state the view
//! renders.

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

use futures_channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures_util::StreamExt as _;
use futures_util::lock::Mutex as AsyncMutex;
use pitagames_core::Store;
use pitagames_stores::auth::{self, AuthOutcome, AuthStep};
use pitagames_stores::cli::CliOptions;

/// How often the writer thread checks for typed input; also how long it can take
/// to notice that the window closed the terminal.
const INPUT_POLL: Duration = Duration::from_millis(100);

/// How many lines the terminal keeps. An auth flow prints a handful, but a CLI
/// that decides to chatter must not grow the window without bound.
const MAX_LINES: usize = 500;

/// Which run the next [`spawn`] starts.
///
/// A run's id rides on every event, so a report from a terminal the user closed
/// cannot be folded into the one they opened afterwards.
static NEXT_RUN: AtomicU64 = AtomicU64::new(1);

/// The id [`spawn`] will hand to the next run.
pub fn next_run() -> u64 {
    NEXT_RUN.fetch_add(1, Ordering::SeqCst)
}

/// One report from a running sign-in.
#[derive(Debug, Clone)]
pub enum AuthEvent {
    /// A line the CLI printed (already stripped of ANSI escapes).
    Line { run: u64, text: String },
    /// The CLI is waiting for a pasted value; show `text` and `hint`.
    Prompt {
        run: u64,
        text: String,
        hint: String,
    },
    /// The run ended; `Ok` means a session was stored.
    Finished {
        run: u64,
        result: Result<AuthOutcome, String>,
    },
}

impl AuthEvent {
    /// The run this report belongs to.
    pub const fn run(&self) -> u64 {
        match self {
            Self::Line { run, .. } | Self::Prompt { run, .. } | Self::Finished { run, .. } => *run,
        }
    }

    /// Whether this is the run's last report.
    pub const fn is_finished(&self) -> bool {
        matches!(self, Self::Finished { .. })
    }
}

/// What the terminal sends to its running sign-in.
#[derive(Debug, Clone)]
enum AuthAnswer {
    /// One typed line, without the newline.
    Line(String),
    /// The user cancelled (or closed the terminal).
    Cancel,
}

/// The window's handle on a running sign-in's input.
///
/// Cheap to clone and non-blocking: the write itself happens on the worker's
/// input thread, so a click never waits for the CLI's prompt.
#[derive(Clone)]
pub struct AuthInputs {
    sender: Sender<AuthAnswer>,
}

impl AuthInputs {
    /// Send one typed line. `false` once the run is over.
    pub fn answer(&self, text: &str) -> bool {
        self.sender.send(AuthAnswer::Line(text.to_string())).is_ok()
    }

    /// Ask the CLI to stop; the run then reports itself as cancelled.
    pub fn cancel(&self) {
        let _ = self.sender.send(AuthAnswer::Cancel);
    }
}

/// Where a sign-in stands, for the view.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum AuthStatus {
    /// The CLI is still working (or waiting for input).
    #[default]
    Running,
    /// A session was stored.
    SignedIn,
    /// The sign-in failed, with the CLI's own explanation.
    Failed(String),
    /// The user cancelled it.
    Cancelled,
}

/// The state the sign-in terminal renders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthTerminal {
    run: u64,
    store: Store,
    lines: Vec<String>,
    /// The sentence to show when the CLI wants a pasted value.
    prompt: Option<String>,
    status: AuthStatus,
    log: Option<PathBuf>,
}

impl AuthTerminal {
    /// A terminal for a run that has just been spawned.
    pub fn new(run: u64, store: Store) -> Self {
        Self {
            run,
            store,
            lines: Vec::new(),
            prompt: None,
            status: AuthStatus::Running,
            log: None,
        }
    }

    /// The store this sign-in belongs to.
    pub const fn store(&self) -> Store {
        self.store
    }

    /// The lines the CLI printed, oldest first.
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// The hint to show beside the input, while the CLI waits for a value.
    pub fn prompt(&self) -> Option<&str> {
        self.prompt.as_deref()
    }

    pub const fn status(&self) -> &AuthStatus {
        &self.status
    }

    /// The per-run raw transcript, once the CLI answered with one.
    pub fn log(&self) -> Option<&std::path::Path> {
        self.log.as_deref()
    }

    /// Whether the run is still alive (the input stays enabled).
    pub fn running(&self) -> bool {
        self.status == AuthStatus::Running
    }

    /// Fold one worker report in.
    ///
    /// Returns whether the store's row should be re-probed — which is exactly
    /// the case where a session was just stored. A report for another run is
    /// ignored, so a terminal the user closed cannot colour a later one.
    pub fn apply(&mut self, event: &AuthEvent) -> bool {
        if event.run() != self.run {
            return false;
        }

        match event {
            AuthEvent::Line { text, .. } => {
                self.push(text);
                false
            }
            AuthEvent::Prompt { text, hint, .. } => {
                self.push(text);
                self.prompt = Some(hint.clone());
                false
            }
            AuthEvent::Finished { result, .. } => {
                self.prompt = None;
                match result {
                    Ok(outcome) => {
                        self.log = outcome.log_path().map(std::path::Path::to_path_buf);
                        if outcome.succeeded() {
                            self.status = AuthStatus::SignedIn;
                            return true;
                        }
                        self.status = if outcome.cancelled() {
                            AuthStatus::Cancelled
                        } else {
                            AuthStatus::Failed(outcome.failure_message())
                        };
                    }
                    Err(message) => self.status = AuthStatus::Failed(message.clone()),
                }
                false
            }
        }
    }

    /// Append a line, dropping the oldest ones past the cap.
    fn push(&mut self, text: &str) {
        self.lines.push(text.to_string());
        if self.lines.len() > MAX_LINES {
            let drop = self.lines.len() - MAX_LINES;
            self.lines.drain(..drop);
        }
    }
}

/// Start one store's sign-in, reporting to `events`.
///
/// The thread owns the run; the window only keeps [`AuthInputs`], so a closed
/// terminal cancels the CLI instead of leaving it waiting for an answer nobody
/// can type.
pub fn spawn(
    run: u64,
    store: Store,
    options: CliOptions,
    events: UnboundedSender<AuthEvent>,
) -> AuthInputs {
    let (sender, answers) = mpsc::channel();
    let reporter = events.clone();
    let thread_name = format!("pitagames-auth-{}", store.as_str());

    let spawned = thread::Builder::new().name(thread_name).spawn(move || {
        let started = auth::AuthRun::start(store, &options);
        let run_object = match started {
            Ok(run_object) => Arc::new(run_object),
            Err(error) => {
                let _ = events.unbounded_send(AuthEvent::Finished {
                    run,
                    result: Err(error.to_string()),
                });
                return;
            }
        };

        // The writer thread exists because the reader blocks inside the driver
        // while the CLI waits for input: one thread cannot do both. It ends when
        // the run does, or when the window drops its inputs.
        let done = Arc::new(AtomicBool::new(false));
        let watched = Arc::clone(&done);
        let writer = Arc::clone(&run_object);
        let typed = thread::Builder::new()
            .name("pitagames-auth-input".to_string())
            .spawn(move || {
                while !watched.load(Ordering::SeqCst) {
                    match answers.recv_timeout(INPUT_POLL) {
                        Ok(AuthAnswer::Line(text)) => {
                            let _ = writer.answer(&text);
                        }
                        Ok(AuthAnswer::Cancel) => writer.cancel(),
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => {
                            // The window is gone: nobody can answer, so stop
                            // rather than leave the CLI waiting.
                            writer.cancel();
                            break;
                        }
                    }
                }
            });

        if let Err(error) = typed {
            eprintln!("[pitagames] auth input thread could not start: {error}");
        }

        let mut failure = None;
        loop {
            match run_object.next() {
                Ok(Some(AuthStep::Line(text) | AuthStep::Continued(text))) => {
                    let _ = events.unbounded_send(AuthEvent::Line { run, text });
                }
                Ok(Some(AuthStep::Prompt { text, hint })) => {
                    let _ = events.unbounded_send(AuthEvent::Prompt { run, text, hint });
                }
                Ok(None) => break,
                Err(error) => {
                    failure = Some(error.to_string());
                    break;
                }
            }
        }

        let result = match failure {
            Some(message) => Err(message),
            None => run_object.finish().map_err(|error| error.to_string()),
        };
        done.store(true, Ordering::SeqCst);
        let _ = events.unbounded_send(AuthEvent::Finished { run, result });
    });

    if let Err(error) = spawned {
        let _ = reporter.unbounded_send(AuthEvent::Finished {
            run,
            result: Err(format!("could not start the sign-in thread: {error}")),
        });
    }

    AuthInputs { sender }
}

/// The window's end of the auth event channel.
///
/// The same shape as [`crate::store_installs::InstallChannel`]: one drain future
/// owns the receiver, behind an async mutex because `use_hook` needs a `Clone`.
#[derive(Clone)]
pub struct AuthChannel {
    sender: UnboundedSender<AuthEvent>,
    receiver: Rc<AsyncMutex<Option<UnboundedReceiver<AuthEvent>>>>,
}

impl AuthChannel {
    pub fn new() -> Self {
        let (sender, receiver) = unbounded();
        Self {
            sender,
            receiver: Rc::new(AsyncMutex::new(Some(receiver))),
        }
    }

    /// A sender for the sign-in buttons.
    pub fn sender(&self) -> UnboundedSender<AuthEvent> {
        self.sender.clone()
    }

    /// Wait for the next report; `None` once the channel closes.
    pub async fn next(&self) -> Option<AuthEvent> {
        let mut slot = self.receiver.lock().await;
        let receiver = slot.as_mut()?;
        receiver.next().await
    }
}

impl Default for AuthChannel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_channel::mpsc::TryRecvError;
    use std::fs;
    use std::path::Path;
    use std::time::Instant;

    fn terminal() -> AuthTerminal {
        AuthTerminal::new(7, Store::Epic)
    }

    #[test]
    fn lines_and_a_prompt_land_in_order() {
        let mut auth = terminal();
        assert!(auth.running());
        assert_eq!(auth.prompt(), None);

        assert!(!auth.apply(&AuthEvent::Line {
            run: 7,
            text: "Please open https://epic.example/login".into(),
        }));
        assert!(!auth.apply(&AuthEvent::Prompt {
            run: 7,
            text: "Enter the authorization code:".into(),
            hint: "paste the code".into(),
        }));

        assert_eq!(
            auth.lines(),
            [
                "Please open https://epic.example/login".to_string(),
                "Enter the authorization code:".to_string()
            ]
        );
        assert_eq!(auth.prompt(), Some("paste the code"));
        assert!(auth.running());
    }

    #[test]
    fn a_signed_in_run_asks_for_a_re_probe() {
        let mut auth = terminal();

        assert!(
            auth.apply(&AuthEvent::Finished {
                run: 7,
                result: Ok(outcome(true, false)),
            }),
            "a stored session is what re-probes the row"
        );
        assert_eq!(auth.status(), &AuthStatus::SignedIn);
        assert!(!auth.running());
        assert_eq!(auth.prompt(), None, "the input goes away with the run");
    }

    #[test]
    fn a_failure_keeps_the_clis_message_and_does_not_re_probe() {
        let mut auth = terminal();

        assert!(!auth.apply(&AuthEvent::Finished {
            run: 7,
            result: Ok(outcome(false, false)),
        }));
        assert_eq!(
            auth.status(),
            &AuthStatus::Failed("Invalid authorization code".to_string())
        );
        assert!(!auth.running());
    }

    #[test]
    fn a_cancelled_run_says_so_instead_of_reporting_a_cli_error() {
        let mut auth = terminal();
        assert!(!auth.apply(&AuthEvent::Finished {
            run: 7,
            result: Ok(outcome(false, true)),
        }));
        assert_eq!(auth.status(), &AuthStatus::Cancelled);
    }

    #[test]
    fn a_run_that_never_started_reports_the_io_error() {
        let mut auth = terminal();
        assert!(!auth.apply(&AuthEvent::Finished {
            run: 7,
            result: Err("CLI \"legendary\" was not found in PATH".into()),
        }));
        assert_eq!(
            auth.status(),
            &AuthStatus::Failed("CLI \"legendary\" was not found in PATH".to_string())
        );
    }

    #[test]
    fn a_late_report_from_a_closed_run_is_ignored() {
        let mut auth = terminal();
        assert!(!auth.apply(&AuthEvent::Line {
            run: 99,
            text: "hello from another terminal".into(),
        }));
        assert!(!auth.apply(&AuthEvent::Finished {
            run: 99,
            result: Err("gone".into()),
        }));

        assert!(auth.lines().is_empty(), "another run's lines are not ours");
        assert!(auth.running(), "another run's verdict is not ours");
    }

    #[test]
    fn a_chatty_cli_cannot_grow_the_terminal_without_bound() {
        let mut auth = terminal();
        for index in 0..MAX_LINES + 20 {
            auth.apply(&AuthEvent::Line {
                run: 7,
                text: format!("line {index}"),
            });
        }
        assert_eq!(auth.lines().len(), MAX_LINES);
        assert_eq!(auth.lines()[MAX_LINES - 1], "line 519".to_string());
        assert_eq!(auth.lines()[0], "line 20".to_string());
    }

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

        let (events, mut receiver) = unbounded();
        let inputs = spawn(11, Store::Epic, options(dir.path()), events);
        let mut auth = AuthTerminal::new(11, Store::Epic);

        let deadline = Instant::now() + Duration::from_secs(20);
        let mut answered = false;
        loop {
            assert!(Instant::now() < deadline, "the fake CLI never finished");
            match receiver.try_recv() {
                Ok(event) => {
                    if let AuthEvent::Prompt { .. } = &event
                        && !answered
                    {
                        answered = true;
                        assert!(inputs.answer("123456"));
                    }
                    auth.apply(&event);
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
        assert_eq!(auth.status(), &AuthStatus::SignedIn);
        assert!(
            auth.lines()
                .iter()
                .any(|line| line.contains("Authentication successful")),
            "{:?}",
            auth.lines()
        );
    }

    /// A finished run's verdict, without spawning a CLI.
    ///
    /// `RunReport`'s fields are public, so the model tests can build one; the
    /// relay's own tests exercise the real constructor through a fake CLI.
    fn outcome(success: bool, cancelled: bool) -> AuthOutcome {
        let events = if success {
            vec![pitagames_stores::ProgressEvent::Done]
        } else if cancelled {
            vec![pitagames_stores::ProgressEvent::Failed {
                code: 1,
                message: "cancelled by user".to_string(),
            }]
        } else {
            vec![pitagames_stores::ProgressEvent::Failed {
                code: 0,
                message: "Invalid authorization code".to_string(),
            }]
        };

        AuthOutcome {
            report: pitagames_stores::RunReport {
                command: "auth",
                outcome: pitagames_stores::CliOutcome {
                    success,
                    code: if success { 0 } else { 1 },
                    cancelled,
                    timed_out: None,
                    log_path: None,
                    events,
                    raw_tail: String::new(),
                },
                text: String::new(),
                errors: Vec::new(),
                warnings: Vec::new(),
                format: pitagames_stores::LogFormat::Legendary,
            },
        }
    }
}
