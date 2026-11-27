//! The CLI driver shared by the non-Steam store backends.
//!
//! Epic (`legendary`), Amazon (`nile`) and GOG (`lgogdownloader`) are wrapped
//! as external programs. This module is the one place that spawns them: it
//! allocates a PTY, streams output line by line into [`ProgressEvent`]s,
//! forwards typed stdin (auth prompts, TOTP codes), and enforces cancellation
//! and timeouts without ever building a shell string.
//!
//! ```text
//!  CliSpec (program + argv templates)          CliHandle (this thread)
//!         │                                    ├─ writer ──► pty master ──► child stdin
//!         ▼                                    ├─ child  (wait / kill)
//!  CliCommand ──argv()──► program + args ──spawn─┤
//!                                               └─ rx ◄── reader thread ◄── pty master
//!                                                            └── raw log file
//! ```
//!
//! The reader thread owns a cloned master fd and tees raw bytes to an optional
//! per-run log; the handle owns the writer, so stdin writes never race the
//! reader. `portable-pty` starts the child in its own session with the PTY as
//! the controlling terminal, which is what makes a `SIGHUP` to the child also
//! reach a CLI's worker processes.
//!
//! Timeouts are per operation: an install gets an idle timeout (a silent CLI is
//! stuck) and a hard cap, while an interactive auth flow waits for a person and
//! only gets the hard cap. When a timeout or [`CliHandle::cancel`] fires, the
//! child is signalled with `SIGHUP`; if it survives the grace period (or ignores
//! the signal) the whole process group is `SIGKILL`ed, so a cancelled install
//! cannot leave workers running in the background.
//!
//! Nothing here is async and nothing here knows about SQLite or the GUI: the
//! store backends (M2) drive a [`CliHandle`] from their own thread and persist
//! whatever they need.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use portable_pty::{Child, ChildKiller, CommandBuilder, ExitStatus, PtySize, native_pty_system};
use regex::Regex;
use serde::Serialize;

/// Default quiet period before a run is considered stuck (installs, verify).
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Default hard cap for a non-interactive run.
pub const DEFAULT_HARD_TIMEOUT: Duration = Duration::from_secs(6 * 60 * 60);
/// Hard cap for an interactive auth flow, which has no idle timeout.
pub const DEFAULT_AUTH_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// The terminal has been silent this long and a partial line is buffered:
/// flush it, so a `Prompt: ` that never ends with a newline still reaches the
/// caller (the user has to see it to answer).
const PARTIAL_FLUSH_QUIET: Duration = Duration::from_millis(150);
/// How long a signalled child gets before its process group is killed.
const ESCALATE_GRACE: Duration = Duration::from_secs(2);
/// Poll slice for the output channel; also the resolution of the timeouts.
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Bytes read from the PTY per syscall.
const READ_CHUNK: usize = 8 * 1024;
/// Bounded reader queue: 4096 × 8 KiB ≈ 32 MiB, after which backpressure
/// stops the CLI instead of growing the launcher's memory.
const READER_QUEUE: usize = 4096;
/// Raw output kept in [`CliOutcome::raw_tail`] for failure records.
const RAW_TAIL_BYTES: usize = 16 * 1024;
/// A long line is cut to this many characters before it becomes a failure
/// message.
const FAILURE_TEXT_CHARS: usize = 200;
/// The PTY size CLIs see; wide enough that wrapped help/progress text stays
/// readable in logs.
const PTY_SIZE: PtySize = PtySize {
    rows: 50,
    cols: 120,
    pixel_width: 0,
    pixel_height: 0,
};

/// One operation a store CLI understands.
///
/// The variants carry exactly the values the argv templates may interpolate:
/// `{id}` everywhere it applies, `{dir}`/`{parent}`/`{name}` for an install (the
/// last two are the two halves of `{dir}`), and `{dir}` again for a verify or a
/// launch that needs to be pointed at the installed game, nothing for the
/// commands that only read or change the CLI's own state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliCommand {
    ListInstalled,
    ListOwned,
    Auth,
    /// Remove the stored session (`legendary auth --delete`).
    Logout,
    /// Refresh the store's cached library (`nile library sync`).
    Sync,
    Install {
        id: String,
        install_dir: PathBuf,
    },
    /// Check an installed game's files without downloading.
    ///
    /// `install_dir` is optional because not every CLI needs it: legendary and
    /// nile address a game by id alone, while `gogdl` wants `--path` for both a
    /// repair and a launch. When it is `Some`, the verify template may use
    /// `{dir}`/`{parent}`/`{name}`.
    Verify {
        id: String,
        install_dir: Option<PathBuf>,
    },
    Launch {
        id: String,
        install_dir: Option<PathBuf>,
    },
    /// The CLI's own account/install summary.
    Status,
    /// The CLI's own version, for the "outdated" warning.
    Version,
}

impl CliCommand {
    /// Stable label used in log file names and error messages.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::ListInstalled => "list-installed",
            Self::ListOwned => "list-owned",
            Self::Auth => "auth",
            Self::Logout => "logout",
            Self::Sync => "sync",
            Self::Install { .. } => "install",
            Self::Verify { .. } => "verify",
            Self::Launch { .. } => "launch",
            Self::Status => "status",
            Self::Version => "version",
        }
    }
}

/// How a CLI's output lines are turned into [`ProgressEvent`]s.
#[derive(Debug, Clone)]
pub enum ProgressParser {
    /// Every line is a [`ProgressEvent::Line`].
    Plain,
    /// A regex with an optional capture group; the captured (or whole) match
    /// is parsed as `0..=100`. A line that does not match (or is out of range)
    /// stays a [`ProgressEvent::Line`].
    Percent(Regex),
    /// JSON objects per line: `{"stage": "...", "percent": 42}` becomes
    /// [`ProgressEvent::Stage`] / [`ProgressEvent::Percent`]; anything else
    /// stays a line. Used by CLIs with machine-readable progress.
    JsonLines,
}

/// One parsed step of a CLI run.
///
/// The stream always ends with exactly one terminal event: [`Self::Done`] when
/// the CLI exited `0` without being cancelled or timing out, otherwise
/// [`Self::Failed`] with a message taken from the CLI's own last line (or from
/// the cancel/timeout/signal that ended the run).
#[derive(Debug, Clone, PartialEq)]
pub enum ProgressEvent {
    /// A plain output line (ANSI escapes stripped).
    Line(String),
    /// A progress line: percentage and the text it came from.
    Percent(f32, String),
    /// A named phase, from a JSON line or a CLI that reports stages.
    Stage(String),
    /// The run finished successfully.
    Done,
    /// The run did not finish successfully.
    Failed { code: i32, message: String },
}

/// Which timeout ended a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutKind {
    /// No output for the configured idle period.
    Idle,
    /// The configured hard cap elapsed.
    Hard,
}

impl std::fmt::Display for TimeoutKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Idle => "idle",
            Self::Hard => "hard",
        })
    }
}

/// Everything the driver needs to know about one CLI.
///
/// The argv templates are vectors, never shell strings; `{id}` and `{dir}` are
/// substituted verbatim (see [`CliSpec::argv`]). A spec is cheap to build and
/// reusable across runs — the per-store presets land with the Epic/Amazon/GOG
/// slices.
#[derive(Debug, Clone)]
pub struct CliSpec {
    /// Program name (resolved through `PATH`) or absolute path.
    pub program: String,
    /// Kept for the spec's readability: the driver always allocates a PTY,
    /// which is harmless for non-interactive CLIs and required for prompts.
    pub needs_tty: bool,
    /// Output line parser.
    pub progress: ProgressParser,
    pub list_installed: Vec<String>,
    pub list_owned: Vec<String>,
    pub auth: Vec<String>,
    /// `legendary auth --delete`: forget the stored session.
    pub logout: Vec<String>,
    /// Refresh the store's own library cache; a store without one leaves this
    /// empty and the command is rejected as unsupported.
    pub sync: Vec<String>,
    /// Templated with `{id}`, `{dir}`, `{parent}` and `{name}`.
    pub install: Vec<String>,
    /// Templated with `{id}`.
    pub verify: Vec<String>,
    /// Templated with `{id}`.
    pub launch: Vec<String>,
    pub status: Vec<String>,
    pub version: Vec<String>,
}

