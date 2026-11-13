//! GOG backend, driven by `lgogdownloader` (library) and `gogdl` (games).
//!
//! GOG has no public library API a launcher may call, so v1 wraps the two CLIs
//! the plan names. They are not interchangeable, and pretending otherwise would
//! produce a backend that lists games it cannot install or installs games it
//! cannot launch. This module therefore drives **two programs with two jobs**
//! (see `docs/m2-gog.md`):
//!
//! - [`PROGRAM`] = `lgogdownloader` — the native Arch downloader. It is the only
//!   one of the two that can **list the account's games with titles** (`--list
//!   json`), it is the one whose login is a **browser URL + pasted redirect**
//!   (`--browser-login`, the same code-paste shape as Epic/Amazon), and it
//!   reports login status offline (`--check-login-status`).
//! - [`GAME_PROGRAM`] = `gogdl` — the Heroic downloader. It is the only one that
//!   installs a **launchable game directory** (`download <id> --path <dir>`),
//!   repairs it (`repair`), and **launches** it (`launch <dir> <id>`).
//!
//! The plan said "prefer `lgogdownloader` for install/verify, fall back to
//! `gogdl`". Building it showed why that ordering does not hold: lgogdownloader
//! downloads *installer files* (`setup_*.exe`) that still need running under
//! Wine, and it has no launch command at all, so a game it fetched could not be
//! played until the M2 prefix item landed. `gogdl` extracts a game directory and
//! can start it, which is what a launcher needs. So the split is by capability,
//! not by preference, and each job names the program that actually does it.
//!
//! ```text
//!  Gog::new() ──spec_for(command)──► CliSpec ──► CliHandle (spawn / next_event / wait)
//!      │                                             │
//!      ├─ owned()    lgogdownloader --list json        └─ streams Line/Percent events
//!      ├─ account()  lgogdownloader --check-login-status
//!      └─ auth       lgogdownloader --browser-login (URL + pasted redirect)
//!      │
//!      ├─ install    gogdl download {id} --path {dir} --platform windows
//!      ├─ verify     gogdl repair   {id} --path {dir} --platform windows
//!      └─ launch     gogdl launch   {dir} {id} --platform windows
//! ```
//!
//! Three details of the real CLIs shape this module (see `docs/m2-gog.md`):
//!
//! - **`--list json` is an array of rich game details**, not a flat id/title
//!   pair: each entry carries `product_id`, `gamename`, `title`, and its DLCs
//!   nested under `dlcs`. [`parse_owned`] takes the top-level entries (the base
//!   games) and ignores the nested DLCs, so a pack is not a second library row.
//! - **The GOG id is the numeric `product_id`.** `gogdl` addresses a game by it
//!   everywhere, and lgogdownloader's `--galaxy-install` accepts it too. The
//!   `gamename` slug is only a fallback for the rare entry that has no
//!   `product_id`, and the display title.
//! - **`gogdl` exits non-zero on failure** (`exit(1)`), unlike legendary and
//!   nile which log an error and exit `0`, so the exit status *is* a verdict
//!   here; its `[NAME] INFO:` lines still parse with the bracketed layout.
//!
//! Everything here is headless: no Dioxus, no SQLite, no Steam. The GUI's store
//! status and library sync land on top of these functions.

use std::path::PathBuf;

use pitagames_core::paths;
use pitagames_core::{GameId, OwnedGame, Store};
use serde::Deserialize;

use crate::cli::{
    CliCommand, CliError, CliHandle, CliOptions, CliOutcome, CliSpec, ProgressEvent, ProgressParser,
};
use crate::report::{LogFormat, RunFailure, RunReport, extract_json};

pub use crate::cli::StoreStatus;

/// The library program: listing, login status and the code-paste login.
pub const PROGRAM: &str = "lgogdownloader";

/// The game program: install, repair and launch.
pub const GAME_PROGRAM: &str = "gogdl";

/// The platform `gogdl` is told to install. Every GOG title PitaGames targets in
/// v1 is the Windows build (run under Proton/Wine); a native Linux build is a
/// later refinement. See the module docs and `docs/m2-gog.md`.
pub const GAME_PLATFORM: &str = "windows";

