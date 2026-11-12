//! Epic Games Store backend, driven by the `legendary` CLI.
//!
//! Epic has no documented public library API a launcher may call, so v1 wraps
//! [`legendary`](https://github.com/legendary-gl/legendary) — the same choice the
//! plan makes for Amazon (`nile`) and GOG (`lgogdownloader`). This module owns
//! the Epic-specific half of that: the argv presets, the JSON shapes legendary
//! prints, and the one place where its exit code is not the answer.
//!
//! ```text
//!  Epic::new() ──spec()──► CliSpec ──► CliHandle (spawn / next_event / wait)
//!      │                                  │
//!      ├─ owned()      list --json        └─ streams Percent/Line events
//!      ├─ installed()  list-installed --json
//!      ├─ account()    status --offline --json
//!      └─ verify()     verify {id}  → classification, not exit code
//! ```
//!
//! Three details of the real CLI shape this module (see `docs/m2-epic.md`):
//!
//! - **A PTY merges stderr into stdout**, and legendary logs through a
//!   `StreamHandler` on stderr. `--json` output therefore arrives surrounded by
//!   `[cli] INFO: ...` lines; [`parse_owned`] / [`parse_installed`] filter those
//!   out instead of assuming the stream is pure JSON.
//! - **`verify` exits `0` even when files are corrupt or missing.** The verdict
//!   is the last summary line, so [`VerifyReport`] is parsed from the output and
//!   never from `CliOutcome::success`.
//! - **`install` exits `0` on some error paths** (a third-party-managed title,
//!   for example). [`RunReport::succeeded`] therefore also requires that the CLI
//!   logged nothing at `ERROR`/`CRITICAL`/`FATAL`.
//!
//! Everything here is headless: no Dioxus, no SQLite, no Steam. The GUI's store
//! status and library sync land on top of these functions.

use std::path::{Path, PathBuf};

use pitagames_core::paths;
use pitagames_core::{Game, GameId, OwnedGame, Store};
use regex::Regex;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::cli::{
    CliCommand, CliError, CliHandle, CliOptions, CliOutcome, CliSpec, ProgressEvent,
    ProgressParser, resolve_program,
};

/// The program this backend drives.
pub const PROGRAM: &str = "legendary";

/// The one percent pattern both of legendary's progress lines share.
///
/// Downloads print `[DLManager] INFO: = Progress: 12.34% (1234/10000), Running
/// for …`; a verify prints `Verification progress: 123/456 (12.3%) [1.2 MiB/s]`.
/// Matching only those prefixes is deliberate: the install summary also contains
/// numbers followed by `%` ("Compression savings: 12.3%"), and a generic percent
/// regex would move the bar backwards before the download even starts.
pub const PROGRESS_PATTERN: &str =
    r"(?:= Progress:[^(]*?|Verification progress:[^(]*\()([0-9]{1,3}(?:\.[0-9]+)?)%";

/// The level tags legendary's log formatter writes (`[name] LEVEL: message`).
const LOG_LEVELS: [&str; 6] = ["DEBUG", "INFO", "WARNING", "ERROR", "CRITICAL", "FATAL"];

fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

/// The `legendary` preset.
pub fn spec() -> CliSpec {
    spec_with_program(PROGRAM)
}