impl CliSpec {
    /// Builds the argv for `command`, substituting the template placeholders.
    ///
    /// Substitution is one pass and verbatim: an argument vector needs no
    /// quoting, and a substituted value is never rescanned for placeholders.
    /// A placeholder that is not available for the command (or malformed) is a
    /// [`CliError::Template`], never silently dropped. A command whose template
    /// is empty is [`CliError::Unsupported`], so a preset cannot run a CLI with
    /// no arguments by accident.
    pub fn argv(&self, command: &CliCommand) -> Result<Vec<String>, CliError> {
        let (template, values): (&[String], Vec<(&str, Option<String>)>) = match command {
            CliCommand::ListInstalled => (&self.list_installed, Vec::new()),
            CliCommand::ListOwned => (&self.list_owned, Vec::new()),
            CliCommand::Auth => (&self.auth, Vec::new()),
            CliCommand::Logout => (&self.logout, Vec::new()),
            CliCommand::Sync => (&self.sync, Vec::new()),
            CliCommand::Install { id, install_dir } => (
                &self.install,
                vec![
                    ("id", Some(id.clone())),
                    ("dir", Some(install_dir.to_string_lossy().into_owned())),
                    (
                        "parent",
                        install_dir
                            .parent()
                            .map(|parent| parent.to_string_lossy().into_owned()),
                    ),
                    (
                        "name",
                        install_dir
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned()),
                    ),
                ],
            ),
            // A verify or a launch carries the game's directory only when the
            // caller knows it (gogdl needs `--path` for both); a backend whose
            // template does not mention `{dir}` is unaffected.
            CliCommand::Verify { id, install_dir } => (
                &self.verify,
                vec![("id", Some(id.clone()))]
                    .into_iter()
                    .chain(dir_values(install_dir))
                    .collect(),
            ),
            CliCommand::Launch { id, install_dir } => (
                &self.launch,
                vec![("id", Some(id.clone()))]
                    .into_iter()
                    .chain(dir_values(install_dir))
                    .collect(),
            ),
            CliCommand::Status => (&self.status, Vec::new()),
            CliCommand::Version => (&self.version, Vec::new()),
        };
        if template.is_empty() {
            return Err(CliError::Unsupported {
                command: command.as_str(),
            });
        }
        template
            .iter()
            .map(|token| substitute(token, &values))
            .collect()
    }
}

/// The `{dir}`/`{parent}`/`{name}` values for a command that carries an
/// optional install directory.
///
/// A `None` entry is deliberately kept as `("dir", None)` (not omitted) so a
/// template that mentions `{dir}` fails with `CliError::Template` instead of
/// silently dropping the argument.
fn dir_values(install_dir: &Option<PathBuf>) -> Vec<(&'static str, Option<String>)> {
    let Some(dir) = install_dir else {
        return vec![("dir", None), ("parent", None), ("name", None)];
    };
    vec![
        ("dir", Some(dir.to_string_lossy().into_owned())),
        (
            "parent",
            dir.parent()
                .map(|parent| parent.to_string_lossy().into_owned()),
        ),
        (
            "name",
            dir.file_name()
                .map(|name| name.to_string_lossy().into_owned()),
        ),
    ]
}

fn substitute(token: &str, values: &[(&str, Option<String>)]) -> Result<String, CliError> {
    let mut out = String::with_capacity(token.len());
    let mut rest = token;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            return Err(CliError::Template {
                token: token.to_string(),
                placeholder: after.to_string(),
            });
        };
        let name = &after[..close];
        // A value the command carries but this install directory cannot split
        // (`{parent}` of `/`) is just as unavailable as an unknown name: an
        // argument must never be silently dropped.
        let Some((_, Some(value))) = values.iter().find(|(key, _)| *key == name) else {
            return Err(CliError::Template {
                token: token.to_string(),
                placeholder: name.to_string(),
            });
        };
        out.push_str(value);
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Whether a store's CLI is there, which is the first thing a store row shows.
///
/// Shared by every backend on the driver: the question is "did `resolve_program`
/// find an executable", and the answer names the program either way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum StoreStatus {
    /// The program resolved to an executable file.
    Ready { program: PathBuf },
    /// Nothing by that name is on `PATH`; installs are unavailable.
    CliMissing { program: String },
}

impl StoreStatus {
    /// Probes a program name without running it.
    pub fn probe(program: &str, path_override: Option<&str>) -> Self {
        match resolve_program(program, path_override) {
            Ok(program) => Self::Ready { program },
            Err(_) => Self::CliMissing {
                program: program.to_string(),
            },
        }
    }

    pub const fn is_ready(&self) -> bool {
        matches!(self, Self::Ready { .. })
    }

    /// The resolved program, when there is one.
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Ready { program } => Some(program),
            Self::CliMissing { .. } => None,
        }
    }

    /// The program name as it was asked for, for an error message.
    pub fn program(&self) -> &str {
        match self {
            Self::Ready { program } => program
                .file_name()
                .and_then(|name| name.to_str())
                .or_else(|| program.to_str())
                .unwrap_or("cli"),
            Self::CliMissing { program } => program,
        }
    }
}

/// Per-run driver settings.
#[derive(Debug, Clone)]
pub struct CliOptions {
    /// Directory for the per-run raw log; `None` disables logging.
    pub log_dir: Option<PathBuf>,
    /// Environment overrides merged over the inherited environment (`PATH`
    /// replacement included — this is how tests inject fake CLIs).
    pub env: BTreeMap<String, String>,
    /// Working directory; `None` keeps the process's home directory.
    pub cwd: Option<PathBuf>,
    /// Kill a run that produced no output for this long; `None` waits forever.
    pub idle_timeout: Option<Duration>,
    /// Hard cap for the whole run; `None` waits forever.
    pub hard_timeout: Option<Duration>,
}

impl Default for CliOptions {
    fn default() -> Self {
        Self {
            log_dir: None,
            env: BTreeMap::new(),
            cwd: None,
            idle_timeout: Some(DEFAULT_IDLE_TIMEOUT),
            hard_timeout: Some(DEFAULT_HARD_TIMEOUT),
        }
    }
}

impl CliOptions {
    /// Settings for an auth flow: a person may take minutes to fetch a TOTP
    /// code, so only the hard cap applies.
    pub fn interactive() -> Self {
        Self {
            idle_timeout: None,
            hard_timeout: Some(DEFAULT_AUTH_TIMEOUT),
            ..Self::default()
        }
    }
}