/// The login-status line `--check-login-status` prints when a session is stored.
pub const LOGGED_IN_MARKER: &str = "Login status: Logged in";
/// The same line when there is no session (the command also exits `1`).
pub const LOGGED_OUT_MARKER: &str = "Login status: Not logged in";

fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

/// The `lgogdownloader` preset (library, login, version).
///
/// It lists games and checks the session; it has no install/verify/launch of its
/// own, so those templates stay empty and the dispatcher in [`Gog::spec_for`]
/// routes them to `gogdl`.
pub fn spec() -> CliSpec {
    spec_with_program(PROGRAM)
}

/// The library preset with another program name, for a wrapper or a test.
pub fn spec_with_program(program: impl Into<String>) -> CliSpec {
    CliSpec {
        program: program.into(),
        needs_tty: true,
        // lgogdownloader draws a block-character bar and prints human text, with
        // no percent figure anywhere, so every line stays a plain line.
        progress: ProgressParser::Plain,
        // The rich details array (product_id/gamename/title, DLCs nested).
        list_owned: args(&["--list", "json"]),
        // Neither CLI can list "what is on disk under *our* directory": gogdl
        // has no listing, and lgogdownloader's `--status` checks its own
        // configured directory. Installed state comes from the scan, not here.
        list_installed: Vec::new(),
        // The browser URL + pasted redirect, which `--browser-login` forces
        // (without it lgogdownloader posts an email/password form first).
        auth: args(&["--browser-login"]),
        // Neither CLI can forget the session; that would mean deleting the
        // cookie/token files, which is not a CLI operation. See docs.
        logout: Vec::new(),
        // `--list` refreshes from the account on every run, so there is no
        // separate cache to sync.
        sync: Vec::new(),
        install: Vec::new(),
        verify: Vec::new(),
        launch: Vec::new(),
        // Offline: reads the stored cookies, never the network.
        status: args(&["--check-login-status"]),
        version: args(&["--version"]),
    }
}

/// The `gogdl` preset (install, repair, launch).
///
/// `auth_file` is passed as `--auth-config-path` in front of each subcommand,
/// which is where argparse requires a main-parser option. It is the JSON file
/// `gogdl` writes its tokens to; [`auth_file`] is the default under PitaGames'
/// config directory, so the launcher never touches Heroic's own file.
pub fn game_spec() -> CliSpec {
    game_spec_with(GAME_PROGRAM, auth_file())
}

/// The game preset with another program name and auth file, for a test.
pub fn game_spec_with(program: impl Into<String>, auth_file: Option<PathBuf>) -> CliSpec {
    let auth = |command: &str| -> Vec<String> {
        match &auth_file {
            Some(path) => vec![
                "--auth-config-path".to_string(),
                path.to_string_lossy().into_owned(),
                command.to_string(),
            ],
            None => vec![command.to_string()],
        }
    };

    let mut install = auth("download");
    install.extend(args(&[
        "{id}",
        "--path",
        "{dir}",
        "--platform",
        GAME_PLATFORM,
    ]));
    let mut verify = auth("repair");
    verify.extend(args(&[
        "{id}",
        "--path",
        "{dir}",
        "--platform",
        GAME_PLATFORM,
    ]));
    // `gogdl launch` takes the install **path** first, then the id.
    let mut launch = auth("launch");
    launch.extend(args(&["{dir}", "{id}", "--platform", GAME_PLATFORM]));

    CliSpec {
        program: program.into(),
        needs_tty: true,
        // gogdl logs `[NAME] INFO: message` lines and prints no percent figure.
        progress: ProgressParser::Plain,
        list_owned: Vec::new(),
        list_installed: Vec::new(),
        auth: Vec::new(),
        logout: Vec::new(),
        sync: Vec::new(),
        install,
        verify,
        launch,
        status: Vec::new(),
        version: args(&["--version"]),
    }
}

/// Where `gogdl` stores its GOG tokens.
///
/// `$XDG_CONFIG_HOME/pitagames/gog/auth.json`, so the launcher owns its own
/// session file instead of Heroic's `gog_store/auth.json`. `gogdl` creates and
/// rewrites it; PitaGames only passes the path. `None` only when there is no
/// config directory.
pub fn auth_file() -> Option<PathBuf> {
    paths::config_dir().map(|dir| dir.join("gog").join("auth.json"))
}

