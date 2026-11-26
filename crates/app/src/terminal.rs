//! The one in-app terminal.
//!
//! A bottom sheet that streams a child process's output, passes on the one line
//! it asks for, and cancels it when the sheet closes. The store sign-in
//! ([`crate::auth`]) is the first caller; the M3/M3.5 mod installers are the
//! next — which is why nothing here knows *what* it is driving, only that the
//! thing streams lines, may ask for one line, and can be stopped.
//!
//! ```text
//!  sheet ──► spawn(run, name, start, events) ──┬─ reader thread ──► TerminalEvent::Line ──┐
//!                                              │                    TerminalEvent::Prompt ┤
//!                                              └─ input thread  ◄── TerminalInputs ──────┘
//!                                                                     TerminalEvent::Finished
//! ```
//!
//! Two threads, because the process blocks on its own prompt while the person
//! types: the reader drains [`TerminalDriver::next_step`] and the writer applies
//! what the window sent. Both share one `Arc<dyn TerminalDriver>`. The module is
//! framework-free so it is unit-tested without a display; [`Terminal`] is the
//! state the view renders.
//!
//! Every event carries the run id it belongs to, so a report from a sheet the
//! user closed cannot colour the one they opened afterwards.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

use futures_channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures_util::StreamExt as _;
use futures_util::lock::Mutex as AsyncMutex;

/// How often the writer thread checks for typed input; also how long it can take
/// to notice that the window closed the terminal.
const INPUT_POLL: Duration = Duration::from_millis(100);

/// How many lines the terminal keeps. A run prints a handful, but a process that
/// decides to chatter must not grow the window without bound.
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

/// One step of a run, as the driver classifies it.
///
/// A line already answered by the driver (a "press ENTER" courtesy) is just a
/// [`Line`](Self::Line) here: the shared terminal does not care that it *was* a
/// prompt, only that nothing has to be typed now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalStep {
    /// A line the process printed; show it as it is.
    Line(String),
    /// The process is waiting for one typed line.
    Prompt { text: String, hint: String },
}

/// A finished run, as the terminal needs to judge it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalOutcome {
    /// The run did what its caller wanted (a session stored, a mod installed).
    pub succeeded: bool,
    /// The person stopped it.
    pub cancelled: bool,
    /// One line explaining a failure, shown when neither of the above holds.
    pub message: String,
    /// The per-run raw transcript, when the driver wrote one.
    pub log: Option<PathBuf>,
}

/// One report from a running terminal.
#[derive(Debug, Clone)]
pub enum TerminalEvent {
    /// A line the process printed.
    Line { run: u64, text: String },
    /// The process is waiting for a value; show `text` and `hint`.
    Prompt {
        run: u64,
        text: String,
        hint: String,
    },
    /// The run ended; `Ok` carries the driver's verdict.
    Finished {
        run: u64,
        result: Result<TerminalOutcome, String>,
    },
}

impl TerminalEvent {
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

/// What the terminal sends to its running process.
#[derive(Debug, Clone)]
enum TerminalAnswer {
    /// One typed line, without the newline.
    Line(String),
    /// The person cancelled (or closed the terminal).
    Cancel,
}

/// The window's handle on a running process's input.
///
/// Cheap to clone and non-blocking: the write itself happens on the worker's
/// input thread, so a click never waits for the process's prompt.
#[derive(Clone)]
pub struct TerminalInputs {
    sender: Sender<TerminalAnswer>,
}

impl TerminalInputs {
    /// Send one typed line. `false` once the run is over.
    pub fn answer(&self, text: &str) -> bool {
        self.sender
            .send(TerminalAnswer::Line(text.to_string()))
            .is_ok()
    }

    /// Ask the process to stop; the run then reports itself as cancelled.
    pub fn cancel(&self) {
        let _ = self.sender.send(TerminalAnswer::Cancel);
    }
}

/// Where a run stands, for the view.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum TerminalStatus {
    /// The process is still working (or waiting for input).
    #[default]
    Running,
    /// The run did what its caller wanted.
    Succeeded,
    /// The run failed, with its own explanation.
    Failed(String),
    /// The person cancelled it.
    Cancelled,
}

/// The state the terminal sheet renders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Terminal {
    run: u64,
    title: String,
    lines: Vec<String>,
    /// The sentence to show when the process wants a value.
    prompt: Option<String>,
    status: TerminalStatus,
    log: Option<PathBuf>,
}