/// Errors that prevent a run from starting or from being read.
///
/// A run that starts and then fails (non-zero exit, cancellation, timeout)
/// is not an error: it ends with a [`ProgressEvent::Failed`] and flags on the
/// [`CliOutcome`], so the partial output survives.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("CLI {program:?} was not found in PATH")]
    NotFound { program: String },

    #[error("this preset has no template for `{command}`")]
    Unsupported { command: &'static str },

    #[error("unsupported placeholder {placeholder:?} in {token:?}")]
    Template { token: String, placeholder: String },

    #[error("failed to create the CLI log {path}: {source}")]
    Log {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to start the CLI: {0}")]
    Spawn(String),

    #[error("CLI I/O failed: {source}")]
    Io {
        #[source]
        source: std::io::Error,
    },
}

/// What a finished run produced.
#[derive(Debug, Clone)]
pub struct CliOutcome {
    /// The CLI exited `0` and the run was neither cancelled nor timed out.
    pub success: bool,
    /// The process exit code (`1` when it died from a signal).
    pub code: u32,
    /// The user called [`CliHandle::cancel`].
    pub cancelled: bool,
    /// Which timeout ended the run, if any.
    pub timed_out: Option<TimeoutKind>,
    /// The per-run raw log, when logging was enabled.
    pub log_path: Option<PathBuf>,
    /// Every event, ending with exactly one terminal event.
    pub events: Vec<ProgressEvent>,
    /// The last 16 KiB of raw output (ANSI included), for failure records.
    /// Note that a PTY echoes typed input: an auth run's tail can contain the
    /// code the user typed, so it is for local diagnostics only.
    pub raw_tail: String,
}

/// A running CLI.
///
/// `CliHandle` is `Send` and its methods take `&self`, so the reader side can
/// live on a worker thread while another thread cancels it or answers a prompt.
/// The handle must be dropped (or [`wait`](Self::wait)ed) for the child to be
/// reaped.
pub struct CliHandle {
    inner: Mutex<CliInner>,
    /// The reader thread's output, behind its own lock.
    ///
    /// The poll loop blocks on this channel for a poll interval at a time, and
    /// holding the state lock across that wait would starve [`cancel`](Self::cancel)
    /// and [`write_stdin`](Self::write_stdin): a thread that waits for the state
    /// only ever arrives between two polls, which is a window far too small to
    /// be fair. Two locks make the blocking wait and the state independent.
    inbox: Mutex<Receiver<Option<Vec<u8>>>>,
}

struct CliInner {
    child: Box<dyn Child + Send + Sync>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    pid: Option<u32>,
    writer: Box<dyn Write + Send>,
    assembler: LineAssembler,
    log_path: Option<PathBuf>,
    idle_timeout: Option<Duration>,
    hard_timeout: Option<Duration>,
    started: Instant,
    last_activity: Instant,
    cursor: usize,
    status: Option<ExitStatus>,
    kill_requested: bool,
    killed_at: Option<Instant>,
    escalated: bool,
    cancelled: bool,
    timed_out: Option<TimeoutKind>,
    eof: bool,
}

/// Turns raw PTY bytes into events.
///
/// Kept free of the child/channel plumbing so the line splitting, ANSI
/// stripping and progress parsing can be unit-tested without spawning
/// anything.
struct LineAssembler {
    parser: ProgressParser,
    line_buf: Vec<u8>,
    raw_tail: Vec<u8>,
    events: Vec<ProgressEvent>,
}

impl LineAssembler {
    fn new(parser: ProgressParser) -> Self {
        Self {
            parser,
            line_buf: Vec::new(),
            raw_tail: Vec::new(),
            events: Vec::new(),
        }
    }

    fn ingest(&mut self, bytes: &[u8]) {
        self.raw_tail.extend_from_slice(bytes);
        if self.raw_tail.len() > RAW_TAIL_BYTES {
            let drop = self.raw_tail.len() - RAW_TAIL_BYTES;
            self.raw_tail.drain(..drop);
        }
        self.line_buf.extend_from_slice(bytes);
        while let Some(end) = self
            .line_buf
            .iter()
            .position(|byte| *byte == b'\n' || *byte == b'\r')
        {
            let line: Vec<u8> = self.line_buf.drain(..=end).collect();
            self.process_line(&line);
        }
    }

    fn flush_partial_line(&mut self) {
        if self.line_buf.is_empty() {
            return;
        }
        let line = std::mem::take(&mut self.line_buf);
        self.process_line(&line);
    }

    fn process_line(&mut self, raw: &[u8]) {
        let text = String::from_utf8_lossy(raw);
        let line = strip_ansi(&text);
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let event = match &self.parser {
            ProgressParser::Plain => ProgressEvent::Line(line.to_string()),
            ProgressParser::Percent(regex) => match percent_value(regex, line) {
                Some(value) => ProgressEvent::Percent(value, line.to_string()),
                None => ProgressEvent::Line(line.to_string()),
            },
            ProgressParser::JsonLines => {
                json_event(line).unwrap_or_else(|| ProgressEvent::Line(line.to_string()))
            }
        };
        self.events.push(event);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Data,
    NoData,
    Eof,
}

/// What the reader thread handed over this poll.
enum Incoming {
    /// Output bytes from the PTY.
    Data(Vec<u8>),
    /// The terminal closed: the child is gone and nothing more will arrive.
    Eof,
    /// Nothing arrived within the poll interval.
    Idle,
}

/// The result of one [`CliHandle::pump`].
enum Poll {
    /// An event for the caller.
    Event(ProgressEvent),
    /// Nothing yet; poll again.
    Idle,
    /// The stream is over.
    End,
}

impl CliHandle {
    /// Resolves the program, allocates the PTY and starts the child.
    pub fn spawn(
        spec: &CliSpec,
        command: &CliCommand,
        options: &CliOptions,
    ) -> Result<Self, CliError> {
        let argv = spec.argv(command)?;
        Self::spawn_argv(
            &spec.program,
            &argv,
            spec.progress.clone(),
            command.as_str(),
            options,
        )
    }

    /// Starts an arbitrary program with an explicit argument vector.
    ///
    /// The store presets are argv templates behind [`CliSpec`]; this is the same
    /// driver for a caller whose argv is already built — the upstream mod
    /// installers (DLSSNR, HelixSR), which are not a store CLI. The program is
    /// resolved through `PATH` exactly as a spec's program is, and `label` names
    /// the per-run transcript.
    pub fn spawn_command(
        program: &str,
        args: &[String],
        label: &str,
        options: &CliOptions,
    ) -> Result<Self, CliError> {
        Self::spawn_argv(program, args, ProgressParser::Plain, label, options)
    }

    fn spawn_argv(
        program: &str,
        argv: &[String],
        parser: ProgressParser,
        label: &str,
        options: &CliOptions,
    ) -> Result<Self, CliError> {
        let program = resolve_program(program, options.env.get("PATH").map(String::as_str))?;
        let log_path = create_log_path(options, &program, label)?;
        let log_file = match &log_path {
            Some(path) => Some(fs::File::create(path).map_err(|source| CliError::Log {
                path: path.clone(),
                source,
            })?),
            None => None,
        };

        let pty = native_pty_system()
            .openpty(PTY_SIZE)
            .map_err(|err| CliError::Spawn(err.to_string()))?;
        let reader = pty
            .master
            .try_clone_reader()
            .map_err(|err| CliError::Spawn(err.to_string()))?;
        let writer = pty
            .master
            .take_writer()
            .map_err(|err| CliError::Spawn(err.to_string()))?;

        let mut builder = CommandBuilder::new(&program);
        for arg in argv {
            builder.arg(arg);
        }
        if let Some(cwd) = &options.cwd {
            builder.cwd(cwd);
        }
        for (key, value) in &options.env {
            builder.env(key, value);
        }
        let child = pty
            .slave
            .spawn_command(builder)
            .map_err(|err| CliError::Spawn(err.to_string()))?;
        let killer = child.clone_killer();
        let pid = child.process_id();
        drop(pty);

        let (tx, rx) = sync_channel(READER_QUEUE);
        std::thread::Builder::new()
            .name("pitagames-cli-reader".to_string())
            .spawn(move || pump_output(reader, log_file, tx))
            .map_err(|err| CliError::Spawn(format!("failed to start the reader thread: {err}")))?;

        let now = Instant::now();
        Ok(Self {
            inner: Mutex::new(CliInner {
                child,
                killer,
                pid,
                writer,
                assembler: LineAssembler::new(parser),
                log_path,
                idle_timeout: options.idle_timeout,
                hard_timeout: options.hard_timeout,
                started: now,
                last_activity: now,
                cursor: 0,
                status: None,
                kill_requested: false,
                killed_at: None,
                escalated: false,
                cancelled: false,
                timed_out: None,
                eof: false,
            }),
            inbox: Mutex::new(rx),
        })
    }

    /// Blocks until the next event. `Ok(None)` means the stream is over (the
    /// terminal event was the last one returned).
    pub fn next_event(&self) -> Result<Option<ProgressEvent>, CliError> {
        loop {
            match self.pump(POLL_INTERVAL)? {
                Poll::Event(event) => return Ok(Some(event)),
                Poll::Idle => continue,
                Poll::End => return Ok(None),
            }
        }
    }

    /// Sends input to the CLI's stdin (a prompt answer or a TOTP code).
    ///
    /// A buffered partial line is flushed first, so the prompt the caller is
    /// answering is already part of the stream.
    pub fn write_stdin(&self, input: &[u8]) -> Result<(), CliError> {
        let mut inner = self.lock();
        inner.assembler.flush_partial_line();
        inner
            .writer
            .write_all(input)
            .map_err(|source| CliError::Io { source })?;
        inner
            .writer
            .flush()
            .map_err(|source| CliError::Io { source })
    }

    /// Asks the CLI to stop. Idempotent; the stream then drains to its terminal
    /// event.
    pub fn cancel(&self) {
        let mut inner = self.lock();
        if !inner.kill_requested {
            inner.cancelled = true;
            inner.request_kill();
        }
    }

    /// The per-run raw log, when logging is enabled.
    pub fn log_path(&self) -> Option<PathBuf> {
        self.lock().log_path.clone()
    }

    /// Drains the run to its terminal event and reports the outcome.
    ///
    /// Safe to call after the event stream already ended: the outcome is built
    /// from what the run recorded.
    pub fn wait(self) -> Result<CliOutcome, CliError> {
        self.drain()
    }

    /// The same drain, without giving up the handle.
    ///
    /// An interactive flow keeps the handle alive across threads (one reads the
    /// output, another one types), so it cannot consume it to learn the verdict.
    pub fn drain(&self) -> Result<CliOutcome, CliError> {
        loop {
            if let Poll::End = self.pump(POLL_INTERVAL)? {
                break;
            }
        }
        Ok(self.lock().outcome())
    }

    /// One pass: yield a buffered event, or wait for output and advance the
    /// state machine.
    ///
    /// The state lock is never held across the wait, which is what keeps
    /// [`cancel`](Self::cancel) and [`write_stdin`](Self::write_stdin) usable
    /// from another thread while this one blocks on a silent CLI.
    fn pump(&self, timeout: Duration) -> Result<Poll, CliError> {
        {
            let mut inner = self.lock();
            if let Some(event) = inner.take_buffered() {
                return Ok(Poll::Event(event));
            }
        }

        let incoming = self.receive(timeout)?;

        let mut inner = self.lock();
        inner.step(incoming)?;
        match inner.take_buffered() {
            // The final step appends the terminal event; it is yielded before
            // the end of the stream is reported.
            Some(event) => Ok(Poll::Event(event)),
            None if inner.eof => Ok(Poll::End),
            None => Ok(Poll::Idle),
        }
    }

    /// Waits up to `timeout` for the reader thread, outside the state lock.
    fn receive(&self, timeout: Duration) -> Result<Incoming, CliError> {
        let inbox = self
            .inbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(match inbox.recv_timeout(timeout) {
            Ok(Some(bytes)) => Incoming::Data(bytes),
            // The reader thread ends after closing the channel: the PTY is done.
            Ok(None) | Err(RecvTimeoutError::Disconnected) => Incoming::Eof,
            Err(RecvTimeoutError::Timeout) => Incoming::Idle,
        })
    }

    fn lock(&self) -> MutexGuard<'_, CliInner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Drop for CliHandle {
    fn drop(&mut self) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        if !inner.kill_requested && !inner.eof {
            inner.request_kill();
        }
    }
}

/// The reader thread: drains the PTY, tees raw bytes to the log, and ends the
/// channel with a final `None`.
fn pump_output(
    mut reader: Box<dyn Read + Send>,
    log_file: Option<fs::File>,
    tx: SyncSender<Option<Vec<u8>>>,
) {
    let mut log = log_file.map(BufWriter::new);
    let mut buf = [0u8; READ_CHUNK];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let chunk = &buf[..n];
                if let Some(file) = log.as_mut() {
                    let _ = file.write_all(chunk);
                }
                if tx.send(Some(chunk.to_vec())).is_err() {
                    return;
                }
            }
            Err(_) => break,
        }
    }
    if let Some(file) = log.as_mut() {
        let _ = file.flush();
    }
    let _ = tx.send(None);
}