/// The preset with another program name, for a wrapper script or a test.
pub fn spec_with_program(program: impl Into<String>) -> CliSpec {
    let progress = Regex::new(PROGRESS_PATTERN).expect("the progress pattern is a literal");
    CliSpec {
        program: program.into(),
        needs_tty: true,
        progress: ProgressParser::Percent(progress),
        // `list` prints owned games (DLC nested under each game), `list-installed`
        // the local ones. Both are arrays of `vars()` of a dataclass, so unknown
        // fields are ignored by the parser.
        list_installed: args(&["list-installed", "--json"]),
        list_owned: args(&["list", "--json"]),
        // The embedded browser cannot work in a launcher, and a webview login
        // cannot be answered over the PTY. Forcing the code-paste flow keeps the
        // auth run answerable with `write_stdin`.
        auth: args(&["auth", "--disable-webview"]),
        logout: args(&["auth", "--delete"]),
        // legendary installs into `<base-path>/<game-folder>`; the two halves of
        // the requested directory keep PitaGames' path authoritative. `--yes`
        // answers the "do you wish to install" prompt, and `--disable-sdl`
        // avoids the interactive install-tag prompt (selective download would
        // otherwise need a human to choose language/DLC parts).
        install: vec![
            "install".into(),
            "{id}".into(),
            "--base-path".into(),
            "{parent}".into(),
            "--game-folder".into(),
            "{name}".into(),
            "--yes".into(),
            "--disable-sdl".into(),
        ],
        verify: args(&["verify", "{id}"]),
        // legendary launches through its own wine configuration, which is what
        // v1 hands to it until the umu/Proton prefix item of M2 lands.
        // `--skip-version-check` avoids both a network login and the hard
        // "game is out of date" refusal; an update is a separate action.
        launch: args(&["launch", "--skip-version-check", "{id}"]),
        // Offline so the store status question ("is this account logged in?")
        // never needs the network; it answers `<not logged in>` instead.
        status: args(&["status", "--offline", "--json"]),
        version: args(&["--version"]),
    }
}

/// Where a default Epic install goes: `games/epic/<app name>` under the data dir.
///
/// The name is sanitized because an app name comes from legendary's JSON (or a
/// command line) and becomes one path component: no separator, no control
/// character, and never `.`/`..`. `None` only when there is no data directory.
pub fn install_dir(app_name: &str) -> Option<PathBuf> {
    paths::games_dir().map(|dir| dir.join("epic").join(safe_component(app_name)))
}

fn safe_component(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|ch| match ch {
            '/' | '\\' => '-',
            ch if ch.is_control() => '-',
            ch => ch,
        })
        .collect();
    let trimmed = cleaned.trim();
    // With the separators gone, only the two traversal names can still escape.
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        "epic-game".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Whether the CLI is there, which is the first thing a store status row shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreStatus {
    /// The program resolved to an executable file.
    Ready { program: PathBuf },
    /// Nothing by that name is on `PATH`; installs are unavailable.
    CliMissing { program: String },
}

impl StoreStatus {
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
                .unwrap_or(PROGRAM),
            Self::CliMissing { program } => program,
        }
    }
}

/// The Epic backend.
#[derive(Debug, Clone)]
pub struct Epic {
    spec: CliSpec,
}

impl Default for Epic {
    fn default() -> Self {
        Self::new()
    }
}

impl Epic {
    pub fn new() -> Self {
        Self::with_program(PROGRAM)
    }

    pub fn with_program(program: impl Into<String>) -> Self {
        Self {
            spec: spec_with_program(program),
        }
    }

    pub fn spec(&self) -> &CliSpec {
        &self.spec
    }

    pub fn program(&self) -> &str {
        &self.spec.program
    }

    /// Resolves the program without running it.
    ///
    /// `path_override` is a `PATH` value (tests and the `--program` flag use it);
    /// `None` reads the inherited environment.
    pub fn status(&self, path_override: Option<&str>) -> StoreStatus {
        match resolve_program(&self.spec.program, path_override) {
            Ok(program) => StoreStatus::Ready { program },
            Err(_) => StoreStatus::CliMissing {
                program: self.spec.program.clone(),
            },
        }
    }

    /// Starts one operation on the PTY driver.
    pub fn spawn(&self, command: &CliCommand, options: &CliOptions) -> Result<CliHandle, CliError> {
        CliHandle::spawn(&self.spec, command, options)
    }

    /// Runs a command to completion and keeps its text, log levels and outcome.
    pub fn output(
        &self,
        command: &CliCommand,
        options: &CliOptions,
    ) -> Result<RunReport, CliError> {
        self.stream(command, options, |_| {})
    }

