//! Amazon Games store backend, driven by the `nile` CLI.
//!
//! Amazon has no public library API a third-party launcher may call — the
//! official client talks to endpoints it does not document — so v1 wraps
//! [`nile`](https://github.com/imLinguin/nile), the same way Epic wraps
//! `legendary`. This module owns the Amazon-specific half: the argv presets, the
//! JSON and text shapes nile prints, its install record file, and the one place
//! where its exit code is actively wrong.
//!
//! ```text
//!  Amazon::new() ──spec()──► CliSpec ──► CliHandle (spawn / next_event / wait)
//!      │                                   │
//!      ├─ owned()      library list --json  └─ streams Percent/Line events
//!      ├─ installed()  library list --installed  (+ <config>/nile/installed.json)
//!      ├─ sync()       library sync
//!      ├─ account()    auth --status
//!      └─ verify()     verify {id}  → repair, classified, not the exit code
//! ```
//!
//! Four details of the real CLI shape this module is built around (see
//! `docs/m2-amazon.md`):
//!
//! - **nile exits `0` for almost every failure.** Every handler logs
//!   `ERROR [LOGGER]: …` and returns, and `main()` returns `0` regardless, so
//!   the exit status cannot be the verdict for anything. [`RunReport`] adds the
//!   CLI's own log levels, exactly like legendary's (different format, see
//!   [`crate::report::LogFormat`]).
//! - **`launch` exits `0` even when it never launched** ("Game is not
//!   installed", a missing `fuel.json`). [`launched`] therefore requires the
//!   `INFO [LAUNCHER]: Launching` marker nile logs immediately before the spawn.
//! - **`verify` is a repair, not a checksum pass.** nile compares the local
//!   manifest and re-downloads what is missing; it prints no verdict line, so
//!   [`VerifyReport`] carries only "completed without an error".
//! - **Auth is a browser login plus a pasted redirect URL**, not a TOTP prompt:
//!   Amazon's 2FA happens in the browser. That URL is the long line the CLI
//!   relays to nile's `input()` prompt.
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
    CliCommand, CliError, CliHandle, CliOptions, CliOutcome, CliSpec, ProgressEvent, ProgressParser,
};
use crate::report::{LogFormat, RunFailure, RunReport, extract_json};

pub use crate::cli::StoreStatus;

/// The program this backend drives.
pub const PROGRAM: &str = "nile";

/// nile's progress bar line, one per second while downloading.
///
/// The bar prints a bare number, with **no percent sign**:
/// `INFO [PROGRESS]:\t = Progress: 12.34 1234/10000, Running for: 00:01:23`.
/// The `= Progress:` prefix is what keeps the adjacent
/// `= Downloaded: 12.34 MiB` / `= Written:` lines from being read as progress,
/// and the trailing whitespace requirement keeps `= Progress: 100` (no
/// fraction) working while `1234.50 MiB` never matches.
pub const PROGRESS_PATTERN: &str = r"=\s*Progress:\s*([0-9]{1,3}(?:\.[0-9]+)?)\s";

/// The logger nile spawns the game from.
pub const LAUNCH_SOURCE: &str = "LAUNCHER";
/// The message that logger writes immediately before the spawn.
pub const LAUNCH_MARKER: &str = "Launching";
/// nile's own install record, inside its config directory.
pub const INSTALLED_FILE: &str = "installed.json";

fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

/// The `nile` preset.
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
        // `library list --json` dumps nile's cached library (the entitlements
        // sync writes); it is an array of `{"id", "product": {"id", "title", …}}`
        // entries. `--installed` is the only listing that knows what is on disk,
        // and only in the text form (below).
        list_owned: args(&["library", "list", "--json"]),
        list_installed: args(&["library", "list", "--installed"]),
        // nile reads a cached library; this is the refresh that fills it (and
        // `auth --login` runs it once automatically).
        sync: args(&["library", "sync"]),
        // Interactive, because the login is: nile prints a URL, waits for
        // ENTER, opens the browser, and asks for the redirected URL. There is
        // no webview or TOTP prompt of our own — Amazon's 2FA is in the browser.
        auth: args(&["auth", "--login"]),
        logout: args(&["auth", "--logout"]),
        // `--path` is nile's *exact* install location, unlike `--base-path`,
        // which appends nile's own sanitized title. That keeps PitaGames'
        // directory authoritative, so an install cannot land somewhere else.
        // nile has no confirmation prompt and no `--yes`, and its own worker
        // pool does the download.
        install: vec![
            "install".into(),
            "{id}".into(),
            "--path".into(),
            "{dir}".into(),
        ],
        // An alias of `install` in nile: it reloads the local manifest and
        // re-downloads missing or corrupt files (a repair, not a read-only
        // check).
        verify: args(&["verify", "{id}"]),
        // nile picks wine (or Bottles) itself; the umu/Proton handoff, the shim
        // profile and the wrapper chain are the later M2 prefix item, and they
        // replace this plain call.
        launch: args(&["launch", "{id}"]),
        // Always JSON, and offline-capable: nile only reads its stored session.
        status: args(&["auth", "--status"]),
        version: args(&["--version"]),
    }
}