impl CliInner {
    /// The next event the assembler already produced, if any.
    fn take_buffered(&mut self) -> Option<ProgressEvent> {
        if self.cursor >= self.assembler.events.len() {
            return None;
        }
        let event = self.assembler.events[self.cursor].clone();
        self.cursor += 1;
        Some(event)
    }

    /// Folds one poll's input into the state and maintains the
    /// timeout/escalation machine.
    fn step(&mut self, incoming: Incoming) -> Result<Step, CliError> {
        if self.eof {
            return Ok(Step::Eof);
        }
        match incoming {
            Incoming::Data(bytes) => {
                self.last_activity = Instant::now();
                self.assembler.ingest(&bytes);
                self.supervise();
                Ok(Step::Data)
            }
            Incoming::Eof => self.finish(),
            Incoming::Idle => {
                if !self.assembler.line_buf.is_empty()
                    && self.last_activity.elapsed() >= PARTIAL_FLUSH_QUIET
                {
                    self.assembler.flush_partial_line();
                }
                self.supervise();
                Ok(Step::NoData)
            }
        }
    }

    /// Reaps the child and appends the terminal event.
    fn finish(&mut self) -> Result<Step, CliError> {
        self.assembler.flush_partial_line();
        let status = self
            .child
            .wait()
            .map_err(|source| CliError::Io { source })?;
        let event = self.terminal_event(&status);
        self.assembler.events.push(event);
        self.status = Some(status);
        self.eof = true;
        Ok(Step::Eof)
    }

    fn terminal_event(&self, status: &ExitStatus) -> ProgressEvent {
        if status.success() && !self.cancelled && self.timed_out.is_none() {
            return ProgressEvent::Done;
        }
        ProgressEvent::Failed {
            code: status.exit_code() as i32,
            message: self.failure_message(status),
        }
    }

    fn failure_message(&self, status: &ExitStatus) -> String {
        if self.cancelled {
            return "cancelled by user".to_string();
        }
        if let Some(kind) = self.timed_out {
            let limit = match kind {
                TimeoutKind::Idle => self.idle_timeout,
                TimeoutKind::Hard => self.hard_timeout,
            };
            if let Some(limit) = limit {
                return match kind {
                    TimeoutKind::Idle => format!(
                        "idle timeout after {} without output",
                        format_duration(limit)
                    ),
                    TimeoutKind::Hard => {
                        format!("hard timeout after {}", format_duration(limit))
                    }
                };
            }
            return format!("{kind} timeout");
        }
        if let Some(text) = self.last_text() {
            return text;
        }
        status.to_string()
    }

    fn last_text(&self) -> Option<String> {
        self.assembler
            .events
            .iter()
            .rev()
            .find_map(|event| match event {
                ProgressEvent::Line(text)
                | ProgressEvent::Stage(text)
                | ProgressEvent::Percent(_, text) => {
                    Some(text.chars().take(FAILURE_TEXT_CHARS).collect::<String>())
                }
                ProgressEvent::Done | ProgressEvent::Failed { .. } => None,
            })
    }

    fn supervise(&mut self) {
        let now = Instant::now();
        if !self.kill_requested {
            if let Some(idle) = self.idle_timeout
                && now.duration_since(self.last_activity) >= idle
            {
                self.timed_out = Some(TimeoutKind::Idle);
                self.request_kill();
            }
            if self.timed_out.is_none()
                && let Some(hard) = self.hard_timeout
                && now.duration_since(self.started) >= hard
            {
                self.timed_out = Some(TimeoutKind::Hard);
                self.request_kill();
            }
        }
        if self.kill_requested
            && !self.escalated
            && let Some(at) = self.killed_at
            && now.duration_since(at) >= ESCALATE_GRACE
        {
            self.escalated = true;
            self.kill_group();
        }
    }

    fn request_kill(&mut self) {
        self.kill_requested = true;
        self.killed_at.get_or_insert_with(Instant::now);
        let _ = self.killer.kill();
    }