    /// Runs a command, handing every event to `on_event` as it arrives.
    ///
    /// This is what an install UI uses: the caller renders progress while the
    /// run is alive and gets the [`RunReport`] to classify the failure.
    pub fn stream(
        &self,
        command: &CliCommand,
        options: &CliOptions,
        mut on_event: impl FnMut(&ProgressEvent),
    ) -> Result<RunReport, CliError> {
        let handle = self.spawn(command, options)?;
        while let Some(event) = handle.next_event()? {
            on_event(&event);
        }
        Ok(RunReport::new(command.as_str(), handle.wait()?))
    }

    /// The account's owned games, DLC excluded.
    pub fn owned(&self, options: &CliOptions) -> Result<Vec<OwnedGame>, EpicError> {
        let report = self.output(&CliCommand::ListOwned, options)?;
        let report = report.require_success()?;
        parse_owned(&report.text)
    }

    /// The games legendary reports as installed, DLC excluded.
    pub fn installed(&self, options: &CliOptions) -> Result<Vec<Game>, EpicError> {
        let report = self.output(&CliCommand::ListInstalled, options)?;
        let report = report.require_success()?;
        parse_installed(&report.text)
    }

    /// The stored session, as `status --offline --json` reports it.
    pub fn account(&self, options: &CliOptions) -> Result<EpicAccount, EpicError> {
        let report = self.output(&CliCommand::Status, options)?;
        let report = report.require_success()?;
        parse_account(&report.text)
    }

    /// The CLI's own version, for the "outdated" warning.
    pub fn version(&self, options: &CliOptions) -> Result<Option<String>, EpicError> {
        let report = self.output(&CliCommand::Version, options)?;
        let report = report.require_success()?;
        Ok(parse_version(&report.text))
    }
}

/// What a finished run produced, with the CLI's own log levels split out.
///
/// The driver already reports the exit status, cancellation and timeouts. This
/// adds the two things a store backend needs: the whole text (for a JSON parser)
/// and the lines the CLI logged at `ERROR`/`WARNING`, which is how a legendary
/// run that exits `0` after refusing to install is caught.
#[derive(Debug, Clone)]
pub struct RunReport {
    pub command: &'static str,
    pub outcome: CliOutcome,
    /// Every event's text, one line each (ANSI already stripped by the driver).
    pub text: String,
    /// Lines logged at `ERROR`, `CRITICAL` or `FATAL`.
    pub errors: Vec<String>,
    /// Lines logged at `WARNING`.
    pub warnings: Vec<String>,
}

impl RunReport {
    fn new(command: &'static str, outcome: CliOutcome) -> Self {
        let mut text = String::new();
        let mut errors = Vec::new();
        let mut warnings = Vec::new();
        for event in &outcome.events {
            let Some(line) = event_text(event) else {
                continue;
            };
            match log_level(line) {
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
        }
    }

    /// The run exited `0`, was not cancelled and logged no error.
    ///
    /// This is the verdict for the library and install/verify operations, which
    /// print nothing but legendary's own log lines. It is *not* the verdict for
    /// `launch`: a game writes to the same PTY, so an engine that logs
    /// `[SomeEngine] ERROR: …` while exiting `0` must not fail the launch. That
    /// path uses [`RunReport::exited_cleanly`].
    pub fn succeeded(&self) -> bool {
        self.outcome.success && self.errors.is_empty()
    }

    /// The run exited `0` and was not cancelled or killed.
    pub fn exited_cleanly(&self) -> bool {
        self.outcome.success
    }

    /// Why the run did not succeed: the terminal event's message, or the last
    /// error line when the CLI exited `0` anyway.
    pub fn failure_message(&self) -> Option<String> {
        match self.outcome.events.last() {
            Some(ProgressEvent::Failed { message, .. }) => Some(message.clone()),
            _ => self.errors.last().cloned(),
        }
    }

    fn require_success(self) -> Result<Self, EpicError> {
        if self.succeeded() {
            return Ok(self);
        }
        Err(EpicError::Failed {
            command: self.command,
            message: self
                .failure_message()
                .unwrap_or_else(|| format!("exited with code {}", self.outcome.code)),
        })
    }

    /// The per-run raw transcript, when logging was enabled.
    pub fn log_path(&self) -> Option<&Path> {
        self.outcome.log_path.as_deref()
    }
}

/// A verify's verdict.
///
/// legendary always exits `0` here, so this is parsed from the summary line:
/// `Verification finished successfully.` or `Verification failed, 3 file(s)
/// corrupted, 1 file(s) are missing.` A run with neither line is not verified.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VerifyReport {
    pub verified: bool,
    pub corrupted: usize,
    pub missing: usize,
}