/// Where a default Amazon install goes: `games/amazon/<game name>`.
///
/// The name is sanitized for the same reason as Epic's: it comes from nile's
/// JSON or a command line and becomes one path component. `None` only when
/// there is no data directory.
pub fn install_dir(game_name: &str) -> Option<PathBuf> {
    paths::games_dir().map(|dir| {
        dir.join("amazon")
            .join(paths::safe_component(game_name, "amazon-game"))
    })
}

/// nile's own config directory (`$XDG_CONFIG_HOME/nile`).
///
/// `NILE_CONFIG_PATH` is honored the way nile reads it: it is a *base* directory
/// and nile appends `nile` to it. Overriding it is also how Heroic keeps its own
/// copy, so a user's data can sit somewhere else than the default.
pub fn config_dir() -> Option<PathBuf> {
    if let Some(base) = std::env::var_os("NILE_CONFIG_PATH").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(base).join("nile"));
    }
    paths::config_home().map(|base| base.join("nile"))
}

/// One installed game as nile records it in its own state file.
///
/// This is the only place the install path, size and version live: the CLI's
/// installed listing prints titles and ids only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledState {
    /// The product id, which is nile's game id everywhere else.
    pub id: String,
    /// Where the files are.
    pub path: PathBuf,
    /// The installed version id.
    pub version: Option<String>,
    /// Installed size in bytes.
    pub size_bytes: Option<u64>,
}

/// Reads `<config>/nile/installed.json`.
///
/// A missing file means "nothing is installed yet" and is not an error; a
/// malformed one is, because the caller is asking a question nile would also
/// fail to answer. It is only used to enrich the CLI's listing, so a broken
/// cache costs the paths, not the library.
pub fn installed_state(dir: Option<&Path>) -> Result<Vec<InstalledState>, AmazonError> {
    let Some(dir) = dir else {
        return Ok(Vec::new());
    };
    let path = dir.join(INSTALLED_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(AmazonError::State { path, source }),
    };
    let entries: Vec<StateEntry> =
        serde_json::from_str(&text).map_err(|source| AmazonError::StateJson { path, source })?;
    Ok(entries
        .into_iter()
        .filter(|entry| !entry.id.trim().is_empty())
        .map(StateEntry::into_state)
        .collect())
}

#[derive(Debug, Deserialize)]
struct StateEntry {
    #[serde(default)]
    id: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    size: u64,
}

impl StateEntry {
    fn into_state(self) -> InstalledState {
        InstalledState {
            id: self.id.trim().to_string(),
            path: PathBuf::from(self.path),
            version: (!self.version.trim().is_empty()).then_some(self.version),
            size_bytes: (self.size > 0).then_some(self.size),
        }
    }
}

/// The Amazon backend.
#[derive(Debug, Clone)]
pub struct Amazon {
    spec: CliSpec,
}

impl Default for Amazon {
    fn default() -> Self {
        Self::new()
    }
}

