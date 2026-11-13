//! What a store CLI's own output says, beyond its exit code.
//!
//! Both wrapped stores log through their own formatter, and the two place the
//! level and the logger in opposite orders:
//!
//! - legendary: `[cli] INFO: message`
//! - nile (Python `logging`): `INFO [CLI]:\t message`
//!
//! GOG's `gogdl` also uses Python `logging`, but with legendary's bracketed
//! order (`[%(name)s] %(levelname)s: …`), so it parses with
//! [`LogFormat::Legendary`]'s layout and has its own name
//! ([`LogFormat::Gogdl`]) only so a reader knows which CLI wrote the line.
//!
//! Reading those lines is what makes a store backend honest, because the exit
//! code is not the verdict for either CLI: both log an error and still exit `0`
//! for whole classes of refusal (see `docs/m2-epic.md`, `docs/m2-amazon.md`).
//! [`RunReport`] keeps the text, the levels and the outcome together, and
//! [`LogFormat`] is the only part that differs per store.
//!
//! The split also matters for `launch`: once a game runs, it writes to the same
//! PTY, so its output must not be mistaken for the CLI's. A game does not print
//! `LEVEL [logger]:` lines, which is what lets Amazon tell "nile refused" from
//! "the game exited".

use crate::cli::{CliOutcome, ProgressEvent};

/// One CLI's log-line shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// `[cli] INFO: message` — legendary's `StreamHandler` format.
    Legendary,
    /// `INFO [CLI]:\t message` — nile's `logging.basicConfig` format.
    Nile,
    /// `[MANAGER] INFO: message` — gogdl's `logging` format. It is the same
    /// bracketed layout as [`LogFormat::Legendary`], named separately so a call
    /// site says which CLI it means.
    Gogdl,
}

/// A line the CLI logged itself, split into its parts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogLine<'a> {
    /// `DEBUG`, `INFO`, `WARNING`, `ERROR`, `CRITICAL` or `FATAL`.
    pub level: &'static str,
    /// The logger name inside the brackets (`cli`, `LIBRARY`, `LAUNCHER`, …).
    pub source: &'a str,
    /// The message, with the prefix and the layout whitespace removed.
    pub message: &'a str,
}

/// The levels Python's `logging` and legendary both use.
const LOG_LEVELS: [&str; 6] = ["DEBUG", "INFO", "WARNING", "ERROR", "CRITICAL", "FATAL"];

impl LogFormat {
    /// Splits a line the CLI logged once; `None` for anything else (a JSON
    /// document, a game's output, a prompt).
    pub fn parse<'a>(self, line: &'a str) -> Option<LogLine<'a>> {
        match self {
            // gogdl uses legendary's bracketed order, so the layout is shared.
            Self::Legendary | Self::Gogdl => {
                let rest = line.trim_start().strip_prefix('[')?;
                let (source, after) = rest.split_once("] ")?;
                // The level must be followed by `:`; ordinary prose that
                // happens to start with "INFO" is not a log line.
                let (level, message) = LOG_LEVELS.iter().find_map(|level| {
                    let after = after.strip_prefix(level)?;
                    Some((*level, after.strip_prefix(':')?))
                })?;
                Some(LogLine {
                    level,
                    source,
                    message: message.trim_start(),
                })
            }
            Self::Nile => {
                let line = line.trim_start();
                let (level, after) = line.split_once(' ')?;
                let level = LOG_LEVELS.iter().copied().find(|known| *known == level)?;
                let after = after.trim_start().strip_prefix('[')?;
                let (source, message) = after.split_once("]:")?;
                Some(LogLine {
                    level,
                    source,
                    message: message.trim_start(),
                })
            }
        }
    }

    /// The level of a log line, which is all most callers need.
    pub fn level(self, line: &str) -> Option<&'static str> {
        self.parse(line).map(|logged| logged.level)
    }
}

/// What a finished run produced, with the CLI's own log levels split out.
///
/// The driver already reports the exit status, cancellation and timeouts. This
/// adds the two things a store backend needs: the whole text (for a JSON
/// parser) and the lines the CLI logged at `ERROR`/`WARNING`, which is how a
/// run that exits `0` after refusing to do the work is caught.
#[derive(Debug, Clone)]
pub struct RunReport {
    /// The command's stable label (`"install"`, `"list-owned"`, …).
    pub command: &'static str,
    pub outcome: CliOutcome,
    /// Every event's text, one line each (ANSI already stripped by the driver).
    pub text: String,
    /// Lines logged at `ERROR`, `CRITICAL` or `FATAL`.
    pub errors: Vec<String>,
    /// Lines logged at `WARNING`.
    pub warnings: Vec<String>,
    /// The log shape this CLI writes.
    pub format: LogFormat,
}