/// The account summary `legendary status --offline --json` prints.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct EpicAccount {
    #[serde(default)]
    pub account: String,
    #[serde(default)]
    pub games_available: usize,
    #[serde(default)]
    pub games_installed: usize,
    #[serde(default)]
    pub config_directory: String,
}

impl EpicAccount {
    /// legendary prints `<not logged in>` when there is no stored session.
    pub fn logged_in(&self) -> bool {
        !self.account.trim().is_empty() && self.account != "<not logged in>"
    }
}

/// Failures of the Epic backend that are not a bare [`CliError`].
#[derive(Debug, thiserror::Error)]
pub enum EpicError {
    #[error("legendary printed no JSON to parse")]
    NoJson,

    #[error("legendary's JSON could not be read: {source}")]
    Json {
        #[from]
        source: serde_json::Error,
    },

    #[error("legendary {command} failed: {message}")]
    Failed {
        command: &'static str,
        message: String,
    },

    #[error(transparent)]
    Cli(#[from] CliError),
}

/// Parses `legendary list --json` (owned games).
pub fn parse_owned(output: &str) -> Result<Vec<OwnedGame>, EpicError> {
    let entries: Vec<OwnedEntry> = decode(output)?;
    Ok(entries
        .into_iter()
        .filter(|entry| !entry.is_dlc())
        .map(|entry| {
            let title = if entry.app_title.trim().is_empty() {
                entry.app_name.clone()
            } else {
                entry.app_title
            };
            OwnedGame::new(GameId::new(Store::Epic, entry.app_name), title)
        })
        .collect())
}

/// Parses `legendary list-installed --json`.
pub fn parse_installed(output: &str) -> Result<Vec<Game>, EpicError> {
    let entries: Vec<InstalledEntry> = decode(output)?;
    Ok(entries
        .into_iter()
        .filter(|entry| !entry.is_dlc)
        .map(InstalledEntry::into_game)
        .collect())
}

/// Parses `legendary status --offline --json`.
pub fn parse_account(output: &str) -> Result<EpicAccount, EpicError> {
    let json = extract_json(output).ok_or(EpicError::NoJson)?;
    Ok(serde_json::from_str(&json)?)
}

/// Parses `legendary --version`
/// (`legendary version "0.20.35", codename "A D…"`).
pub fn parse_version(output: &str) -> Option<String> {
    let line = output.lines().find(|line| line.contains("version \""))?;
    let start = line.find("version \"")? + "version \"".len();
    let end = line[start..].find('"')? + start;
    (!line[start..end].is_empty()).then(|| line[start..end].to_string())
}

/// Classifies a verify run from its summary line.
pub fn verify_report(text: &str) -> VerifyReport {
    if let Some(captures) = verify_failure().captures(text) {
        return VerifyReport {
            verified: false,
            corrupted: captures[1].parse().unwrap_or(0),
            missing: captures[2].parse().unwrap_or(0),
        };
    }
    VerifyReport {
        verified: text.contains("Verification finished successfully."),
        corrupted: 0,
        missing: 0,
    }
}

fn verify_failure() -> &'static Regex {
    static PATTERN: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(
            r"Verification failed, ([0-9]+) file\(s\) corrupted, ([0-9]+) file\(s\) are missing",
        )
        .expect("the verify summary pattern is a literal")
    });
    &PATTERN
}