impl Amazon {
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
        StoreStatus::probe(&self.spec.program, path_override)
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
    /// This is what an install or verify UI uses: the caller renders progress
    /// while the run is alive and gets the [`RunReport`] to classify the result.
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
        Ok(RunReport::new(
            command.as_str(),
            handle.wait()?,
            LogFormat::Nile,
        ))
    }

    /// The account's owned games, from nile's cached library.
    pub fn owned(&self, options: &CliOptions) -> Result<Vec<OwnedGame>, AmazonError> {
        let report = self.output(&CliCommand::ListOwned, options)?;
        let report = report.require_success()?;
        parse_owned(&report.text)
    }

    /// The games nile says are installed, with the recorded path, size and
    /// version when its state file is readable.
    ///
    /// The CLI's `library list --installed` is the authority for *which* games
    /// are installed (it is what nile's own launch checks against); the state
    /// file only fills in the details. A malformed state file therefore costs
    /// the paths and never the rows.
    pub fn installed(&self, options: &CliOptions) -> Result<Vec<Game>, AmazonError> {
        let report = self.output(&CliCommand::ListInstalled, options)?;
        let report = report.require_success()?;
        let state = installed_state(config_dir().as_deref()).unwrap_or_default();
        Ok(installed_games(&report.text, &state))
    }

    /// Refreshes the cached library from Amazon (`library sync`).
    ///
    /// [`Amazon::owned`] reads the cache, so this is the "check the store for
    /// new Prime games" call. It needs a stored session, and nile logs an error
    /// when it has none.
    pub fn sync(&self, options: &CliOptions) -> Result<RunReport, AmazonError> {
        let report = self.output(&CliCommand::Sync, options)?;
        Ok(report.require_success()?)
    }

    /// The stored session, as `auth --status` reports it.
    pub fn account(&self, options: &CliOptions) -> Result<AmazonAccount, AmazonError> {
        let report = self.output(&CliCommand::Status, options)?;
        let report = report.require_success()?;
        parse_account(&report.text)
    }

    /// The CLI's own version, for the "outdated" warning.
    pub fn version(&self, options: &CliOptions) -> Result<Option<String>, AmazonError> {
        let report = self.output(&CliCommand::Version, options)?;
        let report = report.require_success()?;
        Ok(parse_version(&report.text))
    }

    /// Wraps a finished run the caller drove itself in the backend's report.
    ///
    /// Interactive flows (auth) need `spawn` + `write_stdin` + `wait`, so they
    /// cannot go through [`Amazon::stream`]; this gives them the same log-level
    /// verdict, which is how nile's exit-`0` refusals are still caught.
    pub fn report(&self, command: &CliCommand, outcome: CliOutcome) -> RunReport {
        RunReport::new(command.as_str(), outcome, LogFormat::Nile)
    }
}

/// nile's session summary (`{"Username": …, "LoggedIn": true}`).
///
/// The field names are capitalized on the wire because nile prints that dict
/// verbatim; they are snake_case here like every other domain type.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct AmazonAccount {
    #[serde(rename = "Username", default)]
    pub username: String,
    #[serde(rename = "LoggedIn", default)]
    pub logged_in: bool,
}

impl AmazonAccount {
    /// Whether a session is stored. nile prints `<not logged in>` otherwise.
    pub fn signed_in(&self) -> bool {
        self.logged_in
    }
}

/// A verify's verdict.
///
/// nile's `verify` is a repair: it reloads the local manifest and re-downloads
/// missing files, then logs "Download complete" — even after an early refusal.
/// There is no counts line and no checksum summary, so this carries the two
/// things that are real: whether the run finished without an error, and the
/// error lines when it did not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerifyReport {
    pub verified: bool,
    pub errors: Vec<String>,
}

/// Classifies a verify run.
pub fn verify_report(report: &RunReport) -> VerifyReport {
    VerifyReport {
        verified: report.succeeded(),
        errors: report.errors.clone(),
    }
}

/// Whether nile actually spawned a game.
///
/// `nile launch` exits `0` when it refuses ("Game is not installed", "No game
/// match", a missing `fuel.json`), so the exit code alone would report a failed
/// launch as a success. nile logs `INFO [LAUNCHER]: Launching` immediately
/// before the spawn and nothing of its own after; everything past that line is
/// the game's output. The marker plus a clean exit is the verdict, and the
/// error nile logged before the marker is the failure message.
pub fn launched(report: &RunReport) -> bool {
    report.exited_cleanly()
        && report
            .log_lines()
            .any(|line| line.source == LAUNCH_SOURCE && line.message.starts_with(LAUNCH_MARKER))
}

/// Failures of the Amazon backend that are not a bare [`CliError`].
#[derive(Debug, thiserror::Error)]
pub enum AmazonError {
    #[error("nile printed no JSON to parse")]
    NoJson,

    #[error("nile's JSON could not be read: {source}")]
    Json {
        #[from]
        source: serde_json::Error,
    },