impl RunReport {
    pub(crate) fn new(command: &'static str, outcome: CliOutcome, format: LogFormat) -> Self {
        let mut text = String::new();
        let mut errors = Vec::new();
        let mut warnings = Vec::new();
        for line in outcome.events.iter().filter_map(event_text) {
            match format.level(line) {
                Some("ERROR" | "CRITICAL" | "FATAL") => errors.push(line.to_string()),
                Some("WARNING") => warnings.push(line.to_string()),
                _ => {}
            }
            text.push_str(line);
            text.push('\n');
        }
        Self {
            command,
            outcome,
            text,
            errors,
            warnings,
            format,
        }
    }

    /// The run exited `0`, was not cancelled and logged no error.
    ///
    /// This is the verdict for the library and install/verify operations, which
    /// print nothing but the CLI's own log lines. It is *not* the verdict for
    /// `launch`: a game writes to the same PTY, so an engine that logs
    /// `[SomeEngine] ERROR: …` while exiting `0` must not fail the launch. Epic
    /// uses [`RunReport::exited_cleanly`] there; nile needs its own marker (see
    /// `amazon::launched`), because it also exits `0` when it never launched.
    pub fn succeeded(&self) -> bool {
        self.outcome.success && self.errors.is_empty()
    }

    /// The run exited `0` and was not cancelled or killed.
    pub fn exited_cleanly(&self) -> bool {
        self.outcome.success
    }

    /// Why the run did not succeed: the terminal event's message, else the last
    /// error line, else the exit code. `None` when it succeeded.
    pub fn failure_message(&self) -> Option<String> {
        if self.succeeded() {
            return None;
        }
        match self.outcome.events.last() {
            Some(ProgressEvent::Failed { message, .. }) => Some(message.clone()),
            _ => Some(
                self.errors
                    .last()
                    .cloned()
                    .unwrap_or_else(|| format!("exited with code {}", self.outcome.code)),
            ),
        }
    }

    /// The lines the CLI logged itself, with level and source.
    ///
    /// Backends use this for the things a verdict depends on beyond "was there
    /// an error" — nile's `LAUNCHER` line, for instance.
    pub fn log_lines(&self) -> impl Iterator<Item = LogLine<'_>> + '_ {
        let format = self.format;
        self.text.lines().filter_map(move |line| format.parse(line))
    }

    /// The per-run raw transcript, when logging was enabled.
    pub fn log_path(&self) -> Option<&std::path::Path> {
        self.outcome.log_path.as_deref()
    }

    /// Turns a run that did not succeed into a [`RunFailure`].
    ///
    /// Backends wrap it in their own error type (see `EpicError::Failed`), which
    /// keeps the command label and the CLI's own message.
    pub fn require_success(self) -> Result<Self, RunFailure> {
        match self.failure_message() {
            None => Ok(self),
            Some(message) => Err(RunFailure {
                command: self.command,
                message,
            }),
        }
    }
}

/// A store CLI run that did not succeed, for a backend's error type to wrap.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{command} failed: {message}")]
pub struct RunFailure {
    /// The command's stable label (`"install"`, `"list-owned"`, …).
    pub command: &'static str,
    /// The CLI's own explanation, or the terminal event's.
    pub message: String,
}

fn event_text(event: &ProgressEvent) -> Option<&str> {
    match event {
        ProgressEvent::Line(text)
        | ProgressEvent::Stage(text)
        | ProgressEvent::Percent(_, text) => Some(text.as_str()),
        ProgressEvent::Done | ProgressEvent::Failed { .. } => None,
    }
}