#[derive(Debug, Deserialize)]
struct OwnedEntry {
    app_name: String,
    #[serde(default)]
    app_title: String,
    #[serde(default)]
    metadata: serde_json::Value,
}

impl OwnedEntry {
    /// legendary marks DLC with `metadata.mainGameItem`; the same entries are
    /// also nested in the base game's `dlcs`, so filtering on this is enough.
    fn is_dlc(&self) -> bool {
        self.metadata.get("mainGameItem").is_some()
    }
}

#[derive(Debug, Deserialize)]
struct InstalledEntry {
    app_name: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    install_path: String,
    #[serde(default)]
    install_size: u64,
    #[serde(default)]
    version: String,
    #[serde(default)]
    executable: String,
    #[serde(default)]
    is_dlc: bool,
    #[serde(default)]
    platform: String,
    #[serde(default)]
    install_tags: Vec<String>,
    #[serde(default)]
    needs_verification: bool,
}

impl InstalledEntry {
    fn into_game(self) -> Game {
        let title = if self.title.trim().is_empty() {
            self.app_name.clone()
        } else {
            self.title
        };
        let mut game = Game::new(GameId::new(Store::Epic, self.app_name), title);
        game.installed = true;

        if !self.install_path.is_empty() {
            let dir = PathBuf::from(&self.install_path);
            if !self.executable.is_empty() {
                // legendary reports the executable relative to the install dir,
                // sometimes with a leading separator.
                game.exe = Some(dir.join(self.executable.trim_start_matches(['/', '\\'])));
            }
            game.install_dir = Some(dir);
        }
        game.size_bytes = (self.install_size > 0).then_some(self.install_size);
        game.version = (!self.version.is_empty()).then_some(self.version);
        game.tags = self.install_tags;

        // `Game` has no platform field, and Windows-vs-native matters to Proton
        // and to the "won't run under Proton" warning. The tag keeps that
        // information without a schema change.
        if !self.platform.is_empty() {
            game.tags
                .push(format!("platform:{}", self.platform.to_ascii_lowercase()));
        }
        if self.needs_verification {
            game.tags.push("needs-verification".into());
        }
        game
    }
}

fn decode<T: DeserializeOwned>(output: &str) -> Result<Vec<T>, EpicError> {
    let json = extract_json(output).ok_or(EpicError::NoJson)?;
    Ok(serde_json::from_str(&json)?)
}