/// Where a default GOG install goes: `games/gog/<game name>`.
///
/// The name is sanitized for the same reason as Epic's and Amazon's: it comes
/// from the CLI's JSON (or a command line) and becomes one path component.
/// `None` only when there is no data directory.
pub fn install_dir(game_name: &str) -> Option<PathBuf> {
    paths::games_dir().map(|dir| {
        dir.join("gog")
            .join(paths::safe_component(game_name, "gog-game"))
    })
}

/// The GOG backend: the library program plus the game program.
#[derive(Debug, Clone)]
pub struct Gog {
    library: CliSpec,
    game: CliSpec,
}

impl Default for Gog {
    fn default() -> Self {
        Self::new()
    }
}

impl Gog {
    pub fn new() -> Self {
        Self {
            library: spec(),
            game: game_spec(),
        }
    }

    /// The backend with another `gogdl`, keeping the default library program.
    ///
    /// This is the `with_program` every other backend exposes: for GOG the
    /// program that matters to install/verify/launch is `gogdl`. Use
    /// [`Gog::with_programs`] when a test needs to replace both.
    pub fn with_program(program: impl Into<String>) -> Self {
        let mut gog = Self::new();
        gog.game = game_spec_with(program, auth_file());
        gog
    }

    /// The backend with both programs replaced.
    pub fn with_programs(library: impl Into<String>, game: impl Into<String>) -> Self {
        Self::with_programs_and_auth(library, game, auth_file())
    }

    /// The backend with both programs and an explicit `gogdl` auth file.
    pub fn with_programs_and_auth(
        library: impl Into<String>,
        game: impl Into<String>,
        auth_file: Option<PathBuf>,
    ) -> Self {
        Self {
            library: spec_with_program(library),
            game: game_spec_with(game, auth_file),
        }
    }

    /// The library program (`lgogdownloader`).
    pub fn library_spec(&self) -> &CliSpec {
        &self.library
    }

    /// The game program (`gogdl`).
    pub fn game_spec(&self) -> &CliSpec {
        &self.game
    }

    /// The spec that serves `command`.
    ///
    /// Library jobs (listing, login status, version, auth) use `lgogdownloader`;
    /// everything else uses `gogdl`. Commands neither preset supports resolve to
    /// a spec whose template is empty, so [`CliSpec::argv`] reports
    /// [`CliError::Unsupported`] rather than running a bare program.
    pub fn spec_for(&self, command: &CliCommand) -> &CliSpec {
        match command {
            CliCommand::ListOwned | CliCommand::Auth | CliCommand::Status | CliCommand::Version => {
                &self.library
            }
            _ => &self.game,
        }
    }

    /// The program name that serves `command`.
    pub fn program_for(&self, command: &CliCommand) -> &str {
        &self.spec_for(command).program
    }

    /// Resolves the library program without running it.
    pub fn library_status(&self, path_override: Option<&str>) -> StoreStatus {
        StoreStatus::probe(&self.library.program, path_override)
    }

    /// Resolves the game program without running it.
    pub fn game_status(&self, path_override: Option<&str>) -> StoreStatus {
        StoreStatus::probe(&self.game.program, path_override)
    }

    /// Starts one operation on the PTY driver, with the spec the command needs.
    pub fn spawn(&self, command: &CliCommand, options: &CliOptions) -> Result<CliHandle, CliError> {
        CliHandle::spawn(self.spec_for(command), command, options)
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
        let format = if std::ptr::eq(self.spec_for(command), &self.library) {
            LogFormat::Legendary
        } else {
            LogFormat::Gogdl
        };
        Ok(RunReport::new(command.as_str(), handle.wait()?, format))
    }

    /// The account's owned games, from `lgogdownloader --list json`.
    pub fn owned(&self, options: &CliOptions) -> Result<Vec<OwnedGame>, GogError> {
        let report = self.output(&CliCommand::ListOwned, options)?;
        let report = report.require_success()?;
        parse_owned(&report.text)
    }

    /// The stored session, as `lgogdownloader --check-login-status` reports it.
    ///
    /// That command exits `1` when nobody is logged in, so the exit code is not
    /// an error here: the printed status line is the answer, and a run with no
    /// status line at all is the failure.
    pub fn account(&self, options: &CliOptions) -> Result<GogAccount, GogError> {
        let report = self.output(&CliCommand::Status, options)?;
        parse_account(&report.text).ok_or(GogError::NoStatus)
    }