    /// `SIGKILL` the child's process group.
    ///
    /// `portable-pty` puts the child in its own session, so its pid is the
    /// group id and killing `-pid` takes a CLI's workers down with it. A CLI
    /// that ignores `SIGHUP` (or one whose worker holds the PTY open) would
    /// otherwise keep the run alive until the hard timeout.
    fn kill_group(&self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid.filter(|pid| *pid != 0) {
            // SAFETY: plain `kill(2)`; a stale group id can only fail with
            // ESRCH, and the result is deliberately ignored.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }

    fn outcome(&self) -> CliOutcome {
        let status = self
            .status
            .clone()
            .unwrap_or_else(|| ExitStatus::with_exit_code(1));
        CliOutcome {
            success: status.success() && !self.cancelled && self.timed_out.is_none(),
            code: status.exit_code(),
            cancelled: self.cancelled,
            timed_out: self.timed_out,
            log_path: self.log_path.clone(),
            events: self.assembler.events.clone(),
            raw_tail: String::from_utf8_lossy(&self.assembler.raw_tail).into_owned(),
        }
    }
}

fn percent_value(regex: &Regex, line: &str) -> Option<f32> {
    let captures = regex.captures(line)?;
    let raw = captures.get(1).or_else(|| captures.get(0))?.as_str().trim();
    let value: f32 = raw.parse().ok()?;
    (0.0..=100.0).contains(&value).then_some(value)
}

fn json_event(line: &str) -> Option<ProgressEvent> {
    let trimmed = line.trim_start();
    if !trimmed.starts_with('{') {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    let object = value.as_object()?;
    let stage = object
        .get("stage")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let percent = object
        .get("percent")
        .or_else(|| object.get("progress"))
        .and_then(serde_json::Value::as_f64)
        .filter(|value| (0.0..=100.0).contains(value))
        .map(|value| value as f32);
    match (stage, percent) {
        (Some(stage), Some(percent)) => Some(ProgressEvent::Percent(percent, stage)),
        (Some(stage), None) => Some(ProgressEvent::Stage(stage)),
        (None, Some(percent)) => Some(ProgressEvent::Percent(percent, line.to_string())),
        (None, None) => None,
    }
}

/// Removes ANSI/VT escape sequences and stray control characters.
///
/// CLIs colourise their output and drive progress bars with cursor escapes;
/// neither belongs in an event or a failure message. Tabs and printable text
/// survive.
pub fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\x1b' {
            if ch >= ' ' || ch == '\t' {
                out.push(ch);
            }
            continue;
        }
        let Some(kind) = chars.peek().copied() else {
            continue;
        };
        match kind {
            // Control sequence introducer: ESC [ params/intermediates final.
            '[' => {
                chars.next();
                for next in chars.by_ref() {
                    if matches!(next, '\x40'..='\x7e') {
                        break;
                    }
                }
            }
            // Operating system command: ESC ] ... BEL | ST.
            ']' => {
                chars.next();
                for next in chars.by_ref() {
                    match next {
                        '\x07' => break,
                        '\x1b' => {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                        _ => {}
                    }
                }
            }
            // Charset designation: ESC ( / ) + one byte.
            '(' | ')' => {
                chars.next();
                chars.next();
            }
            // Other two-byte escape: drop the introducer and its byte.
            _ => {
                chars.next();
            }
        }
    }
    out
}

fn format_duration(duration: Duration) -> String {
    if duration.as_millis() < 1_000 {
        format!("{} ms", duration.as_millis())
    } else if duration.as_secs() < 120 {
        format!("{} s", duration.as_secs())
    } else {
        format!("{} min", duration.as_secs() / 60)
    }
}

/// Finds the program a run would execute, without running it.
///
/// This is the "is the store CLI installed?" question: an absolute path must be
/// an executable file, a bare name is looked up in `path_override` (a `PATH`
/// value) or the inherited `PATH`, and a miss is [`CliError::NotFound`] naming
/// the program — the state the store status UI shows.
pub fn resolve_program(program: &str, path_override: Option<&str>) -> Result<PathBuf, CliError> {
    let path = Path::new(program);
    if path.components().count() > 1 {
        if is_executable(path) {
            return Ok(path.to_path_buf());
        }
        return Err(CliError::NotFound {
            program: program.to_string(),
        });
    }
    let path_var = match path_override {
        Some(value) => value.to_string(),
        None => std::env::var("PATH").unwrap_or_default(),
    };
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(program);
        if is_executable(&candidate) {
            return Ok(candidate);
        }
    }
    Err(CliError::NotFound {
        program: program.to_string(),
    })
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match fs::metadata(path) {
            Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

fn create_log_path(
    options: &CliOptions,
    program: &Path,
    label: &str,
) -> Result<Option<PathBuf>, CliError> {
    let Some(dir) = &options.log_dir else {
        return Ok(None);
    };
    fs::create_dir_all(dir).map_err(|source| CliError::Log {
        path: dir.clone(),
        source,
    })?;
    let stem = program.file_stem().and_then(OsStr::to_str).unwrap_or("cli");
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let name = format!("cli-{stem}-{label}-{nanos}-{}.log", std::process::id());
    Ok(Some(dir.join(name)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn percent_parser() -> ProgressParser {
        ProgressParser::Percent(Regex::new(r"(\d{1,3}(?:\.\d+)?)\s*%").unwrap())
    }

    fn spec(program: &str, parser: ProgressParser) -> CliSpec {
        CliSpec {
            program: program.to_string(),
            needs_tty: false,
            progress: parser,
            list_installed: Vec::new(),
            list_owned: Vec::new(),
            auth: vec!["auth".into()],
            logout: Vec::new(),
            sync: Vec::new(),
            install: Vec::new(),
            verify: Vec::new(),
            launch: vec!["launch".into()],
            status: Vec::new(),
            version: Vec::new(),
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

    /// Options whose `PATH` starts with `dir`, so a bare program name resolves
    /// to a fake CLI in that directory (the rest of `PATH` stays available for
    /// the script's own `sleep`/`cat`).
    fn options(dir: &Path) -> CliOptions {
        let mut env = BTreeMap::new();
        let path = std::env::var("PATH").unwrap_or_default();
        env.insert("PATH".to_string(), format!("{}:{path}", dir.display()));
        CliOptions {
            log_dir: Some(dir.join("logs")),
            env,
            ..CliOptions::default()
        }
    }

    fn run(spec: &CliSpec, options: &CliOptions) -> Result<CliOutcome, CliError> {
        let command = CliCommand::Launch {
            id: "test".to_string(),
            install_dir: None,
        };
        CliHandle::spawn(spec, &command, options)?.wait()
    }

    /// Waits (bounded) for a fake CLI to signal that it reached a known point.
    /// Used instead of sleeping a fixed amount, so a slow machine cannot make
    /// a test act before the child is ready.
    fn wait_for_file(path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if path.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the fake CLI never created {}", path.display());
    }

    fn last_event(outcome: &CliOutcome) -> &ProgressEvent {
        outcome.events.last().expect("a terminal event")
    }

    #[test]
    fn argv_substitutes_install_placeholders() {
        let mut spec = spec("legendary", ProgressParser::Plain);
        spec.install = vec![
            "install".into(),
            "{id}".into(),
            "--base-path".into(),
            "{dir}".into(),
        ];
        let argv = spec
            .argv(&CliCommand::Install {
                id: "Gizmo".into(),
                install_dir: PathBuf::from("/games/Gizmo Deluxe"),
            })
            .unwrap();
        assert_eq!(
            argv,
            ["install", "Gizmo", "--base-path", "/games/Gizmo Deluxe"]
        );
    }

    #[test]
    fn argv_splits_an_install_directory_into_parent_and_name() {
        let mut spec = spec("legendary", ProgressParser::Plain);
        spec.install = vec![
            "install".into(),
            "{id}".into(),
            "--base-path".into(),
            "{parent}".into(),
            "--game-folder".into(),
            "{name}".into(),
        ];

        let argv = spec
            .argv(&CliCommand::Install {
                id: "Gizmo".into(),
                install_dir: PathBuf::from("/games/Epic/Gizmo"),
            })
            .unwrap();
        assert_eq!(
            argv,
            [
                "install",
                "Gizmo",
                "--base-path",
                "/games/Epic",
                "--game-folder",
                "Gizmo"
            ]
        );

        // A path with no file name (`/`) cannot fill `{name}`; the run must fail
        // before it starts rather than install somewhere else.
        let error = spec
            .argv(&CliCommand::Install {
                id: "Gizmo".into(),
                install_dir: PathBuf::from("/"),
            })
            .unwrap_err();
        assert!(matches!(error, CliError::Template { .. }), "{error}");
    }

    #[test]
    fn verify_status_version_and_logout_use_their_own_templates() {
        let mut spec = spec("legendary", ProgressParser::Plain);
        spec.verify = vec!["verify".into(), "{id}".into()];
        spec.status = vec!["status".into(), "--offline".into(), "--json".into()];
        spec.version = vec!["--version".into()];
        spec.logout = vec!["auth".into(), "--delete".into()];
        spec.sync = vec!["library".into(), "sync".into()];

        assert_eq!(
            spec.argv(&CliCommand::Verify {
                id: "Gizmo".into(),
                install_dir: None,
            })
            .unwrap(),
            ["verify", "Gizmo"]
        );
        assert_eq!(
            spec.argv(&CliCommand::Status).unwrap(),
            ["status", "--offline", "--json"]
        );
        assert_eq!(spec.argv(&CliCommand::Version).unwrap(), ["--version"]);
        assert_eq!(
            spec.argv(&CliCommand::Logout).unwrap(),
            ["auth", "--delete"]
        );
        assert_eq!(spec.argv(&CliCommand::Sync).unwrap(), ["library", "sync"]);
        assert_eq!(
            CliCommand::Verify {
                id: "x".into(),
                install_dir: None,
            }
            .as_str(),
            "verify"
        );
        assert_eq!(CliCommand::Status.as_str(), "status");
        assert_eq!(CliCommand::Sync.as_str(), "sync");
    }

    #[test]
    fn a_command_without_a_template_is_unsupported_not_an_empty_argv() {
        let spec = spec("legendary", ProgressParser::Plain);
        let error = spec.argv(&CliCommand::Sync).unwrap_err();
        assert!(
            matches!(error, CliError::Unsupported { command: "sync" }),
            "{error}"
        );
        assert!(
            error.to_string().contains("no template for `sync`"),
            "{error}"
        );
    }

    #[test]
    fn store_status_names_the_program_it_probed() {
        let dir = tempfile::tempdir().unwrap();
        let missing = StoreStatus::probe("no-such-cli-xyz", Some(dir.path().to_str().unwrap()));
        assert!(!missing.is_ready());
        assert_eq!(missing.program(), "no-such-cli-xyz");
        assert_eq!(missing.path(), None);

        let program = script(dir.path(), "fake-cli", "#!/bin/sh\nexit 0\n");
        let ready = StoreStatus::probe("fake-cli", Some(dir.path().to_str().unwrap()));
        assert!(ready.is_ready());
        assert_eq!(ready.program(), "fake-cli");
        assert_eq!(ready.path(), Some(program.as_path()));
    }

    #[test]
    fn argv_rejects_unavailable_and_malformed_placeholders() {
        let mut spec = spec("legendary", ProgressParser::Plain);
        spec.launch = vec!["launch".into(), "{dir}".into()];
        let error = spec
            .argv(&CliCommand::Launch {
                id: "Gizmo".into(),
                install_dir: None,
            })
            .unwrap_err();
        assert!(matches!(error, CliError::Template { .. }), "{error}");

        spec.launch = vec!["launch".into(), "{id".into()];
        let error = spec
            .argv(&CliCommand::Launch {
                id: "Gizmo".into(),
                install_dir: None,
            })
            .unwrap_err();
        assert!(matches!(error, CliError::Template { .. }), "{error}");

        spec.launch = vec!["launch".into(), "plain".into()];
        assert_eq!(
            spec.argv(&CliCommand::Launch {
                id: "x".into(),
                install_dir: None,
            })
            .unwrap(),
            ["launch", "plain"]
        );
    }

    #[test]
    fn argv_does_not_rescan_substituted_values() {
        let mut spec = spec("legendary", ProgressParser::Plain);
        spec.launch = vec!["launch".into(), "{id}".into()];
        let argv = spec
            .argv(&CliCommand::Launch {
                id: "{dir}".into(),
                install_dir: None,
            })
            .unwrap();
        assert_eq!(argv, ["launch", "{dir}"]);
    }

    #[test]
    fn plain_parser_keeps_lines() {
        let mut inner = LineAssembler::new(ProgressParser::Plain);
        inner.process_line(b"one\n");
        inner.process_line(b"two\n");
        assert_eq!(
            inner.events,
            [
                ProgressEvent::Line("one".into()),
                ProgressEvent::Line("two".into())
            ]
        );
    }

    #[test]
    fn percent_parser_emits_percent_and_falls_back() {
        let mut inner = LineAssembler::new(percent_parser());
        inner.process_line(b"downloading 42.5% of 1.2 GiB\n");
        inner.process_line(b"still 100% done\n");
        inner.process_line(b"999% is not a percentage\n");
        inner.process_line(b"no number here\n");
        assert_eq!(
            inner.events,
            [
                ProgressEvent::Percent(42.5, "downloading 42.5% of 1.2 GiB".into()),
                ProgressEvent::Percent(100.0, "still 100% done".into()),
                ProgressEvent::Line("999% is not a percentage".into()),
                ProgressEvent::Line("no number here".into()),
            ]
        );
    }

    #[test]
    fn json_lines_parser_maps_stage_and_percent() {
        let mut inner = LineAssembler::new(ProgressParser::JsonLines);
        inner.process_line(br#"{"stage":"downloading","percent":42}"#);
        inner.process_line(br#"{"stage":"verifying"}"#);
        inner.process_line(br#"{"note":"no progress fields"}"#);
        inner.process_line(br#"["not","an","object"]"#);
        inner.process_line(b"not json\n");
        assert_eq!(
            inner.events,
            [
                ProgressEvent::Percent(42.0, "downloading".into()),
                ProgressEvent::Stage("verifying".into()),
                ProgressEvent::Line(r#"{"note":"no progress fields"}"#.into()),
                ProgressEvent::Line(r#"["not","an","object"]"#.into()),
                ProgressEvent::Line("not json".into()),
            ]
        );
    }

    #[test]
    fn json_percent_out_of_range_is_ignored() {
        let mut inner = LineAssembler::new(ProgressParser::JsonLines);
        inner.process_line(br#"{"percent":1234}"#);
        assert_eq!(
            inner.events,
            [ProgressEvent::Line(r#"{"percent":1234}"#.into())]
        );
    }

    #[test]
    fn ingest_splits_on_newlines_and_carriage_returns() {
        let mut inner = LineAssembler::new(ProgressParser::Plain);
        inner.ingest(b"first\rsecond\nthird\r\n");
        assert_eq!(
            inner.events,
            [
                ProgressEvent::Line("first".into()),
                ProgressEvent::Line("second".into()),
                ProgressEvent::Line("third".into()),
            ]
        );
    }

    #[test]
    fn ingest_keeps_an_incomplete_line_until_it_is_flushed() {
        let mut inner = LineAssembler::new(ProgressParser::Plain);
        inner.ingest(b"prompt: ");
        assert!(inner.events.is_empty());
        inner.flush_partial_line();
        assert_eq!(inner.events, [ProgressEvent::Line("prompt:".into())]);
    }

    #[test]
    fn strip_ansi_removes_colours_and_controls() {
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m"), "red");
        assert_eq!(strip_ansi("a\x1b[1;32mb\x1b[Kc"), "abc");
        assert_eq!(strip_ansi("\x1b]0;window title\x07body"), "body");
        assert_eq!(strip_ansi("\x1b[?25l"), "");
        assert_eq!(strip_ansi("x\x1b)y"), "x");
        assert_eq!(strip_ansi("ok\x1b"), "ok");
        assert_eq!(strip_ansi("a\x07b\x00c"), "abc");
        assert_eq!(strip_ansi("plain text"), "plain text");
        assert_eq!(strip_ansi("tab\tstays"), "tab\tstays");
    }

    #[test]
    fn resolve_program_finds_path_entries_and_absolute_files() {
        let dir = tempfile::tempdir().unwrap();
        let program = script(dir.path(), "fake-cli", "#!/bin/sh\nexit 0\n");

        let resolved = resolve_program("fake-cli", Some(dir.path().to_str().unwrap())).unwrap();
        assert_eq!(resolved, program);

        let missing = resolve_program("no-such-cli-xyz", Some(dir.path().to_str().unwrap()));
        assert!(matches!(missing, Err(CliError::NotFound { .. })));

        let absolute = resolve_program(program.to_str().unwrap(), None).unwrap();
        assert_eq!(absolute, program);

        let relative = resolve_program("missing/relative", None);
        assert!(matches!(relative, Err(CliError::NotFound { .. })));
    }

    #[test]
    fn plain_list_emits_lines_and_done() {
        let dir = tempfile::tempdir().unwrap();
        script(
            dir.path(),
            "fake-list",
            "#!/bin/sh\necho 'game one'\necho 'game two'\nexit 0\n",
        );
        let outcome = run(
            &spec("fake-list", ProgressParser::Plain),
            &options(dir.path()),
        )
        .unwrap();
        assert!(outcome.success);
        assert_eq!(outcome.code, 0);
        assert_eq!(
            outcome.events,
            [
                ProgressEvent::Line("game one".into()),
                ProgressEvent::Line("game two".into()),
                ProgressEvent::Done,
            ]
        );
        assert!(outcome.raw_tail.contains("game two"));
        assert!(outcome.log_path.as_ref().unwrap().exists());
    }

    #[test]
    fn percent_install_emits_percent_events() {
        let dir = tempfile::tempdir().unwrap();
        script(
            dir.path(),
            "fake-install",
            "#!/bin/sh\necho 'Fetching manifest...'\n\
             echo 'downloading 10% (1.0/10.0 GiB)'\n\
             echo 'downloading 55% (5.5/10.0 GiB)'\n\
             echo 'Install complete.'\nexit 0\n",
        );
        let outcome = run(
            &spec("fake-install", percent_parser()),
            &options(dir.path()),
        )
        .unwrap();
        assert!(outcome.success);
        let percents: Vec<f32> = outcome
            .events
            .iter()
            .filter_map(|event| match event {
                ProgressEvent::Percent(value, _) => Some(*value),
                _ => None,
            })
            .collect();
        assert_eq!(percents, [10.0, 55.0]);
        assert!(
            outcome
                .events
                .contains(&ProgressEvent::Line("Install complete.".into()))
        );
        assert_eq!(last_event(&outcome), &ProgressEvent::Done);

        let log = fs::read_to_string(outcome.log_path.as_ref().unwrap()).unwrap();
        assert!(log.contains("downloading 55% (5.5/10.0 GiB)"), "{log}");
    }

    #[test]
    fn json_progress_lines_become_stage_and_percent() {
        let dir = tempfile::tempdir().unwrap();
        script(
            dir.path(),
            "fake-json",
            "#!/bin/sh\n\
             echo '{\"stage\":\"downloading\",\"percent\":42}'\n\
             echo '{\"stage\":\"verifying\"}'\nexit 0\n",
        );
        let outcome = run(
            &spec("fake-json", ProgressParser::JsonLines),
            &options(dir.path()),
        )
        .unwrap();
        assert_eq!(
            outcome.events,
            [
                ProgressEvent::Percent(42.0, "downloading".into()),
                ProgressEvent::Stage("verifying".into()),
                ProgressEvent::Done,
            ]
        );
    }

    #[test]
    fn auth_round_trips_stdin_and_reports_success() {
        let dir = tempfile::tempdir().unwrap();
        script(
            dir.path(),
            "fake-auth",
            "#!/bin/sh\n\
             printf 'Enter your Epic code: '\n\
             read code\n\
             if [ \"$code\" = \"123456\" ]; then echo 'Auth OK'; exit 0; fi\n\
             echo 'error: your session has expired'\nexit 1\n",
        );
        let spec = spec("fake-auth", ProgressParser::Plain);
        let options = CliOptions {
            idle_timeout: None,
            ..options(dir.path())
        };
        let handle = CliHandle::spawn(&spec, &CliCommand::Auth, &options).unwrap();
        let prompt = handle.next_event().unwrap().unwrap();
        assert!(
            matches!(&prompt, ProgressEvent::Line(text) if text == "Enter your Epic code:"),
            "{prompt:?}"
        );
        handle.write_stdin(b"123456\n").unwrap();
        let outcome = handle.wait().unwrap();
        assert!(outcome.success);
        assert!(
            outcome
                .events
                .contains(&ProgressEvent::Line("Auth OK".into())),
            "{:?}",
            outcome.events
        );
    }

    #[test]
    fn expired_auth_reports_the_cli_error() {
        let dir = tempfile::tempdir().unwrap();
        script(
            dir.path(),
            "fake-auth",
            "#!/bin/sh\n\
             printf 'Enter your Epic code: '\n\
             read code\n\
             echo 'error: your session has expired'\nexit 1\n",
        );
        let spec = spec("fake-auth", ProgressParser::Plain);
        let options = CliOptions {
            idle_timeout: None,
            ..options(dir.path())
        };
        let handle = CliHandle::spawn(&spec, &CliCommand::Auth, &options).unwrap();
        let _prompt = handle.next_event().unwrap().unwrap();
        handle.write_stdin(b"000000\n").unwrap();
        let outcome = handle.wait().unwrap();
        assert!(!outcome.success);
        assert_eq!(outcome.code, 1);
        assert_eq!(
            last_event(&outcome),
            &ProgressEvent::Failed {
                code: 1,
                message: "error: your session has expired".into(),
            }
        );
    }

    #[test]
    fn disk_full_and_network_failures_name_the_cli_error() {
        for (name, line) in [
            ("fake-disk", "error: No space left on device"),
            ("fake-network", "error: Network request timed out"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            script(
                dir.path(),
                name,
                &format!("#!/bin/sh\necho 'working...'\necho '{line}'\nexit 1\n"),
            );
            let outcome = run(&spec(name, ProgressParser::Plain), &options(dir.path())).unwrap();
            assert!(!outcome.success);
            assert_eq!(
                last_event(&outcome),
                &ProgressEvent::Failed {
                    code: 1,
                    message: line.into(),
                }
            );
        }
    }

    #[test]
    fn ansi_output_is_cleaned_in_events_and_messages() {
        let dir = tempfile::tempdir().unwrap();
        script(
            dir.path(),
            "fake-ansi",
            "#!/bin/sh\nprintf '\\033[31merror: boom\\033[0m\\n'\nexit 1\n",
        );
        let outcome = run(
            &spec("fake-ansi", ProgressParser::Plain),
            &options(dir.path()),
        )
        .unwrap();
        assert_eq!(
            outcome.events,
            [
                ProgressEvent::Line("error: boom".into()),
                ProgressEvent::Failed {
                    code: 1,
                    message: "error: boom".into(),
                },
            ]
        );
        // The raw log keeps the escapes: it is the unmodified transcript.
        let log = fs::read_to_string(outcome.log_path.as_ref().unwrap()).unwrap();
        assert!(log.contains("\x1b[31m"), "{log:?}");
    }

    #[test]
    fn a_trailing_line_without_a_newline_is_still_reported() {
        let dir = tempfile::tempdir().unwrap();
        script(
            dir.path(),
            "fake-tail",
            "#!/bin/sh\nprintf 'final line without newline'\nexit 0\n",
        );
        let outcome = run(
            &spec("fake-tail", ProgressParser::Plain),
            &options(dir.path()),
        )
        .unwrap();
        assert_eq!(
            outcome.events,
            [
                ProgressEvent::Line("final line without newline".into()),
                ProgressEvent::Done,
            ]
        );
    }

    #[test]
    fn carriage_return_redraws_become_separate_lines() {
        let dir = tempfile::tempdir().unwrap();
        script(
            dir.path(),
            "fake-redraw",
            "#!/bin/sh\nprintf 'part1\\rpart2\\ntail\\r\\n'\nexit 0\n",
        );
        let outcome = run(
            &spec("fake-redraw", ProgressParser::Plain),
            &options(dir.path()),
        )
        .unwrap();
        assert_eq!(
            outcome.events,
            [
                ProgressEvent::Line("part1".into()),
                ProgressEvent::Line("part2".into()),
                ProgressEvent::Line("tail".into()),
                ProgressEvent::Done,
            ]
        );
    }

    #[test]
    fn a_missing_program_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let error = run(
            &spec("no-such-cli-xyz", ProgressParser::Plain),
            &options(dir.path()),
        )
        .unwrap_err();
        assert!(
            matches!(&error, CliError::NotFound { program } if program == "no-such-cli-xyz"),
            "{error}"
        );
    }

    #[test]
    fn idle_timeout_kills_a_silent_install() {
        let dir = tempfile::tempdir().unwrap();
        // Never prints: the idle clock starts at spawn and nothing resets it,
        // so the test cannot race a slow `fork`+`exec` on a loaded machine.
        script(dir.path(), "fake-idle", "#!/bin/sh\nsleep 30\n");
        let options = CliOptions {
            idle_timeout: Some(Duration::from_secs(2)),
            hard_timeout: Some(Duration::from_secs(20)),
            ..options(dir.path())
        };
        let started = Instant::now();
        let outcome = run(&spec("fake-idle", ProgressParser::Plain), &options).unwrap();
        assert_eq!(outcome.timed_out, Some(TimeoutKind::Idle));
        assert!(!outcome.success);
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(
            matches!(last_event(&outcome), ProgressEvent::Failed { message, .. } if message.contains("idle timeout")),
            "{:?}",
            outcome.events
        );
    }

    #[test]
    fn hard_timeout_kills_a_hung_install() {
        let dir = tempfile::tempdir().unwrap();
        script(dir.path(), "fake-hang", "#!/bin/sh\nsleep 30\n");
        let options = CliOptions {
            idle_timeout: None,
            hard_timeout: Some(Duration::from_millis(400)),
            ..options(dir.path())
        };
        let started = Instant::now();
        let outcome = run(&spec("fake-hang", ProgressParser::Plain), &options).unwrap();
        assert_eq!(outcome.timed_out, Some(TimeoutKind::Hard));
        assert!(!outcome.success);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_child_that_ignores_sighup_is_killed_by_process_group() {
        let dir = tempfile::tempdir().unwrap();
        // `SIG_IGN` survives exec, so `sleep` inherits it too: only the
        // process-group `SIGKILL` can end this run. The marker file is created
        // *after* the trap is installed, so the test cancels only once
        // `SIGHUP` is provably ignored — no startup race.
        script(
            dir.path(),
            "fake-stubborn",
            "#!/bin/sh\ntrap '' HUP\n: > \"$1\"\necho 'alive'\nsleep 30\n",
        );
        let marker = dir.path().join("trap-installed");
        let mut spec = spec("fake-stubborn", ProgressParser::Plain);
        spec.launch = vec!["{id}".into()];
        let options = CliOptions {
            idle_timeout: None,
            // Bounds the test: if escalation were broken the run would only
            // end here, and the elapsed assertion below fails loudly.
            hard_timeout: Some(Duration::from_secs(20)),
            ..options(dir.path())
        };
        let handle = CliHandle::spawn(
            &spec,
            &CliCommand::Launch {
                id: marker.to_string_lossy().into_owned(),
                install_dir: None,
            },
            &options,
        )
        .unwrap();
        wait_for_file(&marker);
        let started = Instant::now();
        handle.cancel();
        let outcome = handle.wait().unwrap();
        assert!(outcome.cancelled);
        assert!(!outcome.success);
        assert_eq!(outcome.timed_out, None);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the process group should be SIGKILLed after the grace period"
        );
        assert!(
            matches!(last_event(&outcome), ProgressEvent::Failed { message, .. } if message == "cancelled by user"),
            "{:?}",
            outcome.events
        );
        // The shell ignored `SIGHUP`, so it got to print this during the grace
        // period: the output before the kill is not lost.
        assert!(
            outcome
                .events
                .contains(&ProgressEvent::Line("alive".into())),
            "{:?}",
            outcome.events
        );
    }

    #[test]
    fn another_thread_can_answer_a_prompt_while_the_reader_waits() {
        let dir = tempfile::tempdir().unwrap();
        script(
            dir.path(),
            "fake-interactive",
            "#!/bin/sh\nprintf 'code: '\nread code\necho \"got:$code\"\nexit 0\n",
        );
        let spec = spec("fake-interactive", ProgressParser::Plain);
        let options = CliOptions {
            idle_timeout: None,
            ..options(dir.path())
        };
        let handle =
            std::sync::Arc::new(CliHandle::spawn(&spec, &CliCommand::Auth, &options).unwrap());

        // The writer types while the reader is blocked on the child, which is
        // the whole reason `next_event` releases the lock between polls: a
        // caller that held it across the wait would deadlock here.
        let writer = std::sync::Arc::clone(&handle);
        let answer = std::thread::spawn(move || writer.write_stdin(b"123456\n"));

        let mut seen = Vec::new();
        while let Some(event) = handle.next_event().unwrap() {
            seen.push(event);
        }
        answer.join().unwrap().unwrap();

        assert!(
            seen.iter().any(
                |event| matches!(event, ProgressEvent::Line(text) if text.contains("got:123456"))
            ),
            "{seen:?}"
        );
        assert_eq!(seen.last(), Some(&ProgressEvent::Done));
    }

    #[test]
    fn a_second_thread_is_not_starved_by_the_poll_loop() {
        let dir = tempfile::tempdir().unwrap();
        // Prints one line, then goes quiet: the reader spends the whole test
        // blocked on a CLI that says nothing.
        script(
            dir.path(),
            "fake-quiet",
            "#!/bin/sh\n: > \"$1\"\necho ready\nsleep 30\n",
        );
        let marker = dir.path().join("up");
        let mut spec = spec("fake-quiet", ProgressParser::Plain);
        spec.launch = vec!["{id}".into()];
        let options = CliOptions {
            idle_timeout: None,
            // Bounds the test: a starved lock would only be released here.
            hard_timeout: Some(Duration::from_secs(30)),
            ..options(dir.path())
        };
        let handle = std::sync::Arc::new(
            CliHandle::spawn(
                &spec,
                &CliCommand::Launch {
                    id: marker.to_string_lossy().into_owned(),
                    install_dir: None,
                },
                &options,
            )
            .unwrap(),
        );
        wait_for_file(&marker);

        let reader = {
            let handle = std::sync::Arc::clone(&handle);
            std::thread::spawn(move || {
                let mut lines = Vec::new();
                while let Some(event) = handle.next_event().unwrap() {
                    lines.push(event);
                }
                lines
            })
        };
        std::thread::sleep(Duration::from_millis(300));

        // A cancel or a typed answer takes this lock from another thread; it
        // must not wait for the poll loop to finish a slice, let alone for the
        // CLI to say something.
        let started = Instant::now();
        let probe = std::sync::Arc::clone(&handle);
        let reached = std::thread::spawn(move || probe.log_path()).join().unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "a second thread waited {:?} for the state lock",
            started.elapsed()
        );
        assert!(reached.is_some());

        handle.cancel();
        let seen = reader.join().unwrap();
        assert!(
            seen.iter()
                .any(|event| matches!(event, ProgressEvent::Line(text) if text == "ready")),
            "{seen:?}"
        );
    }

    #[test]
    fn cancel_ends_the_run_and_reports_it() {
        let dir = tempfile::tempdir().unwrap();
        script(
            dir.path(),
            "fake-cancel",
            "#!/bin/sh\ntrap 'exit 0' HUP\necho 'waiting'\nsleep 30\n",
        );
        let spec = spec("fake-cancel", ProgressParser::Plain);
        let options = CliOptions {
            idle_timeout: None,
            ..options(dir.path())
        };
        let handle = CliHandle::spawn(
            &spec,
            &CliCommand::Launch {
                id: "test".into(),
                install_dir: None,
            },
            &options,
        )
        .unwrap();
        let first = handle.next_event().unwrap().unwrap();
        assert_eq!(first, ProgressEvent::Line("waiting".into()));
        handle.cancel();
        let outcome = handle.wait().unwrap();
        assert!(outcome.cancelled);
        assert!(!outcome.success);
        assert!(
            matches!(last_event(&outcome), ProgressEvent::Failed { message, .. } if message == "cancelled by user"),
            "{:?}",
            outcome.events
        );
    }
}