/// Pulls the JSON payload out of a PTY-merged run.
///
/// The normal case is that the stream is exactly the JSON document (legendary's
/// `--json` path prints nothing else), so that is tried first. Only when that
/// fails are log lines removed, and if the result is still not a document, the
/// span from the first `[`/`{` line to the last `]`/`}` line is tried. The last
/// step is what makes a run that started with `[cli] INFO: …` parseable.
fn extract_json(output: &str) -> Option<String> {
    if serde_json::from_str::<serde_json::Value>(output).is_ok() {
        return Some(output.to_string());
    }

    let lines: Vec<&str> = output
        .lines()
        .filter(|line| log_level(line).is_none())
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

fn event_text(event: &ProgressEvent) -> Option<&str> {
    match event {
        ProgressEvent::Line(text)
        | ProgressEvent::Stage(text)
        | ProgressEvent::Percent(_, text) => Some(text.as_str()),
        ProgressEvent::Done | ProgressEvent::Failed { .. } => None,
    }
}

/// The level of a legendary log line (`[cli] ERROR: message`), if it is one.
fn log_level(line: &str) -> Option<&'static str> {
    let rest = line.trim_start().strip_prefix('[')?;
    let (_, after) = rest.split_once("] ")?;
    LOG_LEVELS
        .into_iter()
        .find(|level| after.starts_with(level) && after[level.len()..].starts_with(':'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::Mutex;

    use crate::cli::{CliCommand, ProgressParser};

    const LIST: &str = include_str!("../../../fixtures/legendary/list.json");
    const LIST_INSTALLED: &str = include_str!("../../../fixtures/legendary/list-installed.json");
    const STATUS: &str = include_str!("../../../fixtures/legendary/status.json");

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn percentages(line: &str) -> Option<f32> {
        let regex = Regex::new(PROGRESS_PATTERN).unwrap();
        let captures = regex.captures(line)?;
        captures.get(1)?.as_str().parse().ok()
    }

    #[test]
    fn the_preset_matches_the_documented_legendary_invocations() {
        let spec = spec();
        assert_eq!(spec.program, "legendary");
        assert!(matches!(spec.progress, ProgressParser::Percent(_)));
        assert_eq!(
            spec.argv(&CliCommand::ListOwned).unwrap(),
            ["list", "--json"]
        );
        assert_eq!(
            spec.argv(&CliCommand::ListInstalled).unwrap(),
            ["list-installed", "--json"]
        );
        assert_eq!(
            spec.argv(&CliCommand::Auth).unwrap(),
            ["auth", "--disable-webview"]
        );
        assert_eq!(
            spec.argv(&CliCommand::Logout).unwrap(),
            ["auth", "--delete"]
        );
        assert_eq!(
            spec.argv(&CliCommand::Status).unwrap(),
            ["status", "--offline", "--json"]
        );
        assert_eq!(spec.argv(&CliCommand::Version).unwrap(), ["--version"]);
        assert_eq!(
            spec.argv(&CliCommand::Verify { id: "Gizmo".into() })
                .unwrap(),
            ["verify", "Gizmo"]
        );
        assert_eq!(
            spec.argv(&CliCommand::Launch { id: "Gizmo".into() })
                .unwrap(),
            ["launch", "--skip-version-check", "Gizmo"]
        );

        let install = spec
            .argv(&CliCommand::Install {
                id: "Gizmo".into(),
                install_dir: PathBuf::from("/games/Epic/Gizmo"),
            })
            .unwrap();
        assert_eq!(
            install,
            [
                "install",
                "Gizmo",
                "--base-path",
                "/games/Epic",
                "--game-folder",
                "Gizmo",
                "--yes",
                "--disable-sdl"
            ]
        );
    }

    #[test]
    fn only_legendarys_progress_lines_count_as_percentages() {
        assert_eq!(
            percentages("[DLManager] INFO: = Progress: 12.34% (1234/10000), Running for 00:01:23"),
            Some(12.34)
        );
        assert_eq!(
            percentages("Verification progress: 123/456 (12.3%) [1.2 MiB/s]"),
            Some(12.3)
        );
        assert_eq!(
            percentages("Verification progress: 1/2 (100.0%) [0.0 MiB/s]"),
            Some(100.0)
        );
        // The install summary also has a percentage; it is not progress.
        assert_eq!(
            percentages("[cli] INFO: Download size: 1234.50 MiB (Compression savings: 37.5%)"),
            None
        );
        assert_eq!(percentages("[cli] INFO: Install size: 2048.00 MiB"), None);
    }

    #[test]
    fn owned_games_parse_around_log_noise_and_skip_dlc() {
        let output = format!(
            "[cli] INFO: Logging in...\n[cli] INFO: Getting game list... (this may take a while)\n{LIST}"
        );
        let games = parse_owned(&output).unwrap();
        let titles: Vec<&str> = games.iter().map(|game| game.title.as_str()).collect();
        assert_eq!(titles, ["Gizmo", "Sugar"]);
        assert!(
            games
                .iter()
                .all(|game| game.id.store == Store::Epic && !game.title.is_empty())
        );
        assert_eq!(games[0].id.store_id, "Gizmo");
        // DLC has its own `metadata.mainGameItem`; it must not become a library row.
        assert!(!games.iter().any(|game| game.id.store_id == "GizmoDeluxe"));
        // ...

        // A compact single-line document parses the same way.
        let compact = LIST.lines().collect::<String>();
        assert_eq!(parse_owned(&compact).unwrap(), games);
    }

    #[test]
    fn installed_games_carry_paths_size_and_platform() {
        let games = parse_installed(LIST_INSTALLED).unwrap();
        assert_eq!(games.len(), 1, "the DLC entry is not a library row");
        let game = &games[0];

        assert_eq!(game.id, GameId::new(Store::Epic, "Gizmo"));
        assert_eq!(game.title, "Gizmo");
        assert!(game.installed);
        assert_eq!(
            game.install_dir.as_deref(),
            Some(Path::new("/home/player/Games/Gizmo"))
        );
        assert_eq!(
            game.exe.as_deref(),
            Some(Path::new(
                "/home/player/Games/Gizmo/Gizmo/Binaries/Win64/Gizmo-Win64-Shipping.exe"
            ))
        );
        assert_eq!(game.size_bytes, Some(12_345_678_901));
        assert_eq!(game.version.as_deref(), Some("0.9.1"));
        assert_eq!(game.tags, ["Windows", "platform:windows"]);
        assert_eq!(game.anticheat, pitagames_core::AntiCheat::Unknown);
    }

    #[test]
    fn a_missing_payload_and_malformed_json_are_distinct_errors() {
        let error = parse_owned("[cli] ERROR: Login failed!\n").unwrap_err();
        assert!(matches!(error, EpicError::NoJson), "{error}");
        assert!(error.to_string().contains("no JSON"));

        let error = parse_owned("[{\"app_name\": nope}]").unwrap_err();
        assert!(matches!(error, EpicError::Json { .. }), "{error}");
    }

    #[test]
    fn verify_reports_corrupt_and_missing_files_instead_of_the_exit_code() {
        assert_eq!(
            verify_report("[cli] INFO: Verification finished successfully.\n"),
            VerifyReport {
                verified: true,
                corrupted: 0,
                missing: 0
            }
        );
        assert_eq!(
            verify_report(
                "[cli] ERROR: File does not match hash: \"a.bin\"\n\
                 [cli] ERROR: Verification failed, 2 file(s) corrupted, 5 file(s) are missing.\n"
            ),
            VerifyReport {
                verified: false,
                corrupted: 2,
                missing: 5
            }
        );
        // A run that never reached the summary is not a pass.
        assert_eq!(
            verify_report("[cli] ERROR: Manifest appears to be missing!"),
            VerifyReport::default()
        );
    }

    #[test]
    fn account_and_version_are_read_from_their_own_text() {
        assert_eq!(parse_account(STATUS).unwrap().account, "player");
        assert_eq!(
            parse_account(STATUS).unwrap().config_directory,
            "/home/player/.config/legendary"
        );
        assert!(parse_account(STATUS).unwrap().logged_in());

        let signed_out = r#"{"account": "<not logged in>", "games_available": 0}"#;
        assert!(!parse_account(signed_out).unwrap().logged_in());

        assert_eq!(
            parse_version("legendary version \"0.20.35\", codename \"Pancakes\"\n").as_deref(),
            Some("0.20.35")
        );
        assert_eq!(parse_version("[cli] INFO: nothing here"), None);
    }

    #[test]
    fn the_default_install_dir_is_one_safe_component() {
        let guard = ENV_LOCK.lock().unwrap();
        // SAFETY: only the tests under this lock touch XDG_DATA_HOME.
        unsafe { std::env::set_var("XDG_DATA_HOME", "/tmp/xdg-epic") };
        let plain = install_dir("Gizmo");
        let hostile = install_dir("../../etc/passwd");
        let dots = install_dir("..");
        unsafe { std::env::remove_var("XDG_DATA_HOME") };
        drop(guard);

        assert_eq!(
            plain,
            Some(PathBuf::from("/tmp/xdg-epic/pitagames/games/epic/Gizmo"))
        );
        assert_eq!(
            hostile.as_deref(),
            Some(Path::new(
                "/tmp/xdg-epic/pitagames/games/epic/..-..-etc-passwd"
            ))
        );
        assert_eq!(
            dots.as_deref(),
            Some(Path::new("/tmp/xdg-epic/pitagames/games/epic/epic-game"))
        );
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

    #[test]
    fn a_missing_cli_and_a_present_one_are_both_reported() {
        let dir = tempfile::tempdir().unwrap();
        let epic = Epic::new();
        let missing = epic.status(Some(dir.path().to_str().unwrap()));
        assert!(!missing.is_ready());
        assert_eq!(missing.program(), "legendary");
        assert_eq!(missing.path(), None);

        script(dir.path(), "legendary", "#!/bin/sh\nexit 0\n");
        let ready = epic.status(Some(dir.path().to_str().unwrap()));
        assert!(ready.is_ready());
        assert_eq!(ready.path(), Some(dir.path().join("legendary").as_path()));
    }

    #[test]
    fn an_exit_zero_run_that_logged_an_error_is_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        script(
            dir.path(),
            "legendary",
            "#!/bin/sh\n\
             echo '[cli] INFO: Preparing downloads'\n\
             echo '[DLManager] WARNING: Retrying chunk'\n\
             echo '[cli] ERROR: The selected title has to be installed via a third-party store'\n\
             exit 0\n",
        );
        let epic = Epic::with_program("legendary");
        let report = epic
            .output(
                &CliCommand::Install {
                    id: "Gizmo".into(),
                    install_dir: dir.path().join("Gizmo"),
                },
                &options(dir.path()),
            )
            .unwrap();

        assert!(report.outcome.success, "the fake CLI exited 0");
        assert!(!report.succeeded());
        assert_eq!(report.warnings.len(), 1);
        assert_eq!(report.errors.len(), 1);
        assert_eq!(
            report.failure_message().as_deref(),
            Some("[cli] ERROR: The selected title has to be installed via a third-party store")
        );
        assert!(report.log_path().unwrap().exists());

        let error = epic.owned(&options(dir.path())).unwrap_err();
        assert!(
            matches!(
                error,
                EpicError::Failed {
                    command: "list-owned",
                    ..
                }
            ),
            "{error}"
        );
    }

    #[test]
    fn a_launch_is_judged_by_its_exit_code_alone() {
        let dir = tempfile::tempdir().unwrap();
        script(
            dir.path(),
            "legendary",
            "#!/bin/sh\n\
             echo '[cli] INFO: Launching \"Gizmo\"'\n\
             echo '[Gizmo] ERROR: shader cache lookup failed'\n\
             exit 0\n",
        );
        let epic = Epic::with_program("legendary");
        let report = epic
            .output(
                &CliCommand::Launch { id: "Gizmo".into() },
                &options(dir.path()),
            )
            .unwrap();

        // The game writes to the same PTY; a logged error is not a failed launch.
        assert_eq!(report.errors.len(), 1);
        assert!(!report.succeeded());
        assert!(report.exited_cleanly());
    }

    #[test]
    fn a_clean_run_parses_its_json_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        script(
            dir.path(),
            "legendary",
            &format!(
                "#!/bin/sh\n\
                 echo '[cli] INFO: Logging in...'\n\
                 cat <<'JSON'\n{LIST}\nJSON\n"
            ),
        );
        let epic = Epic::with_program("legendary");
        let games = epic.owned(&options(dir.path())).unwrap();
        assert_eq!(games.len(), 2);
    }
}