    /// The library CLI's own version, for the "outdated" warning.
    pub fn version(&self, options: &CliOptions) -> Result<Option<String>, GogError> {
        let report = self.output(&CliCommand::Version, options)?;
        let report = report.require_success()?;
        Ok(parse_version(&report.text))
    }

    /// Wraps a finished run the caller drove itself in the backend's report.
    ///
    /// Interactive flows (auth) need `spawn` + `write_stdin` + `wait`, so they
    /// cannot go through [`Gog::stream`]; this gives them the same verdict.
    pub fn report(&self, command: &CliCommand, outcome: CliOutcome) -> RunReport {
        let format = if std::ptr::eq(self.spec_for(command), &self.library) {
            LogFormat::Legendary
        } else {
            LogFormat::Gogdl
        };
        RunReport::new(command.as_str(), outcome, format)
    }
}

/// The login summary `lgogdownloader --check-login-status` prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GogAccount {
    pub logged_in: bool,
}

impl GogAccount {
    pub fn signed_in(&self) -> bool {
        self.logged_in
    }
}

/// Failures of the GOG backend that are not a bare [`CliError`].
#[derive(Debug, thiserror::Error)]
pub enum GogError {
    #[error("lgogdownloader printed no JSON to parse")]
    NoJson,

    #[error("lgogdownloader's JSON could not be read: {source}")]
    Json {
        #[from]
        source: serde_json::Error,
    },

    #[error("lgogdownloader printed no login status")]
    NoStatus,

    #[error("{program} {command} failed: {message}")]
    Failed {
        program: String,
        command: &'static str,
        message: String,
    },