    #[error("nile {command} failed: {message}")]
    Failed {
        command: &'static str,
        message: String,
    },

    #[error("failed to read nile's install state {path}: {source}")]
    State {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("nile's install state {path} is not valid JSON: {source}")]
    StateJson {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error(transparent)]
    Cli(#[from] CliError),
}

impl From<RunFailure> for AmazonError {
    fn from(failure: RunFailure) -> Self {
        Self::Failed {
            command: failure.command,
            message: failure.message,
        }
    }
}

/// Parses `nile library list --json` (the cached entitlements).
pub fn parse_owned(output: &str) -> Result<Vec<OwnedGame>, AmazonError> {
    let entries: Vec<LibraryEntry> = decode(output)?;
    Ok(entries
        .into_iter()
        .filter_map(LibraryEntry::into_owned)
        .collect())
}

/// One entry of nile's cached library.
#[derive(Debug, Deserialize)]
struct LibraryEntry {
    /// Games are addressed by `product.id` everywhere (install, verify, launch);
    /// the entitlement `id` next to it is not a [`GameId`] and is ignored.
    #[serde(default)]
    product: Option<Product>,
}

#[derive(Debug, Deserialize)]
struct Product {
    #[serde(default)]
    id: String,
    /// `null` for some older titles whose ids are bare GUIDs, which is why this
    /// is optional and falls back to the id.
    #[serde(default)]
    title: Option<String>,
}

impl LibraryEntry {
    fn into_owned(self) -> Option<OwnedGame> {
        let product = self.product?;
        let id = product.id.trim().to_string();
        if id.is_empty() {
            return None;
        }
        let title = product
            .title
            .map(|title| title.trim().to_string())
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| id.clone());
        Some(OwnedGame::new(GameId::new(Store::Amazon, id), title))
    }
}

/// One line of `nile library list --installed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledListEntry {
    pub title: String,
    pub id: String,
}

/// Parses the text form of `nile library list --installed`.
///
/// nile prints one line per game, `Title ID: <product id> GENRES: [...]`, then a
/// `*** TOTAL n ***` summary. The listing is title-first, so the id is taken
/// from the **last** ` ID: ` separator: a title that contains one still splits
/// in the right place, and genres never do. Colors are already stripped by the
/// driver.
pub fn parse_installed_list(output: &str) -> Vec<InstalledListEntry> {
    output
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            // The full listing marks installed games; `--installed` already
            // filtered, so the marker is usually absent and must not confuse
            // the title.
            let line = line.strip_prefix("(INSTALLED) ").unwrap_or(line);
            let (title, rest) = line.rsplit_once(" ID: ")?;
            let id = rest.split_whitespace().next()?.trim();
            if id.is_empty() {
                return None;
            }
            let title = title.trim();
            Some(InstalledListEntry {
                title: if title.is_empty() {
                    id.to_string()
                } else {
                    title.to_string()
                },
                id: id.to_string(),
            })
        })
        .collect()
}

/// Builds the installed [`Game`]s from the CLI listing plus nile's state file.
fn installed_games(output: &str, state: &[InstalledState]) -> Vec<Game> {
    parse_installed_list(output)
        .into_iter()
        .map(|entry| {
            let mut game = Game::new(GameId::new(Store::Amazon, entry.id.clone()), entry.title);
            game.installed = true;
            if let Some(details) = state.iter().find(|details| details.id == entry.id) {
                if !details.path.as_os_str().is_empty() {
                    game.install_dir = Some(details.path.clone());
                }
                game.size_bytes = details.size_bytes;
                game.version = details.version.clone();
            }
            // nile only ships Windows builds, and whether a title needs Proton
            // (or refuses to run under it at all) is what the tag records.
            game.tags.push("platform:windows".into());
            game
        })
        .collect()
}

/// Parses `nile auth --status`.
pub fn parse_account(output: &str) -> Result<AmazonAccount, AmazonError> {
    let json = extract_json(output, LogFormat::Nile).ok_or(AmazonError::NoJson)?;
    Ok(serde_json::from_str(&json)?)
}

/// Parses `nile --version` (`1.2.0 Robert Speedwagon`).
///
/// nile prints the version and its release codename separated by a space, with
/// no label. Log lines are skipped so a noisy run still parses.
pub fn parse_version(output: &str) -> Option<String> {
    output
        .lines()
        .filter(|line| LogFormat::Nile.level(line).is_none())
        .flat_map(str::split_whitespace)
        .find(|token| token.starts_with(|ch: char| ch.is_ascii_digit()) && token.contains('.'))
        .map(str::to_string)
}