/// Pulls the JSON payload out of a PTY-merged run.
///
/// The normal case is that the stream is exactly the JSON document (both CLIs'
/// `--json` paths print nothing else), so that is tried first. Only when that
/// fails are log lines removed, and if the result is still not a document, the
/// span from the first `[`/`{` line to the last `]`/`}` line is tried. The last
/// step is what makes a run that started with a log line parseable.
pub(crate) fn extract_json(output: &str, format: LogFormat) -> Option<String> {
    if serde_json::from_str::<serde_json::Value>(output).is_ok() {
        return Some(output.to_string());
    }

    let lines: Vec<&str> = output
        .lines()
        .filter(|line| format.level(line).is_none())
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .collect();
    let joined = lines.join("\n");
    if serde_json::from_str::<serde_json::Value>(&joined).is_ok() {
        return Some(joined);
    }

    let start = lines.iter().position(|line| {
        let text = line.trim_start();
        text.starts_with('[') || text.starts_with('{')
    })?;
    let end = lines.iter().rposition(|line| {
        let text = line.trim_end();
        text.ends_with(']') || text.ends_with('}')
    })?;
    (start <= end).then(|| lines[start..=end].join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::CliOutcome;
    use std::path::PathBuf;

    fn outcome(success: bool, code: u32, events: Vec<ProgressEvent>) -> CliOutcome {
        CliOutcome {
            success,
            code,
            cancelled: false,
            timed_out: None,
            log_path: Some(PathBuf::from("/tmp/cli.log")),
            events,
            raw_tail: String::new(),
        }
    }

    fn lines(values: &[&str]) -> Vec<ProgressEvent> {
        let mut events: Vec<ProgressEvent> = values
            .iter()
            .map(|value| ProgressEvent::Line((*value).to_string()))
            .collect();
        events.push(ProgressEvent::Done);
        events
    }

    #[test]
    fn legendary_lines_split_into_level_source_and_message() {
        let logged = LogFormat::Legendary
            .parse("[cli] INFO: Logging in...")
            .unwrap();
        assert_eq!(logged.level, "INFO");
        assert_eq!(logged.source, "cli");
        assert_eq!(logged.message, "Logging in...");

        let logged = LogFormat::Legendary
            .parse("[DLManager] ERROR: chunk failed")
            .unwrap();
        assert_eq!(logged.level, "ERROR");
        assert_eq!(logged.source, "DLManager");

        assert!(
            LogFormat::Legendary
                .parse("INFO [CLI]:\t Launching")
                .is_none()
        );
        assert!(
            LogFormat::Legendary
                .parse("[cli] LOUD: not a level")
                .is_none()
        );
        assert!(LogFormat::Legendary.parse("[cli] INFO").is_none());
        assert!(LogFormat::Legendary.parse("plain game output").is_none());
    }

    #[test]
    fn nile_lines_split_into_level_source_and_message() {
        let logged = LogFormat::Nile
            .parse("INFO [LIBRARY]:\t Synchronizing library")
            .unwrap();
        assert_eq!(logged.level, "INFO");
        assert_eq!(logged.source, "LIBRARY");
        assert_eq!(logged.message, "Synchronizing library");

        let logged = LogFormat::Nile
            .parse("ERROR [DOWNLOAD]:\t Unable to load manifest")
            .unwrap();
        assert_eq!(logged.level, "ERROR");
        assert_eq!(logged.source, "DOWNLOAD");

        // A game's output can start with a word that is not a level.
        assert!(LogFormat::Nile.parse("Launching game").is_none());
        assert!(LogFormat::Nile.parse("[cli] INFO: Logging in...").is_none());
        assert!(LogFormat::Nile.parse("INFO no brackets").is_none());
    }

    #[test]
    fn a_nile_run_without_errors_but_a_refusal_is_not_a_success() {
        let report = RunReport::new(
            "install",
            outcome(
                true,
                0,
                lines(&[
                    "INFO [LIBRARY]:\t Synchronizing library",
                    "ERROR [CLI]:\t Couldn't find what you are looking for",
                    "WARNING [CLI]:\t Nothing was downloaded",
                ]),
            ),
            LogFormat::Nile,
        );

        assert!(report.outcome.success, "nile exits 0");
        assert!(!report.succeeded());
        assert_eq!(report.errors.len(), 1);
        assert_eq!(report.warnings.len(), 1);
        assert_eq!(
            report.failure_message().as_deref(),
            Some("ERROR [CLI]:\t Couldn't find what you are looking for")
        );
        assert_eq!(
            report.log_path(),
            Some(std::path::Path::new("/tmp/cli.log"))
        );
    }

    #[test]
    fn a_clean_run_has_no_failure_message() {
        let report = RunReport::new(
            "list-owned",
            outcome(true, 0, lines(&["INFO [CLI]:\t done"])),
            LogFormat::Nile,
        );
        assert!(report.succeeded());
        assert!(report.exited_cleanly());
        assert_eq!(report.failure_message(), None);
    }

    #[test]
    fn a_run_cut_short_reports_the_terminal_message() {
        let report = RunReport::new(
            "install",
            outcome(
                false,
                1,
                vec![
                    ProgressEvent::Line("INFO [CLI]:\t downloading".into()),
                    ProgressEvent::Failed {
                        code: 1,
                        message: "cancelled by user".into(),
                    },
                ],
            ),
            LogFormat::Nile,
        );

        assert!(!report.succeeded());
        assert!(!report.exited_cleanly());
        assert_eq!(
            report.failure_message().as_deref(),
            Some("cancelled by user")
        );
        assert_eq!(report.log_lines().count(), 1);
    }

    #[test]
    fn extract_json_ignores_the_formats_own_log_lines() {
        let nile = "INFO [LIBRARY]:\t Synchronizing library\n[{\"product\": {\"id\": \"a\"}}]\n";
        assert_eq!(
            extract_json(nile, LogFormat::Nile).as_deref(),
            Some("[{\"product\": {\"id\": \"a\"}}]")
        );

        let legendary = "[cli] INFO: Getting game list...\n[{\"app_name\": \"Gizmo\"}]\n";
        assert_eq!(
            extract_json(legendary, LogFormat::Legendary).as_deref(),
            Some("[{\"app_name\": \"Gizmo\"}]")
        );

        assert_eq!(extract_json("INFO [CLI]:\t nothing", LogFormat::Nile), None);
    }
}