    #[error(transparent)]
    Cli(#[from] CliError),
}

impl From<RunFailure> for GogError {
    fn from(failure: RunFailure) -> Self {
        Self::Failed {
            program: PROGRAM.to_string(),
            command: failure.command,
            message: failure.message,
        }
    }
}

/// Parses `lgogdownloader --list json`.
///
/// The document is an array of `gameDetails::getDetailsAsJson()` objects. Only
/// the fields a library row needs are read; the DLCs nested under each game's
/// `dlcs` are skipped, so a DLC is not a second row with its parent's art.
pub fn parse_owned(output: &str) -> Result<Vec<OwnedGame>, GogError> {
    let json = extract_json(output, LogFormat::Legendary).ok_or(GogError::NoJson)?;
    let entries: Vec<LibraryEntry> = serde_json::from_str(&json)?;
    Ok(entries
        .into_iter()
        .filter_map(LibraryEntry::into_owned)
        .collect())
}

/// One top-level entry of `--list json`.
#[derive(Debug, Deserialize)]
struct LibraryEntry {
    /// The numeric GOG product id, serialized as a string by lgogdownloader; a
    /// number is accepted too, because the account payload sometimes carries one.
    #[serde(default, deserialize_with = "string_or_number")]
    product_id: String,
    /// The slug GOG uses in URLs and `--game` regexes (`the_witcher_3`).
    #[serde(default)]
    gamename: String,
    /// The display title; empty for some old entries.
    #[serde(default)]
    title: String,
}

impl LibraryEntry {
    fn into_owned(self) -> Option<OwnedGame> {
        let gamename = self.gamename.trim().to_string();
        let id = if self.product_id.trim().is_empty() {
            gamename.clone()
        } else {
            self.product_id.trim().to_string()
        };
        if id.is_empty() {
            return None;
        }
        let title = self.title.trim();
        let title = if title.is_empty() {
            if gamename.is_empty() {
                id.clone()
            } else {
                gamename
            }
        } else {
            title.to_string()
        };
        Some(OwnedGame::new(GameId::new(Store::Gog, id), title))
    }
}

/// Parses the login status `--check-login-status` prints, when it printed one.
pub fn parse_account(output: &str) -> Option<GogAccount> {
    if output.contains(LOGGED_IN_MARKER) {
        Some(GogAccount { logged_in: true })
    } else if output.contains(LOGGED_OUT_MARKER) {
        Some(GogAccount { logged_in: false })
    } else {
        None
    }
}

/// Parses `lgogdownloader --version`.
///
/// lgogdownloader prints its version string (`3.18`, or `LGOGDownloader 3.18`),
/// so the first version-shaped token wins.
pub fn parse_version(output: &str) -> Option<String> {
    output
        .lines()
        .flat_map(str::split_whitespace)
        .find(|token| token.starts_with(|ch: char| ch.is_ascii_digit()) && token.contains('.'))
        .map(|token| {
            token
                .trim_matches(|ch: char| !ch.is_ascii_digit() && ch != '.')
                .to_string()
        })
        .filter(|token| !token.is_empty())
}

/// Accepts a JSON string or number as a `String` (empty for `null`).
fn string_or_number<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize as _;
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(match value {
        serde_json::Value::String(text) => text,
        serde_json::Value::Number(number) => number.to_string(),
        _ => String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;

    use crate::TEST_ENV_LOCK;

    const LIST: &str = include_str!("../../../fixtures/lgogdownloader/list.json");
    const LOGIN_IN: &str = include_str!("../../../fixtures/lgogdownloader/login-status-in.txt");
    const LOGIN_OUT: &str = include_str!("../../../fixtures/lgogdownloader/login-status-out.txt");
    const VERSION: &str = include_str!("../../../fixtures/lgogdownloader/version.txt");

    #[test]
    fn the_library_preset_matches_the_lgogdownloader_invocations() {
        let spec = spec();
        assert_eq!(spec.program, "lgogdownloader");
        assert!(matches!(spec.progress, ProgressParser::Plain));
        assert_eq!(
            spec.argv(&CliCommand::ListOwned).unwrap(),
            ["--list", "json"]
        );
        assert_eq!(spec.argv(&CliCommand::Auth).unwrap(), ["--browser-login"]);
        assert_eq!(
            spec.argv(&CliCommand::Status).unwrap(),
            ["--check-login-status"]
        );
        assert_eq!(spec.argv(&CliCommand::Version).unwrap(), ["--version"]);
        // lgogdownloader has no install/verify/launch; those are gogdl's.
        assert!(matches!(
            spec.argv(&CliCommand::Sync).unwrap_err(),
            CliError::Unsupported { command: "sync" }
        ));
        assert!(matches!(
            spec.argv(&CliCommand::Logout).unwrap_err(),
            CliError::Unsupported { command: "logout" }
        ));
    }

    #[test]
    fn the_game_preset_matches_the_gogdl_invocations() {
        let spec = game_spec_with("gogdl", Some(PathBuf::from("/cfg/pitagames/gog/auth.json")));
        assert_eq!(spec.program, "gogdl");
        assert_eq!(spec.argv(&CliCommand::Version).unwrap(), ["--version"]);
        assert_eq!(
            spec.argv(&CliCommand::Install {
                id: "1207660623".into(),
                install_dir: PathBuf::from("/games/GOG/The Witcher 3"),
            })
            .unwrap(),
            [
                "--auth-config-path",
                "/cfg/pitagames/gog/auth.json",
                "download",
                "1207660623",
                "--path",
                "/games/GOG/The Witcher 3",
                "--platform",
                "windows",
            ]
        );
        assert_eq!(
            spec.argv(&CliCommand::Verify {
                id: "1207660623".into(),
                install_dir: Some(PathBuf::from("/games/GOG/The Witcher 3")),
            })
            .unwrap(),
            [
                "--auth-config-path",
                "/cfg/pitagames/gog/auth.json",
                "repair",
                "1207660623",
                "--path",
                "/games/GOG/The Witcher 3",
                "--platform",
                "windows",
            ]
        );
        // `launch` takes the path first, then the id.
        assert_eq!(
            spec.argv(&CliCommand::Launch {
                id: "1207660623".into(),
                install_dir: Some(PathBuf::from("/games/GOG/The Witcher 3")),
            })
            .unwrap(),
            [
                "--auth-config-path",
                "/cfg/pitagames/gog/auth.json",
                "launch",
                "/games/GOG/The Witcher 3",
                "1207660623",
                "--platform",
                "windows",
            ]
        );
        // Without an auth file the flag is simply absent, not empty.
        let bare = game_spec_with("gogdl", None);
        assert_eq!(
            bare.argv(&CliCommand::Install {
                id: "1".into(),
                install_dir: PathBuf::from("/g"),
            })
            .unwrap()[0],
            "download"
        );
    }

    #[test]
    fn verify_and_launch_without_a_directory_fail_before_running() {
        let spec = game_spec_with("gogdl", None);
        // gogdl needs `--path` for both; a missing directory must be a template
        // error, never a silent `--path` that points nowhere.
        assert!(matches!(
            spec.argv(&CliCommand::Verify {
                id: "1".into(),
                install_dir: None,
            })
            .unwrap_err(),
            CliError::Template { .. }
        ));
        assert!(matches!(
            spec.argv(&CliCommand::Launch {
                id: "1".into(),
                install_dir: None,
            })
            .unwrap_err(),
            CliError::Template { .. }
        ));
    }

    #[test]
    fn commands_are_routed_to_the_program_that_supports_them() {
        let gog = Gog::with_programs("lgogdownloader", "gogdl");
        assert_eq!(gog.program_for(&CliCommand::ListOwned), "lgogdownloader");
        assert_eq!(gog.program_for(&CliCommand::Auth), "lgogdownloader");
        assert_eq!(gog.program_for(&CliCommand::Status), "lgogdownloader");
        assert_eq!(gog.program_for(&CliCommand::Version), "lgogdownloader");
        assert_eq!(
            gog.program_for(&CliCommand::Install {
                id: "1".into(),
                install_dir: PathBuf::from("/g"),
            }),
            "gogdl"
        );
        assert_eq!(
            gog.program_for(&CliCommand::Verify {
                id: "1".into(),
                install_dir: None,
            }),
            "gogdl"
        );
        assert_eq!(
            gog.program_for(&CliCommand::Launch {
                id: "1".into(),
                install_dir: None,
            }),
            "gogdl"
        );
    }

    #[test]
    fn owned_games_parse_from_the_details_array_and_skip_nested_dlc() {
        let games = parse_owned(LIST).unwrap();
        let titles: Vec<&str> = games.iter().map(|game| game.title.as_str()).collect();
        assert_eq!(
            titles,
            ["The Witcher 3: Wild Hunt", "Hollow Knight", "Gwent"]
        );
        assert!(
            games
                .iter()
                .all(|game| game.id.store == Store::Gog && !game.title.is_empty())
        );
        assert_eq!(games[0].id.store_id, "1207660623");
        // A DLC nested under `dlcs` is not a second row.
        assert!(
            !games
                .iter()
                .any(|game| game.title.contains("Blood and Wine"))
        );
        // An entry with no product_id falls back to its gamename.
        assert_eq!(games[2].id.store_id, "gwent_the_card_game");

        // A compact single-line document parses the same way.
        let compact = LIST.lines().collect::<String>();
        assert_eq!(parse_owned(&compact).unwrap(), games);
    }

    #[test]
    fn a_missing_payload_and_malformed_json_are_distinct_errors() {
        let error = parse_owned("Failed to get login token\n").unwrap_err();
        assert!(matches!(error, GogError::NoJson), "{error}");
        assert!(error.to_string().contains("no JSON"));

        let error = parse_owned("[{\"product_id\": nope}]").unwrap_err();
        assert!(matches!(error, GogError::Json { .. }), "{error}");
    }

    #[test]
    fn the_login_status_line_is_the_verdict_not_the_exit_code() {
        assert_eq!(
            parse_account(LOGIN_IN).unwrap(),
            GogAccount { logged_in: true }
        );
        assert_eq!(
            parse_account(LOGIN_OUT).unwrap(),
            GogAccount { logged_in: false }
        );
        assert!(!parse_account(LOGIN_OUT).unwrap().signed_in());
        // "^--check-login-status" also prints noise; anything else is not a status.
        assert_eq!(parse_account("Getting product data 1 / 1"), None);
    }

    #[test]
    fn version_is_read_from_lgogdownloader_its_output() {
        assert_eq!(parse_version(VERSION).as_deref(), Some("3.18"));
        assert_eq!(
            parse_version("Saving config: /home/player/.config/lgogdownloader/config.cfg\n")
                .as_deref(),
            None
        );
    }

    #[test]
    fn the_default_install_dir_is_one_safe_component() {
        let guard = TEST_ENV_LOCK.lock().unwrap();
        // SAFETY: only the tests under this lock touch XDG_DATA_HOME.
        unsafe { std::env::set_var("XDG_DATA_HOME", "/tmp/xdg-gog") };
        let plain = install_dir("The Witcher 3");
        let hostile = install_dir("../../etc/passwd");
        let dots = install_dir("..");
        unsafe { std::env::remove_var("XDG_DATA_HOME") };
        drop(guard);

        assert_eq!(
            plain,
            Some(PathBuf::from(
                "/tmp/xdg-gog/pitagames/games/gog/The Witcher 3"
            ))
        );
        assert_eq!(
            hostile.as_deref(),
            Some(Path::new(
                "/tmp/xdg-gog/pitagames/games/gog/..-..-etc-passwd"
            ))
        );
        assert_eq!(
            dots.as_deref(),
            Some(Path::new("/tmp/xdg-gog/pitagames/games/gog/gog-game"))
        );
    }

    #[test]
    fn the_auth_file_lives_under_pitagames_own_config_dir() {
        let guard = TEST_ENV_LOCK.lock().unwrap();
        let previous = std::env::var_os("XDG_CONFIG_HOME");
        // SAFETY: the env is restored before the lock is released.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", "/tmp/xdg-gog-config") };
        let file = auth_file();
        unsafe {
            match previous {
                Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
        drop(guard);

        assert_eq!(
            file,
            Some(PathBuf::from("/tmp/xdg-gog-config/pitagames/gog/auth.json"))
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
        let gog = Gog::with_programs("lgogdownloader", "gogdl");

        let missing = gog.library_status(Some(dir.path().to_str().unwrap()));
        assert!(!missing.is_ready());
        assert_eq!(missing.program(), "lgogdownloader");
        assert!(
            !gog.game_status(Some(dir.path().to_str().unwrap()))
                .is_ready()
        );

        script(dir.path(), "lgogdownloader", "#!/bin/sh\nexit 0\n");
        script(dir.path(), "gogdl", "#!/bin/sh\nexit 0\n");
        assert!(
            gog.library_status(Some(dir.path().to_str().unwrap()))
                .is_ready()
        );
        assert!(
            gog.game_status(Some(dir.path().to_str().unwrap()))
                .is_ready()
        );
    }

    #[test]
    fn the_login_status_is_read_even_though_it_exits_one() {
        let dir = tempfile::tempdir().unwrap();
        script(
            dir.path(),
            "lgogdownloader",
            &format!(
                "#!/bin/sh\n\
                 echo '{}'\n\
                 exit 1\n",
                LOGGED_OUT_MARKER
            ),
        );
        let gog = Gog::with_programs("lgogdownloader", "gogdl");
        let account = gog.account(&options(dir.path())).unwrap();
        assert!(!account.signed_in());

        script(
            dir.path(),
            "lgogdownloader",
            &format!(
                "#!/bin/sh\n\
                 echo '{}'\n\
                 exit 0\n",
                LOGGED_IN_MARKER
            ),
        );
        assert!(gog.account(&options(dir.path())).unwrap().signed_in());
    }

    #[test]
    fn an_install_is_driven_through_gogdl_not_the_library_program() {
        let dir = tempfile::tempdir().unwrap();
        // The library program must never be invoked for an install; make it fail
        // the run if it is, and let only gogdl succeed.
        script(
            dir.path(),
            "lgogdownloader",
            "#!/bin/sh\necho 'wrong program'\nexit 9\n",
        );
        script(
            dir.path(),
            "gogdl",
            "#!/bin/sh\necho '[MANAGER] INFO: done'\nexit 0\n",
        );
        let gog = Gog::with_programs("lgogdownloader", "gogdl");
        let report = gog
            .output(
                &CliCommand::Install {
                    id: "1207660623".into(),
                    install_dir: dir.path().join("Witcher"),
                },
                &options(dir.path()),
            )
            .unwrap();
        assert!(report.succeeded(), "{:?}", report.failure_message());
        assert!(report.log_path().unwrap().exists());
    }

    #[test]
    fn a_clean_run_parses_its_library_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        script(
            dir.path(),
            "lgogdownloader",
            &format!(
                "#!/bin/sh\n\
                 cat <<'JSON'\n{LIST}\nJSON\n"
            ),
        );
        let gog = Gog::with_programs("lgogdownloader", "gogdl");
        let games = gog.owned(&options(dir.path())).unwrap();
        assert_eq!(games.len(), 3);
    }
}