impl Terminal {
    /// A terminal for a run that has just been spawned.
    ///
    /// `title` is the caller's label — a store name, a mod's name — shown in the
    /// sheet's header; the model itself does not interpret it.
    pub fn new(run: u64, title: impl Into<String>) -> Self {
        Self {
            run,
            title: title.into(),
            lines: Vec::new(),
            prompt: None,
            status: TerminalStatus::Running,
            log: None,
        }
    }

    /// The caller's label for this run.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// The lines the process printed, oldest first.
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// The hint to show beside the input, while the process waits for a value.
    pub fn prompt(&self) -> Option<&str> {
        self.prompt.as_deref()
    }

    pub const fn status(&self) -> &TerminalStatus {
        &self.status
    }

    /// The per-run raw transcript, once the process answered with one.
    pub fn log(&self) -> Option<&Path> {
        self.log.as_deref()
    }

    /// Whether the run is still alive (the input stays enabled).
    pub fn running(&self) -> bool {
        self.status == TerminalStatus::Running
    }

    /// Fold one worker report in.
    ///
    /// Returns whether this report completed the run **successfully** — the
    /// caller's cue to act on a win (re-probe a store's row, refresh a game's mod
    /// state). A report for another run is ignored, so a terminal the user closed
    /// cannot colour a later one.
    pub fn apply(&mut self, event: &TerminalEvent) -> bool {
        if event.run() != self.run {
            return false;
        }

        match event {
            TerminalEvent::Line { text, .. } => {
                self.push(text);
                false
            }
            TerminalEvent::Prompt { text, hint, .. } => {
                self.push(text);
                self.prompt = Some(hint.clone());
                false
            }
            TerminalEvent::Finished { result, .. } => {
                self.prompt = None;
                match result {
                    Ok(outcome) => {
                        self.log = outcome.log.clone();
                        if outcome.succeeded {
                            self.status = TerminalStatus::Succeeded;
                            return true;
                        }
                        self.status = if outcome.cancelled {
                            TerminalStatus::Cancelled
                        } else {
                            TerminalStatus::Failed(outcome.message.clone())
                        };
                    }
                    Err(message) => self.status = TerminalStatus::Failed(message.clone()),
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

/// What a terminal drives.
///
/// A store's CLI, an installer, a fixture. The terminal only needs the stream,
/// the one line it asks for, and how to stop and judge the run — everything
/// specific to *what* runs lives on the caller's side of this trait.
pub trait TerminalDriver: Send + Sync + 'static {
    /// The next thing the process wants, or `None` when its stream ended.
    ///
    /// A caller that gets `None` must still be judged by [`finish`](Self::finish):
    /// the verdict is the outcome, not the end of the stream.
    fn next_step(&self) -> Result<Option<TerminalStep>, String>;

    /// Answer the prompt [`TerminalStep::Prompt`] asked for.
    fn answer(&self, line: &str) -> Result<(), String>;

    /// Ask the process to stop. Idempotent.
    fn cancel(&self);

    /// Drain the run and return its verdict.
    fn finish(&self) -> Result<TerminalOutcome, String>;
}

/// Start one run, reporting to `events`.
///
/// `start` runs **inside the worker thread**, so a slow or failing spawn is
/// reported as a finished run rather than blocking the caller. It returns the
/// [`TerminalDriver`] the terminal will read and write; an `Err` is shown as a
/// failed run. The thread owns the driver; the window only keeps
/// [`TerminalInputs`], so a closed terminal cancels the run instead of leaving it
/// waiting for an answer nobody can type.
pub fn spawn(
    run: u64,
    thread_name: String,
    start: impl FnOnce() -> Result<Box<dyn TerminalDriver>, String> + Send + 'static,
    events: UnboundedSender<TerminalEvent>,
) -> TerminalInputs {
    let (sender, answers) = mpsc::channel();
    let reporter = events.clone();

    let spawned = thread::Builder::new().name(thread_name).spawn(move || {
        let driver: Arc<dyn TerminalDriver> = match start() {
            Ok(driver) => Arc::from(driver),
            Err(message) => {
                let _ = events.unbounded_send(TerminalEvent::Finished {
                    run,
                    result: Err(message),
                });
                return;
            }
        };

        // The writer thread exists because the reader blocks inside the driver
        // while the process waits for input: one thread cannot do both. It ends
        // when the run does, or when the window drops its inputs.
        let done = Arc::new(AtomicBool::new(false));
        let watched = Arc::clone(&done);
        let writer = Arc::clone(&driver);
        let typed = thread::Builder::new()
            .name("pitagames-terminal-input".to_string())
            .spawn(move || {
                while !watched.load(Ordering::SeqCst) {
                    match answers.recv_timeout(INPUT_POLL) {
                        Ok(TerminalAnswer::Line(text)) => {
                            let _ = writer.answer(&text);
                        }
                        Ok(TerminalAnswer::Cancel) => writer.cancel(),
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => {
                            // The window is gone: nobody can answer, so stop
                            // rather than leave the process waiting.
                            writer.cancel();
                            break;
                        }
                    }
                }
            });

        if let Err(error) = typed {
            eprintln!("[pitagames] terminal input thread could not start: {error}");
        }

        let mut failure = None;
        loop {
            match driver.next_step() {
                Ok(Some(TerminalStep::Line(text))) => {
                    let _ = events.unbounded_send(TerminalEvent::Line { run, text });
                }
                Ok(Some(TerminalStep::Prompt { text, hint })) => {
                    let _ = events.unbounded_send(TerminalEvent::Prompt { run, text, hint });
                }
                Ok(None) => break,
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            }
        }

        let result = match failure {
            Some(message) => Err(message),
            None => driver.finish(),
        };
        done.store(true, Ordering::SeqCst);
        let _ = events.unbounded_send(TerminalEvent::Finished { run, result });
    });

    if let Err(error) = spawned {
        let _ = reporter.unbounded_send(TerminalEvent::Finished {
            run,
            result: Err(format!("could not start the terminal thread: {error}")),
        });
    }

    TerminalInputs { sender }
}

/// The window's end of the terminal event channel.
///
/// The same shape as [`crate::store_installs::InstallChannel`]: one drain future
/// owns the receiver, behind an async mutex because `use_hook` needs a `Clone`.
#[derive(Clone)]
pub struct TerminalChannel {
    sender: UnboundedSender<TerminalEvent>,
    receiver: Rc<AsyncMutex<Option<UnboundedReceiver<TerminalEvent>>>>,
}

impl TerminalChannel {
    pub fn new() -> Self {
        let (sender, receiver) = unbounded();
        Self {
            sender,
            receiver: Rc::new(AsyncMutex::new(Some(receiver))),
        }
    }

    /// A sender for the terminal's starter.
    pub fn sender(&self) -> UnboundedSender<TerminalEvent> {
        self.sender.clone()
    }

    /// Wait for the next report; `None` once the channel closes.
    pub async fn next(&self) -> Option<TerminalEvent> {
        let mut slot = self.receiver.lock().await;
        let receiver = slot.as_mut()?;
        receiver.next().await
    }
}

impl Default for TerminalChannel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_channel::mpsc::TryRecvError;
    use std::collections::VecDeque;
    use std::sync::Condvar;
    use std::sync::Mutex;
    use std::time::Instant;

    const RUN: u64 = 7;

    fn terminal() -> Terminal {
        Terminal::new(RUN, "Epic")
    }

    /// A finished run's verdict, built directly (the adapter tests exercise a
    /// real driver through a fake process).
    fn outcome(succeeded: bool, cancelled: bool) -> TerminalOutcome {
        TerminalOutcome {
            succeeded,
            cancelled,
            message: if succeeded {
                String::new()
            } else if cancelled {
                "cancelled by user".to_string()
            } else {
                "Invalid authorization code".to_string()
            },
            log: None,
        }
    }

    #[test]
    fn lines_and_a_prompt_land_in_order() {
        let mut term = terminal();
        assert!(term.running());
        assert_eq!(term.prompt(), None);
        assert_eq!(term.title(), "Epic");

        assert!(!term.apply(&TerminalEvent::Line {
            run: RUN,
            text: "Please open https://epic.example/login".into(),
        }));
        assert!(!term.apply(&TerminalEvent::Prompt {
            run: RUN,
            text: "Enter the authorization code:".into(),
            hint: "paste the code".into(),
        }));

        assert_eq!(
            term.lines(),
            [
                "Please open https://epic.example/login".to_string(),
                "Enter the authorization code:".to_string()
            ]
        );
        assert_eq!(term.prompt(), Some("paste the code"));
        assert!(term.running());
    }

    #[test]
    fn a_successful_run_reports_success() {
        let mut term = terminal();

        assert!(
            term.apply(&TerminalEvent::Finished {
                run: RUN,
                result: Ok(outcome(true, false)),
            }),
            "a successful run is what the caller acts on"
        );
        assert_eq!(term.status(), &TerminalStatus::Succeeded);
        assert!(!term.running());
        assert_eq!(term.prompt(), None, "the input goes away with the run");
    }

    #[test]
    fn a_failure_keeps_the_drivers_message_and_does_not_report_success() {
        let mut term = terminal();

        assert!(!term.apply(&TerminalEvent::Finished {
            run: RUN,
            result: Ok(outcome(false, false)),
        }));
        assert_eq!(
            term.status(),
            &TerminalStatus::Failed("Invalid authorization code".to_string())
        );
        assert!(!term.running());
    }

    #[test]
    fn a_cancelled_run_says_so_instead_of_reporting_a_failure() {
        let mut term = terminal();
        assert!(!term.apply(&TerminalEvent::Finished {
            run: RUN,
            result: Ok(outcome(false, true)),
        }));
        assert_eq!(term.status(), &TerminalStatus::Cancelled);
    }

    #[test]
    fn a_run_that_never_started_reports_the_io_error() {
        let mut term = terminal();
        assert!(!term.apply(&TerminalEvent::Finished {
            run: RUN,
            result: Err("CLI \"legendary\" was not found in PATH".into()),
        }));
        assert_eq!(
            term.status(),
            &TerminalStatus::Failed("CLI \"legendary\" was not found in PATH".to_string())
        );
    }

    #[test]
    fn a_late_report_from_a_closed_run_is_ignored() {
        let mut term = terminal();
        assert!(!term.apply(&TerminalEvent::Line {
            run: 99,
            text: "hello from another terminal".into(),
        }));
        assert!(!term.apply(&TerminalEvent::Finished {
            run: 99,
            result: Err("gone".into()),
        }));

        assert!(term.lines().is_empty(), "another run's lines are not ours");
        assert!(term.running(), "another run's verdict is not ours");
    }

    #[test]
    fn a_chatty_process_cannot_grow_the_terminal_without_bound() {
        let mut term = terminal();
        for index in 0..MAX_LINES + 20 {
            term.apply(&TerminalEvent::Line {
                run: RUN,
                text: format!("line {index}"),
            });
        }
        assert_eq!(term.lines().len(), MAX_LINES);
        assert_eq!(term.lines()[MAX_LINES - 1], "line 519".to_string());
        assert_eq!(term.lines()[0], "line 20".to_string());
    }

    /// A stand-in process for [`spawn`]: a scripted list of steps, a place to
    /// record typed answers, and a verdict.
    ///
    /// This is the **second caller** that proves the terminal is not
    /// store-specific — an installer (or any other process) drives the same
    /// model through [`TerminalDriver`] with no store in sight.
    #[derive(Clone)]
    struct ScriptedDriver {
        inner: Arc<ScriptInner>,
    }

    struct ScriptInner {
        steps: Mutex<VecDeque<TerminalStep>>,
        answers: Mutex<Vec<String>>,
        state: Mutex<ScriptState>,
        wake: Condvar,
        verdict: TerminalOutcome,
    }

    #[derive(Default)]
    struct ScriptState {
        /// Set when an answer has arrived, so the reader may end the stream.
        answered: bool,
        cancelled: bool,
    }

    impl ScriptedDriver {
        fn new(steps: Vec<TerminalStep>, verdict: TerminalOutcome) -> Self {
            Self {
                inner: Arc::new(ScriptInner {
                    steps: Mutex::new(steps.into()),
                    answers: Mutex::new(Vec::new()),
                    state: Mutex::new(ScriptState::default()),
                    wake: Condvar::new(),
                    verdict,
                }),
            }
        }

        fn answers(&self) -> Vec<String> {
            self.inner.answers.lock().unwrap().clone()
        }
    }

    impl TerminalDriver for ScriptedDriver {
        fn next_step(&self) -> Result<Option<TerminalStep>, String> {
            let inner = &self.inner;
            let mut state = inner.state.lock().unwrap();
            loop {
                if state.cancelled {
                    return Ok(None);
                }
                if let Some(step) = inner.steps.lock().unwrap().pop_front() {
                    return Ok(Some(step));
                }
                // Nothing scripted and no cancellation: the process is waiting
                // for the answer to the prompt it just printed.
                if state.answered {
                    return Ok(None);
                }
                state = inner.wake.wait(state).unwrap();
            }
        }

        fn answer(&self, line: &str) -> Result<(), String> {
            let inner = &self.inner;
            inner.answers.lock().unwrap().push(line.to_string());
            let mut state = inner.state.lock().unwrap();
            state.answered = true;
            inner.wake.notify_all();
            Ok(())
        }

        fn cancel(&self) {
            let mut state = self.inner.state.lock().unwrap();
            state.cancelled = true;
            self.inner.wake.notify_all();
        }

        fn finish(&self) -> Result<TerminalOutcome, String> {
            let cancelled =
                self.inner.state.lock().unwrap().cancelled || self.inner.verdict.cancelled;
            let mut outcome = self.inner.verdict.clone();
            if cancelled {
                // A stopped run is cancelled, never a success — the same shape
                // `RunReport::succeeded` gives a signalled CLI.
                outcome.succeeded = false;
                outcome.cancelled = true;
            }
            Ok(outcome)
        }
    }

    /// Block until a done event arrives, folding every report into `term` and
    /// letting `react` answer or cancel.
    fn drive(
        mut term: Terminal,
        receiver: &mut UnboundedReceiver<TerminalEvent>,
        mut react: impl FnMut(&TerminalEvent),
    ) -> Terminal {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(
                Instant::now() < deadline,
                "the scripted driver never finished"
            );
            match receiver.try_recv() {
                Ok(event) => {
                    let finished = event.is_finished();
                    react(&event);
                    term.apply(&event);
                    if finished {
                        return term;
                    }
                }
                Err(TryRecvError::Empty) => thread::sleep(Duration::from_millis(5)),
                Err(TryRecvError::Closed) => panic!("the channel closed before the verdict"),
            }
        }
    }

    #[test]
    fn a_scripted_driver_streams_an_answer_and_finishes() {
        let driver = ScriptedDriver::new(
            vec![
                TerminalStep::Line("opening the page".to_string()),
                TerminalStep::Prompt {
                    text: "Enter the code:".to_string(),
                    hint: "paste the code".to_string(),
                },
            ],
            outcome(true, false),
        );

        let (events, mut receiver) = unbounded();
        let boxed = driver.clone();
        let inputs = spawn(
            RUN,
            "pitagames-test".to_string(),
            move || {
                let driver: Box<dyn TerminalDriver> = Box::new(boxed);
                Ok(driver)
            },
            events,
        );

        let term = drive(Terminal::new(RUN, "Test"), &mut receiver, |event| {
            if let TerminalEvent::Prompt { .. } = event {
                assert!(inputs.answer("123456"));
            }
        });

        assert_eq!(driver.answers(), vec!["123456".to_string()]);
        assert_eq!(term.status(), &TerminalStatus::Succeeded);
        assert_eq!(
            term.lines(),
            [
                "opening the page".to_string(),
                "Enter the code:".to_string()
            ]
        );
    }

    #[test]
    fn cancelling_a_scripted_driver_reports_cancelled() {
        let driver = ScriptedDriver::new(
            vec![TerminalStep::Line("waiting".to_string())],
            outcome(true, false),
        );

        let (events, mut receiver) = unbounded();
        let boxed = driver.clone();
        let inputs = spawn(
            RUN,
            "pitagames-test".to_string(),
            move || {
                let driver: Box<dyn TerminalDriver> = Box::new(boxed);
                Ok(driver)
            },
            events,
        );

        let mut inputs = Some(inputs);
        let term = drive(Terminal::new(RUN, "Test"), &mut receiver, |event| {
            if let TerminalEvent::Line { .. } = event
                && let Some(inputs) = inputs.take()
            {
                inputs.cancel();
            }
        });

        assert_eq!(
            term.status(),
            &TerminalStatus::Cancelled,
            "an explicit cancel is not a success"
        );
    }

    #[test]
    fn dropping_the_inputs_cancels_a_scripted_driver() {
        let driver = ScriptedDriver::new(
            vec![TerminalStep::Line("waiting".to_string())],
            outcome(true, false),
        );

        let (events, mut receiver) = unbounded();
        let boxed = driver.clone();
        let inputs = spawn(
            RUN,
            "pitagames-test".to_string(),
            move || {
                let driver: Box<dyn TerminalDriver> = Box::new(boxed);
                Ok(driver)
            },
            events,
        );

        // The window closing drops the inputs; the writer notices and cancels.
        let mut inputs = Some(inputs);
        let term = drive(Terminal::new(RUN, "Test"), &mut receiver, |event| {
            if let TerminalEvent::Line { .. } = event {
                drop(inputs.take());
            }
        });

        assert_eq!(term.status(), &TerminalStatus::Cancelled);
    }
}