fn decode<T: DeserializeOwned>(output: &str) -> Result<Vec<T>, AmazonError> {
    let json = extract_json(output, LogFormat::Nile).ok_or(AmazonError::NoJson)?;
    Ok(serde_json::from_str(&json)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;

    use crate::TEST_ENV_LOCK;
    use crate::cli::{CliCommand, ProgressParser};

    const LIBRARY: &str = include_str!("../../../fixtures/nile/library-list.json");
    const INSTALLED_LIST: &str = include_str!("../../../fixtures/nile/library-installed.txt");
    const INSTALLED_STATE: &str = include_str!("../../../fixtures/nile/installed.json");
    const STATUS: &str = include_str!("../../../fixtures/nile/auth-status.json");
    const VERSION: &str = include_str!("../../../fixtures/nile/version.txt");

    fn percentages(line: &str) -> Option<f32> {
        let regex = Regex::new(PROGRESS_PATTERN).unwrap();
        let captures = regex.captures(line)?;
        captures.get(1)?.as_str().parse().ok()
    }

    #[test]
    fn the_preset_matches_the_documented_nile_invocations() {
        let spec = spec();
        assert_eq!(spec.program, "nile");
        assert!(matches!(spec.progress, ProgressParser::Percent(_)));
        assert_eq!(
            spec.argv(&CliCommand::ListOwned).unwrap(),
            ["library", "list", "--json"]
        );
        assert_eq!(
            spec.argv(&CliCommand::ListInstalled).unwrap(),
            ["library", "list", "--installed"]
        );
        assert_eq!(spec.argv(&CliCommand::Sync).unwrap(), ["library", "sync"]);
        assert_eq!(spec.argv(&CliCommand::Auth).unwrap(), ["auth", "--login"]);
        assert_eq!(
            spec.argv(&CliCommand::Logout).unwrap(),
            ["auth", "--logout"]
        );
        assert_eq!(
            spec.argv(&CliCommand::Status).unwrap(),
            ["auth", "--status"]
        );
        assert_eq!(spec.argv(&CliCommand::Version).unwrap(), ["--version"]);
        assert_eq!(
            spec.argv(&CliCommand::Verify {
                id: "amzn1.x".into(),
                install_dir: None,
            })
            .unwrap(),
            ["verify", "amzn1.x"]
        );
        assert_eq!(
            spec.argv(&CliCommand::Launch {
                id: "amzn1.x".into(),
                install_dir: None,
            })
            .unwrap(),
            ["launch", "amzn1.x"]
        );

        // `--path` is the exact install directory, so no title-derived folder
        // can move the game somewhere else.
        assert_eq!(
            spec.argv(&CliCommand::Install {
                id: "amzn1.x".into(),
                install_dir: PathBuf::from("/games/Amazon/Adios"),
            })
            .unwrap(),
            ["install", "amzn1.x", "--path", "/games/Amazon/Adios"]
        );
    }

    #[test]
    fn only_the_progress_line_counts_and_it_has_no_percent_sign() {
        assert_eq!(
            percentages(
                "INFO [PROGRESS]:\t = Progress: 12.34 1234/10000, Running for: 00:01:23, ETA: 00:00:09"
            ),
            Some(12.34)
        );
        assert_eq!(
            percentages("INFO [PROGRESS]:\t = Progress: 100.00 10/10, Running for: 00:00:09"),
            Some(100.0)
        );
        // The bar's sibling lines also carry numbers and a unit.
        assert_eq!(
            percentages("INFO [PROGRESS]:\t = Downloaded: 1234.00 MiB, Written: 1234.00 MiB"),
            None
        );
        assert_eq!(
            percentages("INFO [PROGRESS]:\t  + Download\t- 12.34 MiB/s"),
            None
        );
        // A path or a size that happens to contain 100 is not progress.
        assert_eq!(percentages("INFO [CLI]:\t Found: Adios (100.00 MiB)"), None);
    }

    #[test]
    fn owned_games_parse_around_log_noise_and_fall_back_to_the_id() {
        let output = format!("INFO [LIBRARY]:\t Synchronizing library\n{LIBRARY}");
        let games = parse_owned(&output).unwrap();
        let titles: Vec<&str> = games.iter().map(|game| game.title.as_str()).collect();
        assert_eq!(titles, ["Adios", "790f3b07-fc9c"]);
        assert!(
            games
                .iter()
                .all(|game| game.id.store == Store::Amazon && !game.title.is_empty())
        );
        assert_eq!(games[0].id.store_id, "amzn1.adg.product.ab888f57");
        // The entitlement id is not the game id; `product.id` is.
        assert!(
            !games
                .iter()
                .any(|game| game.id.store_id == "entitlement-ab88")
        );

        // A compact single-line document parses the same way.
        let compact = LIBRARY.lines().collect::<String>();
        assert_eq!(parse_owned(&compact).unwrap(), games);
    }

    #[test]
    fn installed_listing_splits_title_and_product_id() {
        let entries = parse_installed_list(INSTALLED_LIST);
        assert_eq!(entries.len(), 2, "the TOTAL line is not a game");
        assert_eq!(entries[0].title, "Adios");
        assert_eq!(entries[0].id, "amzn1.adg.product.ab888f57");
        // A title that itself contains " ID: " still splits on the real
        // separator, which is the last one.
        assert_eq!(entries[1].title, "My ID: The Game");
        assert_eq!(entries[1].id, "790f3b07-fc9c");

        // The full listing's installed marker is not part of the title.
        let marked = parse_installed_list("(INSTALLED) Adios ID: amzn1.adg.product.ab888f57");
        assert_eq!(marked[0].title, "Adios");
    }

    #[test]
    fn the_state_file_supplies_path_size_and_version() {
        let state: Vec<StateEntry> = serde_json::from_str(INSTALLED_STATE).unwrap();
        let state: Vec<InstalledState> = state.into_iter().map(StateEntry::into_state).collect();
        assert_eq!(state.len(), 2);
        assert_eq!(
            state[0].path.as_path(),
            Path::new("/home/player/Games/nile/Adios")
        );
        assert_eq!(state[0].version.as_deref(), Some("2"));
        assert_eq!(state[0].size_bytes, Some(12_345_678_901));
        // nile writes an empty path/version/size for a half-broken entry.
        assert_eq!(state[1].version, None);
        assert_eq!(state[1].size_bytes, None);

        let games = installed_games(INSTALLED_LIST, &state);
        assert_eq!(games.len(), 2);
        assert!(games.iter().all(|game| game.installed));
        assert_eq!(
            games[0].install_dir.as_deref(),
            Some(Path::new("/home/player/Games/nile/Adios"))
        );
        assert_eq!(games[0].size_bytes, Some(12_345_678_901));
        assert_eq!(games[0].version.as_deref(), Some("2"));
        assert_eq!(games[0].tags, ["platform:windows"]);
        // An installed game nile did not record still shows up, without details.
        assert_eq!(games[1].install_dir, None);
        assert_eq!(games[1].title, "My ID: The Game");
    }

    #[test]
    fn a_malformed_state_file_costs_the_details_but_not_the_rows() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(INSTALLED_FILE), "{ not json").unwrap();
        let error = installed_state(Some(dir.path())).unwrap_err();
        assert!(matches!(error, AmazonError::StateJson { .. }), "{error}");
        assert!(error.to_string().contains("not valid JSON"), "{error}");

        // The list still comes from the CLI; `installed()` falls back to it.
        let games = installed_games(INSTALLED_LIST, &[]);
        assert_eq!(games.len(), 2);
        assert_eq!(games[0].install_dir, None);

        // A missing file is not an error: nothing is installed yet.
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(installed_state(Some(empty.path())).unwrap(), []);
        assert_eq!(installed_state(None).unwrap(), []);
    }

    #[test]
    fn the_state_is_read_from_niles_config_dir() {
        let guard = TEST_ENV_LOCK.lock().unwrap();
        // SAFETY: only the tests under this lock touch these variables.
        unsafe { std::env::set_var("NILE_CONFIG_PATH", "/tmp/nile-base") };
        let overridden = config_dir();
        unsafe { std::env::remove_var("NILE_CONFIG_PATH") };
        unsafe { std::env::set_var("XDG_CONFIG_HOME", "/tmp/xdg-config") };
        let xdg = config_dir();
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
        drop(guard);

        assert_eq!(overridden, Some(PathBuf::from("/tmp/nile-base/nile")));
        assert_eq!(xdg, Some(PathBuf::from("/tmp/xdg-config/nile")));
    }

    #[test]
    fn a_missing_payload_and_malformed_json_are_distinct_errors() {
        let error =
            parse_owned("ERROR [LIBRARY]:\t There was an error getting versions\n").unwrap_err();
        assert!(matches!(error, AmazonError::NoJson), "{error}");
        assert!(error.to_string().contains("no JSON"));

        let error = parse_owned("[{\"product\": nope}]").unwrap_err();
        assert!(matches!(error, AmazonError::Json { .. }), "{error}");
    }

    #[test]
    fn account_and_version_are_read_from_their_own_text() {
        let account = parse_account(STATUS).unwrap();
        assert_eq!(account.username, "player");
        assert!(account.signed_in());

        let signed_out = parse_account("{\"Username\": \"<not logged in>\", \"LoggedIn\": false}");
        assert!(!signed_out.unwrap().signed_in());

        assert_eq!(parse_version(VERSION).as_deref(), Some("1.2.0"));
        assert_eq!(
            parse_version("INFO [CLI]:\t starting\n1.2.0 Robert Speedwagon\n").as_deref(),
            Some("1.2.0")
        );
        assert_eq!(parse_version("INFO [CLI]:\t nothing here"), None);
    }

    #[test]
    fn verify_and_launch_are_judged_by_niles_own_lines() {
        let ok = RunReport::new(
            "verify",
            outcome(true, 0, &["INFO [DOWNLOAD]:\t Game is up to date"]),
            LogFormat::Nile,
        );
        assert_eq!(
            verify_report(&ok),
            VerifyReport {
                verified: true,
                errors: vec![]
            }
        );

        let failed = RunReport::new(
            "verify",
            outcome(
                true,
                0,
                &[
                    "ERROR [DOWNLOAD]:\t Unable to load manifest",
                    "INFO [CLI]:\t Download complete",
                ],
            ),
            LogFormat::Nile,
        );
        let verdict = verify_report(&failed);
        assert!(!verdict.verified);
        assert_eq!(verdict.errors.len(), 1);

        // A launch that never happened exits 0 and has no marker.
        let refused = RunReport::new(
            "launch",
            outcome(true, 0, &["ERROR [CLI]:\t Game is not installed"]),
            LogFormat::Nile,
        );
        assert!(!launched(&refused));

        // A real launch logs the marker; the game's own errors afterward do not
        // change the verdict.
        let ran = RunReport::new(
            "launch",
            outcome(
                true,
                0,
                &[
                    "INFO [LAUNCHER]:\t Launching",
                    "ERROR [Engine]:\t something the game printed",
                    "All processes exited",
                ],
            ),
            LogFormat::Nile,
        );
        assert!(launched(&ran));
        assert!(!ran.succeeded(), "the error line stays recorded");
    }

    fn outcome(success: bool, code: u32, lines: &[&str]) -> crate::cli::CliOutcome {
        let mut events: Vec<ProgressEvent> = lines
            .iter()
            .map(|line| ProgressEvent::Line((*line).to_string()))
            .collect();
        events.push(ProgressEvent::Done);
        crate::cli::CliOutcome {
            success,
            code,
            cancelled: false,
            timed_out: None,
            log_path: None,
            events,
            raw_tail: String::new(),
        }
    }

    #[test]
    fn the_default_install_dir_is_one_safe_component() {
        let guard = TEST_ENV_LOCK.lock().unwrap();
        // SAFETY: only the tests under this lock touch XDG_DATA_HOME.
        unsafe { std::env::set_var("XDG_DATA_HOME", "/tmp/xdg-amazon") };
        let plain = install_dir("Adios");
        let hostile = install_dir("../../etc/passwd");
        let dots = install_dir("..");
        unsafe { std::env::remove_var("XDG_DATA_HOME") };
        drop(guard);

        assert_eq!(
            plain,
            Some(PathBuf::from(
                "/tmp/xdg-amazon/pitagames/games/amazon/Adios"
            ))
        );
        assert_eq!(
            hostile.as_deref(),
            Some(Path::new(
                "/tmp/xdg-amazon/pitagames/games/amazon/..-..-etc-passwd"
            ))
        );
        assert_eq!(
            dots.as_deref(),
            Some(Path::new(
                "/tmp/xdg-amazon/pitagames/games/amazon/amazon-game"
            ))
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
        let amazon = Amazon::new();
        let missing = amazon.status(Some(dir.path().to_str().unwrap()));
        assert!(!missing.is_ready());
        assert_eq!(missing.program(), "nile");
        assert_eq!(missing.path(), None);

        script(dir.path(), "nile", "#!/bin/sh\nexit 0\n");
        let ready = amazon.status(Some(dir.path().to_str().unwrap()));
        assert!(ready.is_ready());
        assert_eq!(ready.path(), Some(dir.path().join("nile").as_path()));
    }

    #[test]
    fn an_exit_zero_run_that_logged_an_error_is_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        script(
            dir.path(),
            "nile",
            "#!/bin/sh\n\
             echo 'INFO [LIBRARY]:\t Synchronizing library'\n\
             echo 'WARNING [CLI]:\t Nothing was downloaded'\n\
             echo 'ERROR [CLI]:\t Couldn'\\''t find what you are looking for'\n\
             exit 0\n",
        );
        let amazon = Amazon::new();
        let report = amazon
            .output(
                &CliCommand::Install {
                    id: "amzn1.x".into(),
                    install_dir: dir.path().join("Adios"),
                },
                &options(dir.path()),
            )
            .unwrap();

        assert!(report.outcome.success, "the fake CLI exited 0");
        assert!(!report.succeeded());
        assert_eq!(report.warnings.len(), 1);
        assert_eq!(report.errors.len(), 1);
        assert!(report.log_path().unwrap().exists());

        let error = amazon.sync(&options(dir.path())).unwrap_err();
        assert!(
            matches!(
                error,
                AmazonError::Failed {
                    command: "sync",
                    ..
                }
            ),
            "{error}"
        );
    }

    #[test]
    fn a_launch_that_never_started_is_not_a_success() {
        let dir = tempfile::tempdir().unwrap();
        script(
            dir.path(),
            "nile",
            "#!/bin/sh\n\
             echo 'INFO [LAUNCHER]:\t Searching for amzn1.x'\n\
             echo 'ERROR [CLI]:\t Game is not installed'\n\
             exit 0\n",
        );
        let amazon = Amazon::new();
        let report = amazon
            .output(
                &CliCommand::Launch {
                    id: "amzn1.x".into(),
                    install_dir: None,
                },
                &options(dir.path()),
            )
            .unwrap();

        assert!(report.exited_cleanly(), "nile exits 0 even here");
        assert!(!launched(&report));
    }

    #[test]
    fn a_clean_run_parses_its_library_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        script(
            dir.path(),
            "nile",
            &format!(
                "#!/bin/sh\n\
                 echo 'INFO [LIBRARY]:\t Synchronizing library'\n\
                 cat <<'JSON'\n{LIBRARY}\nJSON\n"
            ),
        );
        let amazon = Amazon::new();
        let games = amazon.owned(&options(dir.path())).unwrap();
        assert_eq!(games.len(), 2);
    }

    #[test]
    fn installed_reads_the_listing_and_niles_state() {
        let dir = tempfile::tempdir().unwrap();
        script(
            dir.path(),
            "nile",
            &format!(
                "#!/bin/sh\n\
                 cat <<'TXT'\n{INSTALLED_LIST}\nTXT\n"
            ),
        );
        // Point nile's config at a fixture tree the backend can read.
        let config = dir.path().join("config");
        fs::create_dir_all(config.join("nile")).unwrap();
        fs::write(config.join("nile").join(INSTALLED_FILE), INSTALLED_STATE).unwrap();

        let guard = TEST_ENV_LOCK.lock().unwrap();
        let previous = std::env::var_os("NILE_CONFIG_PATH");
        // SAFETY: the env is restored before the lock is released.
        unsafe { std::env::set_var("NILE_CONFIG_PATH", &config) };
        let games = Amazon::new().installed(&options(dir.path())).unwrap();
        unsafe {
            match previous {
                Some(value) => std::env::set_var("NILE_CONFIG_PATH", value),
                None => std::env::remove_var("NILE_CONFIG_PATH"),
            }
        }
        drop(guard);

        assert_eq!(games.len(), 2);
        assert_eq!(
            games[0].install_dir.as_deref(),
            Some(Path::new("/home/player/Games/nile/Adios"))
        );
        assert_eq!(games[0].size_bytes, Some(12_345_678_901));
    }
}
